use core::{
    cmp::min,
    ffi::{c_int, c_long, c_uint, c_ulong},
    future::PollFn,
    marker::PhantomData,
    str::ParseBoolError,
    sync::atomic::{AtomicU16, Ordering},
};

use alloc::{boxed::Box, str, string::String, vec::Vec};

use crate::{
    cons::{self},
    heap::SyncUnsafeCell,
    p9,
    pipe::{self, Pipe},
    print, ptr2mut, ptr2ref, ptr2ref_op, rtc,
    sched::{Task, mycpu, sleep_if},
    spin::Lock,
    stuff::{as_slice, as_slice_mut, cstr_as_slice},
    timer,
    tty::{self, Termios, Winsize},
};

pub enum FileKind {
    None,
    Used,
    P9(&'static mut p9::File),
    Cons(&'static mut cons::File),
    Pipe(Box<Pipe>),
}

pub struct File {
    kind: FileKind,
    rc: AtomicU16,
    offt: u64,
    path: Option<String>,
}

pub struct Seek;
impl Seek {
    pub const SET: u64 = 0;
    pub const CUR: u64 = 1;
    pub const END: u64 = 2;
}

impl File {
    pub const fn zeroed() -> File {
        File {
            kind: FileKind::None,
            rc: AtomicU16::new(0),
            offt: 0,
            path: None,
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, ()> {
        if self.rc.load(Ordering::Acquire) == 0 {
            print!("READ NULL FILE....\n");
            return Err(());
        }
        match &mut self.kind {
            FileKind::P9(p9f) => {
                if let Ok(n) = p9f.read(buf, self.offt as usize) {
                    self.offt = self.offt.wrapping_add(n as u64);
                    Ok(n)
                } else {
                    Err(())
                }
            }
            FileKind::Cons(c) => c.read(buf),
            FileKind::Pipe(p) => p.read(buf),
            _ => {
                panic!("read: unhandled file kind.")
            }
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize, ()> {
        if self.rc.load(Ordering::Acquire) == 0 {
            return Err(());
        }
        match &mut self.kind {
            FileKind::P9(p9f) => {
                if let Ok(n) = p9f.write(buf, self.offt as usize) {
                    self.offt = self.offt.wrapping_add(n as u64);
                    Ok(n)
                } else {
                    Err(())
                }
            }
            FileKind::Cons(c) => c.write(buf),
            FileKind::Pipe(p) => p.write(buf),
            _ => {
                panic!("write: unhandled file kind.")
            }
        }
    }

    pub fn close(&mut self, r: bool, w: bool) -> Result<(), ()> {
        if self.rc.load(Ordering::Acquire) == 0 {
            return Ok(());
        }

        match &mut self.kind {
            FileKind::Pipe(p) => {
                assert!(r != w);
                p.close(r);
            }
            _ => {}
        };

        if let Ok(1) = self.rc.compare_exchange(
            1,
            0,
            Ordering::AcqRel, //
            Ordering::Relaxed,
        ) {
            match &mut self.kind {
                FileKind::P9(p9f) => {
                    return if let Ok(_) = p9f.close() {
                        print!(
                            "CLOSE: {} {:?} {}\n",
                            self.rc.load(Ordering::Acquire),
                            self.path,
                            p9f.fid
                        );
                        self.kind = FileKind::None;
                        self.path = None;
                        Ok(())
                    } else {
                        self.rc.fetch_add(1, Ordering::Release);
                        Err(())
                    };
                }
                FileKind::Cons(cons) => {}
                FileKind::Pipe(p) => {}
                _ => panic!("write: unhandled file kind."),
            }
        } else {
            self.rc.fetch_sub(1, Ordering::Release);
        }

        Ok(())
    }

    pub fn seek_to(&mut self, offt: usize) {
        self.offt = offt as u64;
    }

    pub fn seek_by(&mut self, offt: i32) {
        self.offt = if offt > 0 {
            self.offt.wrapping_add(offt as u64)
        } else {
            self.offt.wrapping_sub(offt as u64)
        };
    }

    pub fn get_size(&mut self) -> u64 {
        match &self.kind {
            FileKind::None => 0,
            FileKind::Used => 0,
            FileKind::P9(file) => file.get_size(),
            FileKind::Cons(file) => file.get_size(),
            FileKind::Pipe(_) => 0,
            _ => panic!("unhandled file kind."),
        }
    }

    pub fn lseek(&mut self, offt: i64, whence: u64) -> Result<u64, ()> {
        match whence {
            Seek::SET => self.seek_to(offt as usize),
            Seek::END => {
                let offt = self.get_size().wrapping_sub(offt as u64) as usize;
                self.seek_to(offt)
            }
            Seek::CUR => self.seek_by(offt as i32),
            _ => return Err(()),
        };

        Ok(self.offt)
    }

    pub fn dup(&mut self, r: bool, w: bool) -> Option<&'static mut Self> {
        self.rc.fetch_add(1, Ordering::Release);
        print!(
            "DUP: {:?} rc {}\n",
            self.path,
            self.rc.load(Ordering::Relaxed)
        );
        match &mut self.kind {
            FileKind::Pipe(p) => p.dup(r, w),
            _ => {}
        }
        unsafe { (self as *const Self as *mut Self).as_mut() }
    }

    pub fn read_all(&mut self, mut buf: &mut [u8]) -> Result<(), ()> {
        while buf.len() > 0 {
            let n = self.read(buf).map_err(|_| ())?;
            if n == 0 {
                break;
            }
            buf = &mut buf[n..];
        }
        Ok(())
    }

    pub fn write_all(&mut self, mut buf: &[u8]) -> Result<(), ()> {
        while buf.len() > 0 {
            let n = self.write(buf).map_err(|_| ())?;
            buf = &buf[n..];
        }
        Ok(())
    }

    pub fn fstat(&self, stat: &mut Stat) -> Result<(), ()> {
        match &self.kind {
            FileKind::P9(p9) => p9.stat(stat),
            FileKind::Cons(c) => c.stat(stat),
            FileKind::Pipe(p) => p.stat(stat),
            FileKind::None => panic!("fstat: none"),
            FileKind::Used => panic!("fstat: used"),
            _ => panic!("fstat: unhandled file kind."),
        }
    }

    pub fn getdents64(&mut self, buf: &mut [u8]) -> Result<usize, ()> {
        match &self.kind {
            FileKind::P9(p9) => {
                if let Ok((n, offt)) = p9.getdents64(buf, self.offt) {
                    self.offt = offt as u64;
                    Ok(n)
                } else {
                    Err(())
                }
            }
            _ => panic!("unhandled file kind."),
        }
    }

    pub fn send(&mut self, to: &mut File, n: usize) -> Result<usize, ()> {
        let vec = Vec::<u8>::with_capacity(4096);
        let buf = ptr2mut!(vec.as_ptr(), [u8; 4096]);

        let mut rem = n;
        while rem > 0 {
            let amt = min(rem, 4096);
            let r = self.read(&mut buf[0..amt]).map_err(|_| ())?;
            if r == 0 {
                break;
            }
            to.write_all(&buf[0..r]).map_err(|_| ())?;
            rem -= r;
        }

        Ok(n - rem)
    }

    pub fn readable(&self) -> bool {
        match &self.kind {
            FileKind::None => false,
            FileKind::Used => false,
            FileKind::P9(file) => true,
            FileKind::Cons(file) => file.readable(),
            _ => panic!("unhandled file kind."),
        }
    }

    pub fn writeable(&self) -> bool {
        match self.kind {
            FileKind::None => false,
            FileKind::Used => false,
            FileKind::P9(_) => true,
            FileKind::Cons(_) => true,
            _ => panic!("unhandled file kind."),
        }
    }

    pub fn is_ok(&self) -> bool {
        match self.kind {
            FileKind::None => false,
            FileKind::Used => false,
            FileKind::P9(_) => true,
            FileKind::Cons(_) => true,
            _ => panic!("unhandled file kind."),
        }
    }

    pub fn hanged_up(&self) -> bool {
        false
    }

    pub fn wait4readable(&self) {
        match &self.kind {
            FileKind::P9(file) => {}
            FileKind::Cons(file) => file.wait4readable(),
            x => panic!("unhandled file kind."),
        }
    }

    pub fn is_pipe(&self) -> bool {
        match &self.kind {
            FileKind::Pipe(_) => true,
            _ => false,
        }
    }

    // pub fn pipe_close(&mut self, reader: bool) -> Result<(), ()> {
    //     match &mut self.kind {
    //         FileKind::Pipe(p) => p.close(reader),
    //         _ => Err(()),
    //     };
    //     self.close()
    // }
}

const NFILES: usize = 128;

struct Fs {
    files: [File; NFILES],
}

pub fn open(path: &str, flags: u32, _: u32) -> Result<&'static mut File, ()> {
    if let Some((idx, file)) = alloc_file() {
        return if let Ok(p9file) = p9::open(path, flags) {
            print!("OPEN: path {} fid = {}\n", path, p9file.fid);
            file.kind = FileKind::P9(p9file);
            file.rc = AtomicU16::new(1);
            file.path = Some(String::from(path));
            file.offt = 0;
            Ok(file)
        } else {
            free_file(idx);
            Err(())
        };
    }

    Err(())
}

pub fn open_pipe(nonblock: bool) -> Result<&'static mut File, ()> {
    if let Some((idx, file)) = alloc_file() {
        file.kind = FileKind::Pipe(Box::new(Pipe::new(nonblock)));
        file.rc = AtomicU16::new(1);
        file.path = None;
        file.offt = 0;
        Ok(file)
    } else {
        Err(())
    }
}

pub fn open_cons() -> Result<&'static mut File, ()> {
    if let Some((_, file)) = alloc_file() {
        file.kind = FileKind::Cons(cons::open());
        file.rc = AtomicU16::new(1);
        Ok(file)
    } else {
        Err(())
    }
}

pub fn sys_write() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let fd = tf.regs[0] as usize;

    if fd >= task.files.len() {
        return !0;
    }

    if task.files[fd].is_none() {
        return !0;
    }

    let len = tf.regs[2] as usize;
    let ptr = tf.regs[1];

    let file = task.files[fd].as_mut().unwrap();

    if ptr == 0 {
        return !0;
    }
    // i trust you user
    let buf = as_slice(ptr as *const u8, len);
    if let Ok(n) = file.file.write(buf) {
        n as u64
    } else {
        !0
    }
}

struct IOvec {
    ptr: *mut u8,
    len: usize,
}

pub fn sys_writev() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let fd = tf.regs[0] as usize;
    if fd >= task.files.len() {
        return !0;
    }

    if task.files[fd].is_none() {
        return !0;
    }

    let iovec_len = tf.regs[2] as usize;
    let ptr = tf.regs[1];

    let file = task.files[fd].as_mut().unwrap();

    if ptr == 0 {
        return !0;
    }

    let iovec_buf = as_slice(ptr as *const IOvec, iovec_len);

    let mut written = 0;
    for i in 0..iovec_len {
        let iovec = &iovec_buf[i];
        let buf = as_slice(iovec.ptr, iovec.len);
        if let Ok(n) = file.file.write(buf) {
            written += n as u64
        } else {
            return !0;
        }
    }

    written
}

pub fn getcwd() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let cwd = task.cwd.as_ref().unwrap();
    let len = min(tf.regs[1] as usize, cwd.as_bytes().len());
    let buf = as_slice_mut(tf.regs[0] as *mut u8, len + 1);
    print!("CWD = {}\n", cwd);
    buf[0..cwd.as_bytes().len()].copy_from_slice(&cwd.as_bytes());
    if tf.regs[1] as usize > cwd.len() {
        buf[cwd.len()] = 0;
    } else {
        return -78i64 as u64;
    }
    0
}

pub fn chdir() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let path = cstr_as_slice(tf.regs[0] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    if exists(&path_str) {
        task.cwd = Some(path_str);
        0
    } else {
        -2i64 as u64
    }
}

pub fn umask() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let res = task.umask;
    task.umask = tf.regs[0] as u32;
    res as u64
}

pub fn getdents64() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let fd = tf.regs[0] as usize;
    if fd >= task.files.len() {
        return !0;
    }

    if task.files[fd].is_none() {
        return !0;
    }

    let len = tf.regs[2] as usize;
    let ptr = tf.regs[1];

    let file = task.files[fd].as_mut().unwrap();

    if ptr == 0 {
        return !0;
    }

    let buf = as_slice_mut(ptr as *mut u8, len);
    if let Ok(n) = file.file.getdents64(buf) {
        n as u64
    } else {
        !0
    }
}

pub fn sys_read() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let fd = tf.regs[0] as usize;

    if fd >= task.files.len() {
        return !0;
    }

    if task.files[fd].is_none() {
        return !0;
    }

    let len = tf.regs[2] as usize;
    let ptr = tf.regs[1];

    let file = task.files[fd].as_mut().unwrap();

    if ptr == 0 {
        return !0;
    }

    print!("READ FD: {}\n", fd);
    // i trust you user
    let buf = as_slice_mut(ptr as *mut u8, len);
    if let Ok(n) = file.file.read(buf) {
        n as u64
    } else {
        !0
    }
}

pub fn readlinkat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0];

    let path = cstr_as_slice(tf.regs[1] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    let buf = as_slice_mut(tf.regs[2] as *mut u8, tf.regs[3] as usize);

    let real_path = if let Ok(path) = at_path(fd, path_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    print!("READ LINK AT: {}\n", real_path);

    if let Ok(n) = readlink(&real_path, buf) {
        n as u64
    } else {
        -2i64 as u64
    }
}

pub fn symlink(linkname: &str, path: &str) -> Result<(), ()> {
    p9::symlink(linkname, path)
}

fn cpystr(buf: &mut [u8], s: &str) -> usize {
    let slice = s.as_bytes();
    let n = min(buf.len(), slice.len());
    buf[0..n].copy_from_slice(slice);
    n
}

pub fn readlink(path: &str, buf: &mut [u8]) -> Result<usize, ()> {
    if path == "/proc/self/fd/0" {
        return Ok(cpystr(buf, "/dev/tty"));
    }

    if let Ok(str) = p9::readlink(path) {
        Ok(cpystr(buf, &str))
    } else {
        Err(())
    }
}

pub fn symlinkat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let oldname = cstr_as_slice(tf.regs[0] as *const u8);
    let oldname_str = String::from(str::from_utf8(oldname).unwrap());

    let fd = tf.regs[1];

    let path = cstr_as_slice(tf.regs[2] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    let real_path = if let Ok(path) = at_path(fd, path_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    print!("SYM LINK AT: old {} path {}\n", oldname_str, real_path);

    if let Ok(_) = symlink(&oldname_str, &real_path) {
        0
    } else {
        !0
    }
}

pub fn rename(from: &str, to: &str) -> Result<(), ()> {
    p9::rename(from, to)
}

pub fn renameat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let oldfd = tf.regs[0];
    let newfd = tf.regs[2];

    let oldpath = cstr_as_slice(tf.regs[1] as *const u8);
    let oldpath_str = String::from(str::from_utf8(oldpath).unwrap());

    let newpath = cstr_as_slice(tf.regs[3] as *const u8);
    let newpath_str = String::from(str::from_utf8(newpath).unwrap());

    let real_oldpath = if let Ok(path) = at_path(oldfd, oldpath_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    let real_newpath = if let Ok(path) = at_path(newfd, newpath_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    print!("RENAMEAT: old {} new {}\n", real_oldpath, real_newpath);

    if rename(&real_oldpath, &real_newpath).is_ok() {
        0
    } else {
        -2i64 as u64
    }
}

pub fn link(from: &str, to: &str, follow: bool) -> Result<(), ()> {
    p9::link(from, to, follow)
}

pub fn linkat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let oldfd = tf.regs[0];
    let newfd = tf.regs[2];

    let oldpath = cstr_as_slice(tf.regs[1] as *const u8);
    let oldpath_str = String::from(str::from_utf8(oldpath).unwrap());

    let newpath = cstr_as_slice(tf.regs[3] as *const u8);
    let newpath_str = String::from(str::from_utf8(newpath).unwrap());

    let real_oldpath = if let Ok(path) = at_path(oldfd, oldpath_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    let real_newpath = if let Ok(path) = at_path(newfd, newpath_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    print!("LINKAT: old {} new {}\n", real_oldpath, real_newpath);

    if link(
        &real_oldpath,
        &real_newpath,
        tf.regs[4] & SYMLINK_FOLLOW != 0,
    )
    .is_ok()
    {
        0
    } else {
        -2i64 as u64
    }
}

pub fn getrandom() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    tf.regs[1]
}

pub fn lseek() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0] as usize;
    print!(
        "LSEEK FD {} offt {} whence {}\n",
        fd, tf.regs[1] as i64, tf.regs[2]
    );

    if task.files[fd].is_none() {
        return -2i64 as u64;
    }

    let file = task.files[fd].as_mut().unwrap();

    if let Ok(offt) = file.file.lseek(tf.regs[1] as i64, tf.regs[2]) {
        offt
    } else {
        return -29i64 as u64;
    }
}

pub struct T;
impl T {
    pub const CGETS: u64 = 0x5401;
    pub const CSETS: u64 = 0x5402;
    pub const CSETSW: u64 = 0x5403;
    pub const CSETSF: u64 = 0x5404;
    pub const CGETA: u64 = 0x5405;
    pub const CSETA: u64 = 0x5406;
    pub const CSETAW: u64 = 0x5407;
    pub const CSETAF: u64 = 0x5408;
    pub const CSBRK: u64 = 0x5409;
    pub const CXONC: u64 = 0x540a;
    pub const CFLSH: u64 = 0x540b;
    pub const IOCEXCL: u64 = 0x540c;
    pub const IOCNXCL: u64 = 0x540d;
    pub const IOCSCTTY: u64 = 0x540e;
    pub const IOCGPGRP: u64 = 0x540f;
    pub const IOCSPGRP: u64 = 0x5410;
    pub const IOCOUTQ: u64 = 0x5411;
    pub const IOCSTI: u64 = 0x5412;
    pub const IOCGWINSZ: u64 = 0x5413;
    pub const IOCSWINSZ: u64 = 0x5414;
    pub const IOCMGET: u64 = 0x5415;
    pub const IOCMBIS: u64 = 0x5416;
    pub const IOCMBIC: u64 = 0x5417;
    pub const IOCMSET: u64 = 0x5418;
    pub const IOCGSOFTCAR: u64 = 0x5419;
    pub const IOCSSOFTCAR: u64 = 0x541a;
    pub const FIONREAD: u64 = 0x541b;
    pub const IOCINQ: u64 = T::FIONREAD;
    pub const IOCLINUX: u64 = 0x541c;
    pub const IOCCONS: u64 = 0x541d;
    pub const IOCGSERIAL: u64 = 0x541e;
    pub const IOCSSERIAL: u64 = 0x541f;
    pub const IOCPKT: u64 = 0x5420;
    pub const FIONBIO: u64 = 0x5421;
    pub const IOCNOTTY: u64 = 0x5422;
    pub const IOCSETD: u64 = 0x5423;
    pub const IOCGETD: u64 = 0x5424;
    pub const CSBRKP: u64 = 0x5425;
    pub const IOCSBRK: u64 = 0x5427;
    pub const IOCCBRK: u64 = 0x5428;
    pub const IOCGSID: u64 = 0x5429;
}

pub fn ioctl() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    print!("IOCTL {:x} 0x{:x}\n", tf.regs[0], tf.regs[1]);

    match (tf.regs[1]) {
        T::CGETS => tty::get_termios(tf.regs[2] as *mut Termios),
        T::CSETS => tty::set_termios(tf.regs[2] as *const Termios),
        T::IOCGWINSZ => tty::get_winsz(tf.regs[2] as *mut Winsize),
        T::IOCGPGRP => {
            unsafe { *(tf.regs[2] as *mut u32) = task.pid as u32 };
            0
        }
        T::IOCSPGRP => 0,
        x => panic!("unimplemented ioctl 0x{:x}", x),
    }
}

pub struct O;
impl O {
    pub const RDONLY: u32 = 0;
    pub const WRONLY: u32 = 1 << 0;
    pub const RDWR: u32 = 1 << 1;
    pub const CREAT: u32 = 1 << 6;
    pub const EXCL: u32 = 1 << 7;
    pub const NOCTTY: u32 = 1 << 8;
    pub const TRUNC: u32 = 1 << 9;
    pub const APPEND: u32 = 1 << 10;
    pub const NONBLOCK: u32 = 1 << 11;
    pub const DSYNC: u32 = 1 << 12;
    pub const ASYNC: u32 = 1 << 13;
    pub const DIRECTORY: u32 = 1 << 14;
    pub const NOFOLLOW: u32 = 1 << 15;
    pub const DIRECT: u32 = 1 << 16;
    pub const LARGEFILE: u32 = 1 << 17;
    pub const NOATIME: u32 = 1 << 18;
    pub const CLOEXEC: u32 = 1 << 19;
    pub const SYNC: u32 = 1 << 20;
    pub const PATH: u32 = 1 << 21;
    pub const TMPFILE: u32 = 1 << 22;
}

fn exists(path: &str) -> bool {
    p9::exists(path)
}

fn remove(path: &str) -> Result<(), ()> {
    p9::remove(path)
}

pub fn unlinkat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0];

    let path = cstr_as_slice(tf.regs[1] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    let real_path = if let Ok(path) = at_path(fd, path_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    if let Ok(_) = remove(&real_path) {
        0
    } else {
        -2i64 as u64
    }
}

pub fn mkdir(path: &str, mode: u32) -> Result<(), ()> {
    p9::mkdir(path, mode)
}

pub fn mkdirat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0];

    let path = cstr_as_slice(tf.regs[1] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    if let Ok(path) = at_path(fd, path_str, task) {
        print!("MKDIRAT {}\n", path);
        if mkdir(&path, tf.regs[2] as u32).is_ok() {
            0
        } else {
            !0
        }
    } else {
        -2i64 as u64
    }
}

fn at_path(fd: u64, path: String, task: &Task) -> Result<String, ()> {
    if path.starts_with("/") {
        return Ok(path);
    }

    let mut dir_path = if fd == AT_FDCWD as u64 {
        task.cwd.as_ref().unwrap().clone()
    } else {
        if let Some(dir) = task.get_file(fd as usize) {
            if let Some(p) = &dir.file.path {
                p.clone()
            } else {
                return Err(());
            }
        } else {
            return Err(());
        }
    };

    if !dir_path.ends_with("/") {
        dir_path.push('/');
    }

    dir_path.push_str(&path);
    Ok(dir_path)
}

pub fn utimensat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0];

    let path = cstr_as_slice(tf.regs[1] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    let real_path = if let Ok(path) = at_path(fd, path_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    if exists(&real_path) { 0 } else { -2i64 as u64 }
}

pub fn faccessat() -> u64 {
    utimensat()
}

pub fn openat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0];

    if tf.regs[1] == 0 {
        return !0;
    }

    let path = cstr_as_slice(tf.regs[1] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    let real_path = if let Ok(path) = at_path(fd, path_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    print!("OPEN: path {} by {}\n", real_path, task.pid);

    let mut idx = None;
    for i in 0..task.files.len() {
        if (task.files[i].is_none()) {
            idx = Some(i);
            break;
        }
    }

    if let Some(idx) = idx {
        if let Ok(f) = open(&real_path, tf.regs[2] as u32, tf.regs[3] as u32) {
            task.files[idx] = Some(crate::sched::FD {
                file: f,
                read: true,
                write: true,
            });
            return idx as u64;
        } else {
            print!("FAILED TO OPEN: {}\n", real_path);
        }
    }

    !0
}

pub fn fcntl() -> u64 {
    0
}

#[repr(C)]
pub struct Pollfd {
    fd: i32,
    events: i16,
    revents: i16,
}

pub struct POLL;
impl POLL {
    pub const IN: i16 = 0x001;
    pub const PRI: i16 = 0x002;
    pub const OUT: i16 = 0x004;
    pub const ERR: i16 = 0x008;
    pub const HUP: i16 = 0x010;
    pub const NVAL: i16 = 0x020;
    pub const RDNORM: i16 = 0x040;
    pub const RDBAND: i16 = 0x080;
}

// done with int disabled
fn check_events(pfds: &mut [Pollfd], task: &Task, timer_wait: bool) -> usize {
    let mut n_events = 0;
    for i in 0..pfds.len() {
        let pfd = &mut pfds[i];
        let events = pfd.events;
        let fd = pfd.fd;
        print!("CHECK EVENT fd: {} events: {}\n", fd, events);

        pfd.revents = 0;

        if let Some(file) = task.get_file(fd as usize) {
            if !file.file.is_ok() {
                pfd.revents |= POLL::ERR;
            }

            if file.file.hanged_up() {
                pfd.revents |= POLL::HUP;
            }
        } else {
            print!("POLL NO FILE: {}\n", fd);
            pfd.revents |= POLL::NVAL;
        }

        if pfd.revents != 0 {
            n_events += 1;
            continue;
        }

        match events {
            POLL::IN => {
                if let Some(file) = task.get_file(fd as usize) {
                    if file.file.readable() {
                        print!("POLLIN DETECTED.\n");
                        pfd.revents |= POLL::IN;
                        n_events += 1;
                    } else {
                        print!("ADDING POLLIN TO WQ\n");
                        file.file.wait4readable();
                        if timer_wait {
                            timer::add2wait();
                        }
                    }
                }
            }
            x => panic!("unhandled poll: {}\n", x),
        }
    }
    print!("CHECK EVENT RES = {}\n", n_events);
    n_events
}

pub fn ppoll() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    if tf.regs[1] == 0 {
        return 0;
    }

    let pfds = as_slice_mut(tf.regs[0] as *mut Pollfd, tf.regs[1] as usize);
    let ts = ptr2ref_op!(tf.regs[2], rtc::KernelTimespec);

    //TODO signal
    print!("POLL tmeout = {:?}\n", ts);
    let start = timer::read_tick();

    let mut timed_out = false;
    let mut n = 0;

    while !timed_out {
        n = 0;

        sleep_if(&mut || {
            n = check_events(pfds, task, ts.is_some());

            if let Some(ts) = ts {
                print!("WAITING FOR {}ms\n", ts.millis());
                if timer::read_tick() - start > ts.millis() {
                    timed_out = true
                }
            } else {
                print!("WAITING FOR EVER\n");
            }

            n == 0 && !timed_out
        });

        if n > 0 {
            break;
        }
    }

    print!("POLL WAKE n: {} timed_out: {} ts: {:?}\n", n, timed_out, ts);
    n as u64
}

pub fn close() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0] as usize;

    if fd > task.files.len() {
        return !0;
    }

    print!("CLOSE FD {}\n", fd);

    if task.files[fd].is_none() {
        return !0;
    }

    let file = task.files[fd].as_mut().unwrap();
    print!("CLOSING {:?} fd: {} BY {}\n", file.file.path, fd, task.pid);

    // if file.file.is_pipe() {
    //     file.file.pipe_close(file.read);
    //     task.files[fd] = None;
    //     return 0;
    // }

    if let Ok(_) = file.file.close(file.read, file.write) {
        task.files[fd] = None;
        0
    } else {
        !0
    }
}

pub fn dup3() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let old_fd = tf.regs[0] as usize;
    let new_fd = tf.regs[1] as usize;

    print!("DUP3 old {} new {}\n", old_fd, new_fd);

    if task.files[old_fd].is_none() {
        return !0;
    }

    if old_fd == new_fd {
        return old_fd as u64;
    }

    let mut replaced = task.get_file(new_fd);

    // let file = task.get_file(old_fd).unwrap();
    task.files[new_fd] = Some(task.dup_file(old_fd));

    if let Some(f) = &mut replaced {
        f.file.close(f.read, f.write).unwrap();
    }

    print!("DUP3 {} to {}\n", old_fd, new_fd);

    new_fd as u64
}

pub fn ftruncate() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0] as usize;

    if task.files[fd].is_none() {
        return !0;
    }

    let file = task.get_file(fd).unwrap().file;

    if file.path.is_none() {
        return !0;
    }

    if truncate(file.path.as_ref().unwrap(), tf.regs[1]).is_ok() {
        0
    } else {
        !0
    }
}

pub fn sendfile64() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let out_fd = tf.regs[0] as usize;
    let in_fd = tf.regs[1] as usize;
    let offt = tf.regs[2] as *mut u64;
    let cnt = tf.regs[3] as usize;

    if task.files[in_fd].is_none() {
        return !0;
    }

    if task.files[out_fd].is_none() {
        return !0;
    }

    print!("SENDFILE: {} {} {:?} {}\n", in_fd, out_fd, offt, cnt);

    let ifile = task.get_file(in_fd).unwrap().file;
    let ofile = task.get_file(out_fd).unwrap().file;

    if !offt.is_null() {
        ifile.seek_to(unsafe { offt.read() as usize })
    }

    if let Ok(n) = ifile.send(ofile, cnt) {
        if !offt.is_null() {
            unsafe { offt.write(ifile.offt) }
        }
        n as u64
    } else {
        !0
    }
}

pub const AT_SYMLINK_NOFOLLOW: u32 = 256;
pub const SYMLINK_FOLLOW: u64 = 0x400;

pub fn fstat(path: &str, stat: &mut Stat, follow: bool) -> Result<(), ()> {
    p9::stat(&path, stat, follow)
}

pub fn truncate(path: &str, size: u64) -> Result<(), ()> {
    p9::truncate(path, size)
}

pub fn newfsstatat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fd = tf.regs[0];

    let path = cstr_as_slice(tf.regs[1] as *const u8);
    let path_str = String::from(str::from_utf8(path).unwrap());

    let real_path = if let Ok(path) = at_path(fd, path_str, task) {
        path
    } else {
        return -2i64 as u64;
    };

    print!("NEWFSTAT: {}\n", real_path);

    let stat = unsafe { (tf.regs[2] as *mut Stat).as_mut() }.unwrap();
    if fstat(
        &real_path,
        stat,
        tf.regs[3] as u32 & AT_SYMLINK_NOFOLLOW == 0,
    )
    .is_ok()
    {
        return 0;
    }

    print!("NEWFSTAT FAIL: {}\n", real_path);
    return -2i64 as u64;
}

pub fn newfstat() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let fd = tf.regs[0] as usize;

    if task.files[fd].is_none() {
        return !0;
    }

    let file = task.files[fd].as_ref().unwrap();

    if let Ok(_) = file
        .file
        .fstat(unsafe { (tf.regs[1] as *mut Stat).as_mut() }.unwrap())
    {
        return 0;
    }

    !0
}
pub const AT_FDCWD: i32 = -100;

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct Stat {
    pub st_dev: c_ulong,
    pub st_ino: c_ulong,
    pub st_mode: c_uint,
    pub st_nlink: c_uint,
    pub st_uid: c_uint,
    pub st_gid: c_uint,
    pub st_rdev: c_ulong,
    pub __pad1: c_ulong,
    pub st_size: c_long,
    pub st_blksize: c_int,
    pub __pad2: c_int,
    pub st_blocks: c_long,
    pub st_atime: c_long,
    pub st_atime_nsec: c_ulong,
    pub st_mtime: c_long,
    pub st_mtime_nsec: c_ulong,
    pub st_ctime: c_long,
    pub st_ctime_nsec: c_ulong,
    pub __unused4: c_uint,
    pub __unused5: c_uint,
}

static FS: Lock<Fs> = Lock::new(
    "fs",
    Fs {
        files: [
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
            File::zeroed(),
        ],
    },
);

fn alloc_file() -> Option<(usize, &'static mut File)> {
    let lock = FS.acquire();
    let fs = lock.as_mut();

    for i in 0..fs.files.len() {
        let file = &mut fs.files[i];
        if let FileKind::None = file.kind {
            file.kind = FileKind::Used;
            let steal = unsafe { (file as *mut File).as_mut() }.unwrap();
            return Some((i, steal));
        }
    }

    None
}

fn free_file(idx: usize) {
    let lock = FS.acquire();
    let fs = lock.as_mut();
    fs.files[idx].kind = FileKind::None;
}
