use core::cmp::min;

use crate::{
    fs::{self, Stat},
    print, ptr2mut,
    sched::{self, mycpu},
    spin::Lock,
    stuff::{as_slice, as_slice_mut, print_slice_chars},
};

pub struct Pipe {
    readers: usize,
    writers: usize,
    rpos: usize,
    wpos: usize,
    empty: bool,
    full: bool,
    nonblock: bool,
    lock: Lock<()>,
    buf: [u8; 4096],
}

impl Pipe {
    pub const fn new(nonblock: bool) -> Pipe {
        Pipe {
            readers: 1,
            writers: 0,
            rpos: 0,
            wpos: 0,
            empty: true,
            full: false,
            nonblock,
            lock: Lock::new("pipe", ()),
            buf: [0u8; 4096],
        }
    }

    fn get_w_pos(&self) -> usize {
        self.wpos % self.buf.len()
    }

    fn get_r_pos(&self) -> usize {
        self.rpos % self.buf.len()
    }

    fn get_write_space(&mut self) -> (Option<&'static mut [u8]>, Option<&'static mut [u8]>) {
        if self.full {
            return (None, None);
        }

        let rpos = self.get_r_pos();
        let wpos = self.get_w_pos();

        if wpos >= rpos {
            let a = &mut self.buf[wpos..];
            let a = if a.len() > 0 {
                as_slice_mut(&mut a[0] as *const u8 as *mut u8, a.len())
            } else {
                &mut []
            };
            let b = &mut self.buf[0..rpos];
            let b = if b.len() > 0 {
                as_slice_mut(&mut b[0] as *const u8 as *mut u8, b.len())
            } else {
                &mut []
            };
            (Some(a), Some(b))
        } else {
            let a = &mut self.buf[wpos..rpos];
            let a = as_slice_mut(&mut a[0] as *const u8 as *mut u8, a.len());
            (Some(a), None)
        }
    }

    fn get_read_space(&mut self) -> (Option<&'static [u8]>, Option<&'static [u8]>) {
        if self.empty {
            return (None, None);
        }

        let rpos = self.get_r_pos();
        let wpos = self.get_w_pos();

        if rpos >= wpos {
            let a = &self.buf[rpos..];
            let a = if a.len() > 0 {
                as_slice(&a[0] as *const u8, a.len())
            } else {
                &[]
            };
            let b = &self.buf[0..wpos];
            let b = if b.len() > 0 {
                as_slice(&b[0] as *const u8, b.len())
            } else {
                &[]
            };
            (Some(a), Some(b))
        } else {
            let a = &self.buf[rpos..wpos];
            let a = as_slice(&a[0] as *const u8, a.len());
            (Some(a), None)
        }
    }

    fn write_inner(&mut self, mut buf: &[u8]) -> usize {
        let (a, b) = self.get_write_space();

        let wpoz = self.wpos;

        if let Some(a) = a {
            let min = core::cmp::min(buf.len(), a.len());
            a[0..min].copy_from_slice(&buf[0..min]);

            self.wpos = self.wpos.wrapping_add(min);
            buf = &buf[min..];

            if let Some(b) = b
                && buf.len() > 0
            {
                let min = core::cmp::min(buf.len(), b.len());
                b[0..min].copy_from_slice(&buf[0..min]);
                self.wpos = self.wpos.wrapping_add(min);
            }
        }

        let written = self.wpos.wrapping_sub(wpoz);

        if self.wpos == self.rpos {
            self.full = true;
        }

        if written > 0 {
            self.empty = false;
        }

        written
    }

    pub fn write(&mut self, mut buf: &[u8]) -> Result<usize, ()> {
        let lock = self.lock.acquire();
        let mut cnt = 0;
        'outer: loop {
            if self.readers == 0 {
                break;
            }

            let n = (ptr2mut!(self as *const Pipe, Pipe)).write_inner(buf);

            cnt += n;

            if n == buf.len() {
                break;
            }

            buf = &buf[n..];

            if n > 0 {
                sched::wakeup(&self.rpos as *const usize as u64);
            }

            while self.full {
                sched::sleep(&self.wpos as *const usize as u64, lock.get_lock());

                if self.readers == 0 {
                    break 'outer;
                }
            }
        }
        if self.readers != 0 {
            sched::wakeup(&self.rpos as *const usize as u64);
        }
        Ok(buf.len())
    }

    fn read_inner(&mut self, mut buf: &mut [u8]) -> usize {
        let (a, b) = self.get_read_space();

        let rpoz = self.rpos;

        if let Some(a) = a {
            let min = core::cmp::min(buf.len(), a.len());
            buf[0..min].copy_from_slice(&a[0..min]);

            self.rpos = self.rpos.wrapping_add(min);
            buf = &mut buf[min..];

            if let Some(b) = b
                && buf.len() > 0
            {
                let min = core::cmp::min(buf.len(), b.len());
                buf[0..min].copy_from_slice(&b[0..min]);
                self.rpos = self.rpos.wrapping_add(min);
            }
        }

        let rd = self.rpos.wrapping_sub(rpoz);

        if self.wpos == self.rpos {
            self.empty = true;
        }

        if rd > 0 {
            self.full = false;
        }

        rd
    }

    pub fn read(&mut self, mut buf: &mut [u8]) -> Result<usize, ()> {
        let lock = self.lock.acquire();
        let mut cnt = 0;
        'outer: loop {
            let n = (ptr2mut!(self as *const Pipe, Pipe)).read_inner(buf);

            cnt += n;

            print!("PIPE READ: {} {}\n", self.readers, self.writers);
            // print_slice_chars(&buf[0..n]);
            if self.writers == 0 {
                break;
            }

            if n == buf.len() {
                break;
            }

            buf = &mut buf[n..];

            if n > 0 {
                sched::wakeup(&self.wpos as *const usize as u64);
            }

            while self.empty {
                sched::sleep(&self.rpos as *const usize as u64, lock.get_lock());

                if self.writers == 0 {
                    break 'outer;
                }
            }
        }
        if self.writers != 0 {
            sched::wakeup(&self.wpos as *const usize as u64);
        }
        Ok(cnt)
    }

    pub fn stat(&self, stat: &mut Stat) -> Result<(), ()> {
        stat.st_ino = 0;
        stat.st_size = 0;
        stat.st_nlink = 1;
        stat.st_mode = 0o010000;
        Ok(())
    }

    pub fn close(&mut self, reader: bool) -> Result<(), ()> {
        let lock = self.lock.acquire();
        if reader {
            self.readers -= 1;
        } else {
            self.writers -= 1;
        }
        print!(
            "========> cLoSING PIPE: {} r: {} w: {} nb: {}\n",
            reader, self.readers, self.writers, self.nonblock
        );
        sched::wakeup(&self.wpos as *const usize as u64);
        sched::wakeup(&self.rpos as *const usize as u64);
        drop(lock);
        Ok(())
    }

    pub fn dup(&mut self, r: bool, w: bool) {
        let lock = self.lock.acquire();
        assert!(r != w);
        if r {
            self.readers += 1;
        }

        if w {
            self.writers += 1;
        }
        drop(lock)
    }
}

pub fn pipe2() -> u64 {
    let task = mycpu().get_task().unwrap();
    let tf = task.get_trap_frame().unwrap();

    let fds = as_slice_mut(tf.regs[0] as *mut u32, 2);

    if let Ok(pip) = fs::open_pipe(tf.regs[1] & fs::O::NONBLOCK as u64 != 0) {
        let dup = pip.dup(false, true).unwrap();
        let ridx = task
            .alloc_file(sched::FD {
                file: pip,
                read: true,
                write: false,
            })
            .unwrap();
        let widx = task
            .alloc_file(sched::FD {
                file: dup,
                read: false,
                write: true,
            })
            .unwrap();
        fds[0] = ridx as u32;
        fds[1] = widx as u32;
        0
    } else {
        return !0;
    }
}
