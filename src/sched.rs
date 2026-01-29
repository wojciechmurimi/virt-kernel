use core::{
    arch::{asm, naked_asm},
    cmp::min,
    mem::forget,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{collections::linked_list::LinkedList, string::String, vec::Vec};

use crate::{
    arch::{
        pstate_i_clr, pstate_i_set, r_far_el1, r_pstate_daif, r_tpidr_el0, r_tpidrro_el0,
        tlbi_aside1, w_tpidr_el0, w_ttbr0_el1,
    },
    dsb,
    elf::{self, Elf, Elf64Phdr, PhIter},
    fs::{self, File},
    heap::SyncUnsafeCell,
    isb, p9,
    pm::{self, GB, MB, align_b, align_f},
    print, ptr2mut,
    spin::Lock,
    stuff::{as_slice, as_slice_mut, cstr_as_slice, cstr64_as_slice, defer},
    tlbi_vmalle1, trap,
    vm::{self, PmWrap, free_pt, map_v2p_4k_inner, unmap_4k_inner, v2p, v2p_pt},
    wfi,
};

pub struct Cpu {
    int_enable: bool,
    pub int_disables: u32,
    task_idx: Option<usize>,
    shed_ctx: [u64; 15],
}

unsafe impl Sync for Cpu {}

impl Cpu {
    pub fn disable_intr(&mut self) {
        if self.int_disables == 0 {
            self.int_enable = (r_pstate_daif() | 0b10) == 0;
            pstate_i_set();
        }
        self.int_disables += 1;
    }

    pub fn enable_intr(&mut self) {
        if self.int_disables == 1 && self.int_enable {
            pstate_i_clr();
        }
        self.int_disables -= 1;
    }

    pub fn get_task(&mut self) -> Option<&'static mut Task> {
        self.disable_intr();
        let task = match self.task_idx {
            Some(idx) => Some(&mut TASKS.as_mut()[idx]),
            _ => None,
        };
        self.enable_intr();
        task
    }
}

pub const NCPU: usize = 1;

static CPUS: SyncUnsafeCell<[Cpu; NCPU]> = SyncUnsafeCell::new([Cpu {
    int_enable: false,
    int_disables: 0,
    task_idx: None,
    shed_ctx: [0; 15],
}]);

pub fn cpuid() -> usize {
    0
}

pub fn mycpu() -> &'static mut Cpu {
    &mut CPUS.as_mut()[cpuid()]
}

static NTASKS: usize = 8;

enum State {
    Free,
    Used,
    Ready,
    Running,
    Sleeping,
    Zombie,
}

#[derive(Clone, Copy, Debug)]
enum RegionType {
    Program,
    Stack,
    Brk,
    Mmap,
}

const REGION_MAX_SZ: usize = GB;

#[derive(Clone, Copy, Debug)]
pub struct Region {
    ty: RegionType,
    pub vaddr: usize,
    cap: usize,
    pub len: usize,
    pub flags: u32,
    pub granule: u8,
}

impl Region {
    pub fn calc_granule(block_size: usize) -> u8 {
        assert!(block_size.count_ones() == 1);
        assert!(block_size >= 4096 && block_size <= MB);
        ((block_size / 4096) - 1) as u8
    }

    pub fn blksize(&self) -> usize {
        4096 << self.granule as usize
    }

    pub fn blksize2(granule: u8) -> usize {
        4096 << granule as usize
    }

    pub fn has(&self, vaddr: usize) -> bool {
        vaddr >= self.vaddr && vaddr < (self.vaddr + self.len)
    }

    pub fn end(&self) -> usize {
        self.len + self.vaddr
    }

    pub fn alloc(&mut self, sz: usize) -> Option<usize> {
        assert!(sz % self.blksize() == 0);
        if self.len + sz > self.vaddr + REGION_MAX_SZ {
            None
        } else {
            let res = self.vaddr + self.len;
            self.len += sz;
            Some(res)
        }
    }
}

pub type RTree = LinkedList<Region>;

pub struct FD {
    pub file: &'static mut fs::File,
    pub read: bool,
    pub write: bool,
}

pub struct Task {
    parent: Option<*mut Task>,
    exit_code: u64,
    sp_el1: usize,
    tid: Option<u64>,
    state: State,
    ctx: [u64; 15],
    lock: Lock<()>,
    pub trapframe: u64,
    user_pt: Option<u64>,
    user_sp: Option<u64>,
    chan: Option<u64>,
    pub pid: u16,
    // pub files: [Option<&'static mut fs::File>; 8],
    pub files: [Option<FD>; 8],
    program: RTree,
    mmap: Region,
    brk: Region,
    spel0: Region,
    pub umask: u32,
    pub cwd: Option<String>,
}

unsafe impl Sync for Task {}

impl Task {
    const STACKSIZE: usize = 4096 * 2;

    const fn zeroed() -> Task {
        Task {
            parent: None,
            exit_code: 0,
            sp_el1: 0,
            tid: None,
            state: State::Free,
            ctx: [0; 15],
            lock: Lock::new("T", ()),
            trapframe: 0,
            user_pt: None,
            user_sp: None,
            chan: None,
            pid: 0,
            files: [None, None, None, None, None, None, None, None],
            program: RTree::new(),
            mmap: Region {
                ty: RegionType::Mmap,
                vaddr: GB,
                cap: REGION_MAX_SZ,
                len: 0,
                flags: elf::PF_R | elf::PF_W,
                granule: 0,
            },
            brk: Region {
                ty: RegionType::Brk,
                vaddr: 2 * GB,
                cap: REGION_MAX_SZ,
                len: 0,
                flags: elf::PF_R | elf::PF_W,
                granule: 0,
            },
            spel0: Region {
                ty: RegionType::Stack,
                vaddr: 8 * GB,
                cap: SPEL0_SIZE,
                len: SPEL0_SIZE,
                flags: elf::PF_R | elf::PF_W,
                granule: 1,
            },
            umask: 0777,
            cwd: None,
        }
    }

    pub fn get_trap_frame(&self) -> Option<&'static mut trap::Frame> {
        unsafe { (self.trapframe as *mut trap::Frame).as_mut() }
    }

    pub fn get_file(&self, idx: usize) -> Option<FD> {
        if idx > self.files.len() {
            return None;
        }

        if self.files[idx].is_none() {
            return None;
        }

        let file = self.files[idx].as_ref().unwrap();
        return Some(FD {
            read: file.read,
            write: file.write,
            file: ptr2mut!((file.file) as *const File, File),
        });
    }

    pub fn alloc_file(&mut self, file: FD) -> Option<usize> {
        let lock = self.lock.acquire();

        for i in 0..self.files.len() {
            if self.files[i].is_none() {
                self.files[i] = Some(file);
                return Some(i);
            }
        }

        drop(lock);

        None
    }

    pub fn close_file(&mut self, idx: usize) {
        let lock = self.lock.acquire();

        if let Some(f) = &mut self.files[idx] {
            f.file.close(f.read, f.write);
            self.files[idx] = None;
        }

        drop(lock);
    }

    pub fn dup_file(&mut self, idx: usize) -> FD {
        if let Some(f) = &mut self.files[idx] {
            f.file.dup(f.read, f.write);
            return FD {
                file: ptr2mut!(f.file as *mut fs::File, fs::File),
                read: f.read,
                write: f.write,
            };
        } else {
            panic!("dup none.")
        }
    }

    fn init(&mut self) {
        let sp_el1 = pm::alloc(4096 * 2).unwrap();
        self.sp_el1 = vm::map(sp_el1, 2, vm::PR_PW).unwrap();
    }

    fn init_user(&mut self) {
        let user_pt = pm::alloc(4096).unwrap() as u64;
        self.user_pt = Some(user_pt);
        let l0_pt = PmWrap::new(self.user_pt.unwrap() as usize, vm::PR_PW, true).unwrap();

        let user_sp = pm::alloc(SPEL0_SIZE).unwrap();

        map(
            l0_pt.as_slice_mut(),
            self.spel0.vaddr,
            user_sp,
            self.spel0.len / 4096,
            vm::PR_PW_UR_UW1,
        ) //
        .unwrap();

        self.user_sp = Some(user_sp as u64);

        let sp_el1 = self.sp_el1 + Self::STACKSIZE;
        let tf_ptr = unsafe { (sp_el1 as *mut trap::Frame).sub(1) };
        let tf = unsafe { tf_ptr.as_mut().unwrap() };

        tf.pstate = 0x0;

        self.ctx[12] = tf_ptr as u64;
        self.ctx[13] = forkret as *const fn() as u64;

        for i in 0..self.files.len() {
            self.files[i] = None
        }

        self.trapframe = tf_ptr as u64;
    }
}

pub struct Wq {
    pub tasks: [Option<*mut Task>; NTASKS],
    pub count: usize,
    lock: Lock<()>,
}

impl Wq {
    pub const fn new(name: &'static str) -> Wq {
        Wq {
            tasks: [None; NTASKS],
            count: 0,
            lock: Lock::new(name, ()),
        }
    }

    pub fn add(&mut self, task: *mut Task) {
        if task.is_null() {
            return;
        }
        for i in 0..self.count {
            if self.tasks[i].unwrap() == task {
                return;
            }
        }
        let lock = self.lock.acquire();
        self.tasks[self.count] = Some(task);
        self.count += 1;
        drop(lock)
    }

    pub fn sleep<T>(&mut self, lock: &Lock<T>) {
        let task = mycpu().get_task().unwrap();
        self.add(task as *mut Task);
        let task_lock = task.lock.acquire();
        lock.release();
        task.state = State::Sleeping;
        task.chan = None;
        sched();
        forget(lock.acquire());
        drop(task_lock);
    }

    pub fn wake_all(&mut self) {
        let lock = self.lock.acquire();
        for i in 0..self.count {
            if let Some(task) = self.tasks[i] {
                let task = ptr2mut!(task, Task);
                let task_lock = task.lock.acquire();
                if let State::Sleeping = task.state {
                    task.state = State::Ready;
                }
                drop(task_lock);
            }
        }
        self.count = 0;
        drop(lock)
    }
}

fn map(l0_pt: &mut [u64], v: usize, p: usize, n: usize, perms: u64) -> Result<usize, vm::Error> {
    for i in 0..n {
        map_v2p_4k_inner(
            l0_pt,
            v + (4096 * i),
            p + (4096 * i),
            perms,
            false,
            |_pt| {},
        )
        .map_err(|e| e)?;
    }
    Ok(v)
}

fn unmap(l0_pt: &mut [u64], v: usize, n: usize) -> Result<(), vm::Error> {
    for i in 0..n {
        unmap_4k_inner(l0_pt, v + (4096 * i)).map_err(|e| e)?;
    }
    Ok(())
}

fn map_ovwr(
    l0_pt: &mut [u64],
    v: usize,
    p: usize,
    n: usize,
    perms: u64,
) -> Result<usize, vm::Error> {
    for i in 0..n {
        match map_v2p_4k_inner(
            l0_pt,
            v + (4096 * i),
            p + (4096 * i), //
            perms,
            true,
            |_| {},
        ) {
            Err(vm::Error::Exists(_)) => {}
            e => return e,
        };
    }
    Ok(v)
}

fn map_chg_perms(l0_pt: &mut [u64], v: usize, n: usize, perms: u64) -> Result<usize, vm::Error> {
    for i in 0..n {
        v2p_pt(
            l0_pt,
            v + (4096 * i),
            Some(|ptr: *mut u64| unsafe {
                *ptr = (*ptr & vm::PHY_MASK as u64) | perms | 0x403;
            }),
        )
        .unwrap();
    }
    Ok(v)
}

const SPEL0_SIZE: usize = 4096 * 2;

// inplace
pub fn execv_inner(path: &str, argv: &[&[u8]], envp: &[&[u8]], skipr: bool) -> Result<(), ()> {
    let mut elf = Elf::new(path).map_err(|_| ())?;

    let task = mycpu().get_task().unwrap();
    let user_pt = task.user_pt.unwrap();

    let l0_pt = PmWrap::new(user_pt as usize, vm::PR_PW, false).unwrap();
    free_region(&task.mmap, l0_pt.as_slice_mut(), false);
    task.mmap.len = 0;
    free_region(&task.brk, l0_pt.as_slice_mut(), false);
    task.brk.len = 0;
    free_regions(&mut task.program, l0_pt.as_slice_mut(), false).unwrap();
    task.program.clear();

    let file = unsafe { (elf.file as *mut File).as_mut() }.unwrap();
    let mut phit = PhIter::new(&mut elf);
    let mut ph = Elf64Phdr::zeroed();
    while let Some(p) = phit.next((&mut ph) as *mut Elf64Phdr) {
        if p.kind as u64 != elf::PT_LOAD {
            continue;
        }

        let len = align_f((p.vaddr as usize % 4096) + p.memsz as usize, 4096);
        let vfrom = align_b(p.vaddr as usize, 4096);
        let pages = len / 4096;
        for i in 0..pages {
            let pm = pm::alloc(4096).unwrap();
            map(
                l0_pt.as_slice_mut(),
                vfrom + i * 4096, //
                pm,
                1,
                vm::PR_PW,
            )
            .unwrap();
        }

        tlbi_aside1(task.pid as u64);
        dsb!();
        isb!();

        let slice = as_slice_mut(p.vaddr as *mut u8, p.memsz as usize);
        file.seek_to(p.offset as usize);
        file.read_all(&mut slice[0..p.filesz as usize]).unwrap();
        (&mut slice[p.filesz as usize..]).fill(0);
        task.program.push_back(Region {
            vaddr: vfrom,
            len,
            flags: p.flags,
            granule: 0,
            cap: len,
            ty: RegionType::Program,
        });

        map_chg_perms(
            l0_pt.as_slice_mut(),
            vfrom,
            pages,
            if p.flags == elf::PF_R | elf::PF_X {
                vm::PR_UR_UX
            } else if p.flags == elf::PF_R | elf::PF_W {
                vm::PR_PW_UR_UW1
            } else if p.flags == elf::PF_R {
                vm::PR_UR
            } else {
                panic!("unhandled flags combo")
            },
        )
        .unwrap();
    }

    let sp_el0 = as_slice_mut(task.spel0.vaddr as *mut u8, task.spel0.len);
    sp_el0.fill(0);

    let mut w_idx = SPEL0_SIZE;
    macro_rules! curptr {
        () => {
            &sp_el0[w_idx] as *const u8 as u64
        };
    }
    w_idx -= 16; //AT_RANDOM
    let at_random = curptr!();

    let mut s = Vec::new();
    s.push(0); // envp null term
    for i in 0..envp.len() {
        let slice = envp[envp.len() - i - 1];
        if slice.len() == 0 {
            break;
        }
        if slice.len() + 1 > w_idx {
            return Err(());
        }
        w_idx -= 1;
        sp_el0[w_idx] = 0;
        w_idx -= slice.len();
        sp_el0[w_idx..w_idx + slice.len()].copy_from_slice(slice);
        s.push(curptr!());
    }

    s.push(0); // argv null term

    for i in 0..argv.len() {
        let slice = argv[argv.len() - i - 1];
        if slice.len() == 0 {
            break;
        }
        if slice.len() + 1 > w_idx {
            return Err(());
        }
        w_idx -= 1;
        sp_el0[w_idx] = 0;
        w_idx -= slice.len();
        sp_el0[w_idx..w_idx + slice.len()].copy_from_slice(slice);
        s.push(curptr!());
    }

    #[repr(C)]
    struct Aux {
        k: u64,
        v: u64,
    }

    w_idx = align_b(w_idx, 8);
    let mut aux_ptr = (&mut sp_el0[w_idx]) as *mut u8 as *mut Aux;
    let mut aux_ref = unsafe { aux_ptr.as_mut() }.unwrap();
    let _ = aux_ref;

    macro_rules! auxv {
        ($k:expr, $v:expr) => {{
            if w_idx < 16 {
                return Err(());
            }
            unsafe {
                aux_ptr = aux_ptr.sub(1);
                aux_ref = aux_ptr.as_mut().unwrap();
                aux_ref.k = $k;
                aux_ref.v = $v;
            }
            w_idx -= 16;
        }};
    }

    auxv!(0, 0);
    auxv!(25, at_random as u64);

    let ptrs_len = 8 * (s.len() + 1);
    if w_idx < ptrs_len {
        return Err(());
    }
    w_idx -= ptrs_len;

    let ptrs = as_slice_mut(&mut sp_el0[w_idx] as *mut u8 as *mut usize, ptrs_len / 8);

    let sp_pos = curptr!();

    w_idx = 0;
    ptrs[w_idx] = argv.len();
    w_idx += 1;

    while let Some(ptr) = s.pop() {
        ptrs[w_idx] = ptr as usize;
        w_idx += 1;
    }

    let tf = unsafe { (task.trapframe as *mut trap::Frame).as_mut() }.unwrap();
    tf.zero();

    tf.pc = elf.header.entry;
    tf.pstate = 0x0;
    tf.sp_el0 = sp_pos as u64;

    restore_ttbr0(task.pid as usize, user_pt as usize);
    w_tpidr_el0(0);
    Ok(())
}

pub fn execve() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let path = cstr_as_slice(tf.regs[0] as *const u8);
    let path = Vec::from(path);

    let pathstr = match str::from_utf8(path.as_slice()) {
        Ok(p) => p,
        _ => return !0,
    };

    let argv_ptr = cstr64_as_slice(tf.regs[1] as *const u64);
    let envp_ptr = cstr64_as_slice(tf.regs[2] as *const u64);

    let mut argv = Vec::new();
    let mut envp = Vec::new();

    let mut all = Vec::new();

    for i in 0..argv_ptr.len() {
        let cstr = cstr_as_slice(argv_ptr[i] as *const u8);
        let vec = Vec::from(cstr);
        argv.push(as_slice(vec.as_ptr(), vec.len()));
        all.push(vec);
    }

    for i in 0..envp_ptr.len() {
        let cstr = cstr_as_slice(envp_ptr[i] as *const u8);
        let vec = Vec::from(cstr);
        envp.push(as_slice(vec.as_ptr(), vec.len()));
        all.push(vec);
    }

    let ret = match execv_inner(pathstr, &argv.as_slice(), &envp.as_slice(), true) {
        Ok(_) => 0,
        _ => !0,
    };

    ret
}

pub fn brk() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let pos = task.brk.end() as u64;

    let new_pos = align_f(tf.regs[0] as usize, 4096) as u64;

    if new_pos == 0 {
        return pos;
    }

    if new_pos < pos {
        return pos;
    }

    if new_pos == pos {
        return pos;
    }

    let incr = (new_pos - pos) as usize;
    if incr > 10 * MB {
        return pos;
    }

    let region = task.brk.alloc(incr).unwrap();

    let pages = incr / 4096;
    let l0_pt = PmWrap::new(task.user_pt.unwrap() as usize, vm::PR_PW, false).unwrap();

    for i in 0..pages {
        let v = region + i * 4096;
        let p = pm::alloc(4096).unwrap();
        match map(l0_pt.as_slice_mut(), v, p, 1, vm::PR_PW_UR_UW1) {
            Err(_) => {
                todo!();
            }
            _ => {}
        };
    }

    let slice = as_slice_mut(pos as *mut u8, incr);
    slice.fill(0x0);

    new_pos
}

pub fn settid() -> u64 {
    let task = mycpu().get_task().unwrap();
    task.pid as u64
}

pub fn set_robust_list() -> u64 {
    0
}

pub fn rseq() -> u64 {
    !0
}

pub fn prlimit64() -> u64 {
    0
}

pub fn mprotect() -> u64 {
    0
}

pub fn mmap() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let flags = tf.regs[3];

    // TODO
    if (flags & 0x20) == 0 {
        return !0;
    }

    let len = align_f(tf.regs[1] as usize, 4096);

    if len > 10 * MB {
        return !0;
    }

    let region = task.mmap.alloc(len).unwrap();

    let perms = if tf.regs[2] == 1 {
        vm::PR_UR
    } else if tf.regs[2] == 3 {
        vm::PR_PW_UR_UW1
    } else if tf.regs[2] == 5 {
        vm::PR_UR_UX
    } else {
        panic!("mmap: unknown perms: {}\n", tf.regs[2]);
    };

    let l0_pt = PmWrap::new(task.user_pt.unwrap() as usize, vm::PR_PW, false);
    if l0_pt.is_err() {
        return !0;
    }

    let l0_pt = l0_pt.unwrap();

    let pages = len / 4096;

    for i in 0..pages {
        let v = region + i * 4096;
        let p = pm::alloc(4096).unwrap();

        match map(l0_pt.as_slice_mut(), v, p, 1, perms) {
            Err(_) => {
                todo!()
            }
            _ => {}
        };
    }
    let slice = as_slice_mut(region as *mut u8, len);
    slice.fill(0x0);
    region as u64
}

pub fn munmap() -> u64 {
    0
}

fn clone_region(region: &Region, from_pt: &mut [u64], to_pt: &mut [u64]) {
    assert!(region.vaddr % 4096 == 0 && region.len % 4096 == 0);
    assert!(region.len % region.blksize() == 0);
    let flags = if region.flags == elf::PF_R | elf::PF_X {
        // 0
        vm::PR_UR_UX
    } else if region.flags == elf::PF_R | elf::PF_W {
        // 0
        vm::PR_UR
    } else if region.flags == elf::PF_R {
        // 0
        vm::PR_UR
    } else {
        panic!("unhandled flags combo")
    };

    let n = region.len / region.blksize();
    for i in 0..n {
        let closure = |ent: *mut u64| unsafe {
            *ent = (*ent & vm::PHY_MASK as u64) | flags | 0x403;
        };
        let vm = region.vaddr + (i * region.blksize());
        let pm = v2p_pt(from_pt, vm, Some(closure)).unwrap();
        crate::pm::dup(pm, region.blksize()).unwrap();
        let pages = region.blksize() / 4096;
        map(to_pt, vm, pm, pages, flags).unwrap();
        for j in 1..pages {
            v2p_pt(from_pt, vm + 4096 * j, Some(closure)).unwrap();
        }
    }
}

fn clone_regions(
    from: &RTree,
    to: &mut RTree, //
    from_pt: &mut [u64],
    to_pt: &mut [u64],
) -> Result<(), ()> {
    let mut fit = from.iter();
    while let Some(region) = fit.next() {
        clone_region(region, from_pt, to_pt);
        to.push_back(*region);
    }

    Ok(())
}

pub fn fork() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();
    let flags64 = tf.regs[0];
    let mut set_tid = false;
    let mut clear_tid = false;

    if flags64 & SIGCHLD as u64 != 0 {
        // todo!("flag SIGGCHLD is not implemented");
    }
    if flags64 & CLONE_VM as u64 != 0 {
        todo!("flag CLONE_VM is not implemented");
    }
    if flags64 & CLONE_FS as u64 != 0 {
        todo!("flag CLONE_FS is not implemented");
    }
    if flags64 & CLONE_FILES as u64 != 0 {
        todo!("flag CLONE_FILES is not implemented");
    }
    if flags64 & CLONE_SIGHAND as u64 != 0 {
        todo!("flag CLONE_SIGHAND is not implemented");
    }
    if flags64 & CLONE_PIDFD as u64 != 0 {
        todo!("flag CLONE_PIDFD is not implemented");
    }
    if flags64 & CLONE_PTRACE as u64 != 0 {
        todo!("flag CLONE_PTRACE is not implemented");
    }
    if flags64 & CLONE_VFORK as u64 != 0 {
        todo!("flag CLONE_VFORK is not implemented");
    }
    if flags64 & CLONE_PARENT as u64 != 0 {
        todo!("flag CLONE_PARENT is not implemented");
    }
    if flags64 & CLONE_THREAD as u64 != 0 {
        todo!("flag CLONE_THREAD is not implemented");
    }
    if flags64 & CLONE_NEWNS as u64 != 0 {
        todo!("flag CLONE_NEWNS is not implemented");
    }
    if flags64 & CLONE_SYSVSEM as u64 != 0 {
        todo!("flag CLONE_SYSVSEM is not implemented");
    }
    if flags64 & CLONE_SETTLS as u64 != 0 {
        todo!("flag CLONE_SETTLS is not implemented");
    }
    if flags64 & CLONE_PARENT_SETTID as u64 != 0 {
        todo!("flag CLONE_PARENT_SETTID is not implemented");
    }
    if flags64 & CLONE_CHILD_CLEARTID as u64 != 0 {
        clear_tid = true;
        // todo!("flag CLONE_CHILD_CLEARTID is not implemented");
    }
    if flags64 & CLONE_DETACHED as u64 != 0 {
        todo!("flag CLONE_DETACHED is not implemented");
    }
    if flags64 & CLONE_UNTRACED as u64 != 0 {
        todo!("flag CLONE_UNTRACED is not implemented");
    }
    if flags64 & CLONE_CHILD_SETTID as u64 != 0 {
        set_tid = true;
        // todo!("flag CLONE_CHILD_SETTID is not implemented");
    }
    if flags64 & CLONE_NEWCGROUP as u64 != 0 {
        todo!("flag CLONE_NEWCGROUP is not implemented");
    }
    if flags64 & CLONE_NEWUTS as u64 != 0 {
        todo!("flag CLONE_NEWUTS is not implemented");
    }
    if flags64 & CLONE_NEWIPC as u64 != 0 {
        todo!("flag CLONE_NEWIPC is not implemented");
    }
    if flags64 & CLONE_NEWUSER as u64 != 0 {
        todo!("flag CLONE_NEWUSER is not implemented");
    }
    if flags64 & CLONE_NEWPID as u64 != 0 {
        todo!("flag CLONE_NEWPID is not implemented");
    }
    if flags64 & CLONE_NEWNET as u64 != 0 {
        todo!("flag CLONE_NEWNET is not implemented");
    }
    if flags64 & CLONE_IO as u64 != 0 {
        todo!("flag CLONE_IO is not implemented");
    }
    if flags64 & CLONE_CLEAR_SIGHAND != 0 {
        todo!("flag CLONE_CLEAR_SIGHAND is not implemented");
    }
    if flags64 & CLONE_INTO_CGROUP != 0 {
        todo!("flag CLONE_INTO_CGROUP is not implemented");
    }
    if flags64 & CLONE_NEWTIME as u64 != 0 {
        todo!("flag CLONE_NEWTIME is not implemented");
    }

    // unsigned long clone_flags,
    // unsigned long newsp,
    // int *parent_tidptr,
    // unsigned long tls,
    // int *child_tidptr

    let child_tidptr = tf.regs[4] as *mut u32;

    if let Some(new_task) = alloc_user_task() {
        assert!(new_task.lock.holding());

        new_task.cwd = Some(task.cwd.as_ref().unwrap().clone());

        let from = PmWrap::new(task.user_pt.unwrap() as usize, vm::PR_PW, false).unwrap();
        let to = PmWrap::new(new_task.user_pt.unwrap() as usize, vm::PR_PW, false).unwrap();

        clone_regions(
            &task.program,
            &mut new_task.program, //
            from.as_slice_mut(),
            to.as_slice_mut(),
        )
        .unwrap();

        clone_region(&task.brk, from.as_slice_mut(), to.as_slice_mut());
        clone_region(&task.mmap, from.as_slice_mut(), to.as_slice_mut());

        new_task.brk = task.brk;
        new_task.mmap = task.mmap;

        copy_pm(
            task.user_sp.unwrap() as usize,
            new_task.user_sp.unwrap() as usize,
            2,
        )
        .unwrap();

        tlbi_aside1(task.pid as u64);
        dsb!();
        isb!();

        for i in 0..task.files.len() {
            if let Some(f) = &mut task.files[i] {
                new_task.files[i] = Some(task.dup_file(i));
            }
        }

        let nt = new_task.get_trap_frame().unwrap();
        *nt = *tf;

        nt.regs[0] = 0;

        new_task.ctx[14] = r_tpidr_el0();
        task.ctx[14] = r_tpidr_el0();

        new_task.state = State::Ready;

        let wlock = WAIT.acquire();
        new_task.parent = Some(task as *mut Task);
        drop(wlock);

        let pid = new_task.pid as u64;
        if set_tid {
            unsafe { *child_tidptr = new_task.pid as u32 }
        }

        if clear_tid {
            new_task.tid = Some(tf.regs[4]);
        }
        new_task.lock.release();
        pid
    } else {
        !0
    }
}

pub const SIGCHLD: u32 = 17;
pub const CLONE_VM: u32 = 256;
pub const CLONE_FS: u32 = 512;
pub const CLONE_FILES: u32 = 1024;
pub const CLONE_SIGHAND: u32 = 2048;
pub const CLONE_PIDFD: u32 = 4096;
pub const CLONE_PTRACE: u32 = 8192;
pub const CLONE_VFORK: u32 = 16384;
pub const CLONE_PARENT: u32 = 32768;
pub const CLONE_THREAD: u32 = 65536;
pub const CLONE_NEWNS: u32 = 131072;
pub const CLONE_SYSVSEM: u32 = 262144;
pub const CLONE_SETTLS: u32 = 524288;
pub const CLONE_PARENT_SETTID: u32 = 1048576;
pub const CLONE_CHILD_CLEARTID: u32 = 2097152;
pub const CLONE_DETACHED: u32 = 4194304;
pub const CLONE_UNTRACED: u32 = 8388608;
pub const CLONE_CHILD_SETTID: u32 = 16777216;
pub const CLONE_NEWCGROUP: u32 = 33554432;
pub const CLONE_NEWUTS: u32 = 67108864;
pub const CLONE_NEWIPC: u32 = 134217728;
pub const CLONE_NEWUSER: u32 = 268435456;
pub const CLONE_NEWPID: u32 = 536870912;
pub const CLONE_NEWNET: u32 = 1073741824;
pub const CLONE_IO: u32 = 2147483648;
pub const CLONE_CLEAR_SIGHAND: u64 = 4294967296;
pub const CLONE_INTO_CGROUP: u64 = 8589934592;
pub const CLONE_NEWTIME: u32 = 128;
pub const CLONE_ARGS_SIZE_VER0: u32 = 64;
pub const CLONE_ARGS_SIZE_VER1: u32 = 80;
pub const CLONE_ARGS_SIZE_VER2: u32 = 88;

fn free_region(region: &Region, l0_pt: &mut [u64], skip: bool) {
    let n = region.len / region.blksize();
    assert!(region.len % region.blksize() == 0);
    for i in 0..n {
        let v = region.vaddr + i * region.blksize();
        let p = v2p_pt::<fn(*mut u64)>(l0_pt, v, None).unwrap();
        if !skip {
            pm::free(p, region.blksize());
        }
        unmap(l0_pt, v, region.blksize() / 4096).unwrap();
    }
}

fn free_regions(regions: &mut RTree, l0_pt: &mut [u64], skip: bool) -> Result<(), vm::Error> {
    let mut rit = regions.iter();
    while let Some(region) = rit.next() {
        free_region(region, l0_pt, skip);
    }
    Ok(())
}

fn free_task(pid: usize) -> Result<(), vm::Error> {
    let task: &mut Task = &mut TASKS.as_mut()[pid];

    for i in 0..task.files.len() {
        if let Some(f) = &mut task.files[i] {
            print!("FREE FILE: {} dis: {}\n", i, mycpu().int_disables);
            let c = f.file.close(f.read, f.write);
            print!("FREE FILE: {} DONE dis: {}\n", i, mycpu().int_disables);
            if c.is_err() {}
        }
    }

    let l0_pt = PmWrap::new(
        task.user_pt.unwrap() as usize, //
        vm::PR_PW,
        false,
    )
    .unwrap();

    free_regions(&mut task.program, l0_pt.as_slice_mut(), false).unwrap();

    free_region(&task.spel0, l0_pt.as_slice_mut(), false);
    free_region(&task.brk, l0_pt.as_slice_mut(), false);
    free_region(&task.mmap, l0_pt.as_slice_mut(), false);

    task.user_sp = None;

    task.program.clear();
    task.brk.len = 0;
    task.mmap.len = 0;

    free_pt(task.user_pt.unwrap() as u64);

    let wait_lock = WAIT.acquire();

    //TODO reparent

    if let Some(p) = task.parent {
        wakeup(p as u64);
        print!("TASK FREED pid: {}\n", task.pid);
    }

    let lock = task.lock.acquire();
    task.state = State::Zombie;
    forget(lock);

    drop(wait_lock);

    Ok(())
}

static WAIT: Lock<()> = Lock::new("wait", ());

pub fn exit() -> u64 {
    let task = mycpu().get_task().unwrap();
    task.exit_code = task.get_trap_frame().unwrap().regs[0];

    if task.pid == 0 {
        panic!(
            "pid 0 tried to exit {}\n",
            task.get_trap_frame().unwrap().regs[0]
        );
    }

    free_task(task.pid as usize).unwrap();

    print!("EXIT pid: {} status {}\n", task.pid, task.exit_code);
    sched();
    0
}

pub fn exit_group() -> u64 {
    exit()
}

pub fn getuid() -> u64 {
    0
}

pub fn geteuid() -> u64 {
    0
}

pub fn setuid() -> u64 {
    0
}

pub fn getgid() -> u64 {
    0
}

pub fn setgid() -> u64 {
    0
}

pub fn gettid() -> u64 {
    //TODO
    getpid()
}

#[repr(C)]
pub struct OldUtsname {
    pub name: [u8; 45],
}

pub fn uname() -> u64 {
    let t = mycpu().get_task().unwrap();
    let tf = t.get_trap_frame().unwrap();
    let ptr = unsafe { (tf.regs[0] as *mut OldUtsname).as_mut() }.unwrap();

    (&mut ptr.name[0..2]).copy_from_slice("?\0".as_bytes());
    (&mut ptr.name[7..7 + 6]).copy_from_slice("local\0".as_bytes());
    (&mut ptr.name[7 + 6..7 + 6 + 6]).copy_from_slice("0.0.1\0".as_bytes());
    (&mut ptr.name[7 + 6 + 6..7 + 6 + 6 + 2]).copy_from_slice("0\0".as_bytes());
    (&mut ptr.name[7 + 6 + 6 + 2..7 + 6 + 6 + 2 + 8]).copy_from_slice("aarch64\0".as_bytes());
    0
}

pub fn wait() -> u64 {
    let t = mycpu().get_task().unwrap();
    let tf = t.get_trap_frame().unwrap();
    let ptr = t as *mut Task;
    let wait_lock = WAIT.acquire();
    let tasks = TASKS.as_mut();
    loop {
        let mut has_child = false;
        for i in 0..tasks.len() {
            let task: &mut Task = &mut tasks[i];
            let l = task.lock.acquire();
            if let Some(parent) = task.parent {
                if parent == ptr {
                    has_child = true;
                    if let State::Zombie = task.state {
                        if let Some(tid) = task.tid {
                            unsafe { *(tid as *mut u32) = 0 }
                        }
                        task.state = State::Free;
                        task.parent = None;
                        task.tid = None;
                        print!("sys_WAIT done\n");
                        return task.pid as u64;
                    }
                }
            }
            let _ = l;
        }

        if !has_child {
            return !0;
        }

        sleep(ptr as u64, wait_lock.get_lock());
    }
}

pub fn wait4() -> u64 {
    wait()
}

fn copy_pm(from_pm: usize, to_pm: usize, n: usize) -> Result<(), ()> {
    for i in 0..n {
        let to = PmWrap::new(to_pm + (4096 * i), vm::PR_PW, true).map_err(|_| ())?;
        let from = PmWrap::new(from_pm + (4096 * i), vm::PR, false).map_err(|_| ())?;
        to.as_slice_mut::<u8>().copy_from_slice(from.as_slice());
    }
    Ok(())
}

pub fn find_region(task: &mut Task, v: usize) -> Option<Region> {
    if task.brk.has(v) {
        return Some(task.brk);
    }
    if task.mmap.has(v) {
        return Some(task.brk);
    }
    if let Some(r) = task.program.iter().find(|r| r.has(v)) {
        return Some(*r);
    }
    None
}

pub fn dabt_handler() {
    let task = mycpu().get_task().unwrap();
    let vaddr = r_far_el1() as usize;

    if let Some(region) = find_region(task, vaddr) {
        if region.flags & elf::PF_W > 0 {
            let block = align_b(vaddr, region.blksize());

            let l0_pt = PmWrap::new(
                task.user_pt.unwrap() as usize, //
                vm::PR_PW,
                false,
            )
            .unwrap();

            let mut good = false;
            let _ = v2p_pt(
                l0_pt.as_slice_mut(),
                block,
                Some(|ptr: *mut u64| {
                    let pm_ = unsafe { *ptr as usize & vm::PHY_MASK };

                    pm::cow_action(pm_, region.blksize(), |a, al| {
                        let n = region.blksize() / 4096;
                        match a {
                            pm::CowAction::Remap => {
                                map_ovwr(
                                    l0_pt.as_slice_mut(), //
                                    block,
                                    pm_,
                                    n,
                                    vm::PR_PW_UR_UW1,
                                )
                                .unwrap();
                            }
                            pm::CowAction::Alloc => {
                                let new_pm = al.alloc(region.blksize()).unwrap();
                                copy_pm(pm_, new_pm, n).unwrap();
                                map_ovwr(
                                    l0_pt.as_slice_mut(), //
                                    block,
                                    new_pm,
                                    n,
                                    vm::PR_PW_UR_UW1,
                                )
                                .unwrap();
                            }
                        }
                        good = true;
                    });
                }),
            )
            .unwrap();

            if good {
                return;
            }
        }
    }

    //TODO segfaultonomy
    let tf = task.get_trap_frame().unwrap();
    let tls = r_tpidr_el0();
    let x = r_tpidrro_el0();
    panic!(
        "FATALITY: dabt.. address {:x} pid {} pc {:x} tls {:x} {}\n",
        vaddr, task.pid, tf.pc, tls, x
    );
}

pub fn sleep<T>(chan: u64, lock: &Lock<T>) {
    let task = mycpu().get_task().unwrap();
    let task_lock = task.lock.acquire();
    lock.release();
    task.state = State::Sleeping;
    task.chan = Some(chan);
    sched();
    task.chan = None;
    let old = lock.acquire();
    let _ = task_lock;
    forget(old);
}

pub fn sleep_if<F: FnMut() -> bool>(cond: &mut F) {
    let task = mycpu().get_task().unwrap();
    let task_lock = task.lock.acquire();
    if !cond() {
        return;
    }
    task.state = State::Sleeping;
    task.chan = None;
    sched();
    task.chan = None;
    drop(task_lock);
}

pub fn wakeup(chan: u64) {
    let tasks = TASKS.as_mut();
    for i in 0..tasks.len() {
        let task = &mut tasks[i];
        let lock = task.lock.acquire();
        if let State::Sleeping = task.state {
            if let Some(c) = task.chan {
                if c == chan {
                    task.state = State::Ready;
                }
            }
        }
        let _ = lock;
    }
}

pub fn scheduler() {
    let tasks = TASKS.as_mut();
    let cpu = mycpu();

    loop {
        pstate_i_clr();
        pstate_i_set();
        let mut found = false;
        for i in 0..tasks.len() {
            let task = &mut tasks[i];
            let lock = task.lock.acquire();
            match task.state {
                State::Ready => {
                    task.state = State::Running;
                    cpu.task_idx = Some(i);
                    switch(cpu.shed_ctx.as_mut_ptr(), task.ctx.as_ptr());
                    restore_ttbr0(task.pid as usize, task.user_pt.unwrap() as usize);
                    cpu.task_idx = None;
                    found = true;
                }
                _ => {}
            }
            let _ = lock;
        }

        if !found {
            wfi!();
        }
    }
}

pub fn sched() {
    let cpu = mycpu();
    let task = cpu.get_task().unwrap();
    assert!(cpu.int_disables == 1);
    assert!(task.lock.holding());
    if let State::Running = task.state {
        panic!("running");
    }
    // go back to sheduler()
    switch(task.ctx.as_mut_ptr(), cpu.shed_ctx.as_ptr());
    restore_ttbr0(task.pid as usize, task.user_pt.unwrap() as usize);
    mycpu().int_enable = cpu.int_enable;
}

pub fn yild() {
    let task = mycpu().get_task().unwrap();
    let lock = task.lock.acquire(); // re-acquire one released at fork ret
    task.state = State::Ready;
    sched();
    print!("RESUME {}\n", task.pid);
    drop(lock);
}

pub fn getpid() -> u64 {
    let task = mycpu().get_task().unwrap();
    task.pid as u64
}

pub fn getppid() -> u64 {
    let task = mycpu().get_task().unwrap();
    match task.parent {
        Some(p) => unsafe { p.as_mut() }.unwrap().pid as u64,
        _ => task.pid as u64,
    }
}

pub fn kill() -> u64 {
    0
}

pub fn tgkill() -> u64 {
    0
}

pub fn getpgid() -> u64 {
    0
}

pub fn setpgid() -> u64 {
    0
}

fn alloc_user_task() -> Option<&'static mut Task> {
    let tasks = TASKS.as_mut();
    for i in 0..tasks.len() {
        let task = &mut tasks[i];
        let lock = task.lock.acquire();
        if let State::Free = task.state {
            task.state = State::Used;
            task.pid = i as u16;
            forget(lock);
            task.init_user();
            return unsafe { (task as *const Task as *mut Task).as_mut() };
        }
        let _ = lock;
    }
    None
}

fn restore_ttbr0(task_idx: usize, pt: usize) {
    let ttbr0 = (task_idx << 48) | pt as usize;
    w_ttbr0_el1(ttbr0 as u64);
    dsb!();
    isb!();
    tlbi_aside1(task_idx as u64);
    tlbi_vmalle1!();
    dsb!();
    isb!();
}

pub fn create_user_task() {
    let task = alloc_user_task().unwrap();
    task.files[0] = Some(FD {
        file: fs::open_cons().unwrap(),
        read: true,
        write: true,
    });
    task.files[1] = Some(FD {
        file: fs::open_cons().unwrap(),
        read: true,
        write: true,
    });
    task.files[2] = Some(FD {
        file: fs::open_cons().unwrap(),
        read: true,
        write: true,
    });
    task.state = State::Ready;
    task.lock.release();
}

pub fn rt_sigaction() -> u64 {
    0
}

pub fn rt_sigprocmask() -> u64 {
    0
}

static FIRST: AtomicBool = AtomicBool::new(true);

const TEST_ENV: [&[u8]; 4] = [
    "PATH=/bin".as_bytes(),
    "PWD=/".as_bytes(),
    "LC_ALL=C".as_bytes(),
    "PS1=\\w \\$ ".as_bytes(),
];

#[unsafe(no_mangle)]
#[allow(unused)]
pub extern "C" fn forkret() {
    let cpu = mycpu();
    let task = cpu.get_task().unwrap();
    // was held in scheduler()
    task.lock.release();

    restore_ttbr0(task.pid as usize, task.user_pt.unwrap() as usize);

    if FIRST.swap(false, Ordering::Release) {
        print!("launching init..\n");
        execv_inner(
            "/bin/busybox",
            &["sh".as_bytes(), "-i".as_bytes()],
            &TEST_ENV,
            false,
        )
        .unwrap();
        task.cwd = Some("/".into());
    }

    unsafe {
        asm!(
            "mov sp, {}",
            "ldp x0, x1, [sp], #16",
            "msr elr_el1, x0",
            "msr sp_el0, x1",
            "ldp x1, x0, [sp], #16",
            "msr spsr_el1, x1",
            "ldp x1, x2, [sp], #16",
            "ldp x3, x4, [sp], #16",
            "ldp x5, x6, [sp], #16",
            "ldp x7, x8, [sp], #16",
            "ldp x9, x10, [sp], #16",
            "ldp x11, x12, [sp], #16",
            "ldp x13, x14, [sp], #16",
            "ldp x15, x16, [sp], #16",
            "ldp x17, x18, [sp], #16",
            "ldp x19, x20, [sp], #16",
            "ldp x21, x22, [sp], #16",
            "ldp x23, x24, [sp], #16",
            "ldp x25, x26, [sp], #16",
            "ldp x27, x28, [sp], #16",
            "ldp x29, x30, [sp], #16",
            "eret",
            in(reg) task.trapframe
        )
    }
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn switch(save: *mut u64, load: *const u64) {
    naked_asm!(
        "stp x19, x18, [x0], #16",
        "stp x21, x20, [x0], #16",
        "stp x23, x22, [x0], #16",
        "stp x25, x24, [x0], #16",
        "stp x27, x26, [x0], #16",
        "stp x29, x28, [x0], #16",
        "mov x2, sp",
        "stp x2, x30, [x0], #16",
        "mrs x2, tpidr_el0",
        "str x2, [x0], #8",
        //=========================
        "ldp x19, x18, [x1], #16",
        "ldp x21, x20, [x1], #16",
        "ldp x23, x22, [x1], #16",
        "ldp x25, x24, [x1], #16",
        "ldp x27, x26, [x1], #16",
        "ldp x29, x28, [x1], #16",
        "ldp x2, x30, [x1], #16",
        "mov sp, x2",
        "ldr x2, [x1], #8",
        "msr tpidr_el0, x2",
        "ret"
    )
}

static TASKS: SyncUnsafeCell<[Task; NTASKS]> = SyncUnsafeCell::new([
    Task::zeroed(),
    Task::zeroed(),
    Task::zeroed(),
    Task::zeroed(),
    Task::zeroed(),
    Task::zeroed(),
    Task::zeroed(),
    Task::zeroed(),
]);

pub fn init() {
    let tasks = TASKS.as_mut();
    for i in 0..tasks.len() {
        tasks[i].init();
    }
}
