use core::{cmp, mem::MaybeUninit};

use alloc::boxed::Box;

use crate::{
    fs::{self, File, open},
    p9,
    stuff::as_slice_mut,
};

/* 64-bit ELF base types. */
type Elf64Addr = u64;
type Elf64Half = u16;
type Elf64SHalf = i16;
type Elf64Off = u64;
type Elf64Sword = i32;
type Elf64Word = u32;
type Elf64Xword = u64;
type Elf64Sxword = i64;
type Elf64Versym = u16;

/* These constants are for the segment types stored in the image headers */
pub const PT_NULL: u64 = 0;
pub const PT_LOAD: u64 = 1;
pub const PT_DYNAMIC: u64 = 2;
pub const PT_INTERP: u64 = 3;
pub const PT_NOTE: u64 = 4;
pub const PT_SHLIB: u64 = 5;
pub const PT_PHDR: u64 = 6;
pub const PT_TLS: u64 = 7; /* Thread local storage segment */
pub const PT_LOOS: u64 = 0x60000000; /* OS-specific */
pub const PT_HIOS: u64 = 0x6fffffff; /* OS-specific */
pub const PT_LOPROC: u64 = 0x70000000;
pub const PT_HIPROC: u64 = 0x7fffffff;
pub const PT_GNU_EH_FRAME: u64 = PT_LOOS + 0x474e550;
pub const PT_GNU_STACK: u64 = PT_LOOS + 0x474e551;
pub const PT_GNU_RELRO: u64 = PT_LOOS + 0x474e552;
pub const PT_GNU_PROPERTY: u64 = PT_LOOS + 0x474e553;

/* These constants define the different elf file types */
const ET_NONE: u16 = 0;
const ET_REL: u16 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const ET_CORE: u16 = 4;
const ET_LOPROC: u16 = 0xff00;
const ET_HIPROC: u16 = 0xffff;

/* ARM MTE memory tag segment type */
const PT_AARCH64_MEMTAG_MTE: u64 = PT_LOPROC + 0x2;

const EI_NIDENT: usize = 16;

const EI_MAG0: usize = 0; /* e_ident[] indexes */
const EI_MAG1: usize = 1;
const EI_MAG2: usize = 2;
const EI_MAG3: usize = 3;
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const EI_VERSION: usize = 6;
const EI_OSABI: usize = 7;
const EI_PAD: usize = 8;

#[repr(C)]
pub struct Elf64Hdr {
    ident: [u8; EI_NIDENT], /* ELF "magic number" */
    kind: Elf64Half,
    machine: Elf64Half,
    version: Elf64Word,
    pub entry: Elf64Addr, /* Entry point virtual address */
    phoff: Elf64Off,      /* Program header table file offset */
    shoff: Elf64Off,      /* Section header table file offset */
    flags: Elf64Word,
    ehsize: Elf64Half,
    phentsize: Elf64Half,
    phnum: Elf64Half,
    shentsize: Elf64Half,
    shnum: Elf64Half,
    shstrndx: Elf64Half,
}

impl Elf64Hdr {
    const fn zeroed() -> Elf64Hdr {
        Elf64Hdr {
            ident: [0; EI_NIDENT],
            kind: 0,
            machine: 0,
            version: 0,
            entry: 0,
            phoff: 0,
            shoff: 0,
            flags: 0,
            ehsize: 0,
            phentsize: 0,
            phnum: 0,
            shentsize: 0,
            shnum: 0,
            shstrndx: 0,
        }
    }
}

/* These constants define the permissions on sections in the program
header, p_flags. */
pub const PF_R: u32 = 0x4;
pub const PF_W: u32 = 0x2;
pub const PF_X: u32 = 0x1;

#[repr(C)]
#[derive(Debug)]
pub struct Elf64Phdr {
    pub kind: Elf64Word,
    pub flags: Elf64Word,
    pub offset: Elf64Off,   /* Segment file offset */
    pub vaddr: Elf64Addr,   /* Segment virtual address */
    pub paddr: Elf64Addr,   /* Segment physical address */
    pub filesz: Elf64Xword, /* Segment size in file */
    pub memsz: Elf64Xword,  /* Segment size in memory */
    pub align: Elf64Xword,  /* Segment alignment, file & memory */
}

impl Elf64Phdr {
    pub const fn zeroed() -> Elf64Phdr {
        Elf64Phdr {
            kind: 0,
            flags: 0,
            offset: 0,
            vaddr: 0,
            paddr: 0,
            filesz: 0,
            memsz: 0,
            align: 0,
        }
    }
}

const ELFCLASSNONE: u8 = 0; /* EI_CLASS */
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFCLASSNUM: u8 = 3;

pub struct Elf {
    pub header: Elf64Hdr,
    pub file: &'static mut File,
    idx: usize,
}

impl Elf {
    pub fn new(path: &str) -> Result<Elf, ()> {
        if let Ok(file) = open(path, fs::O::RDONLY as u32, 0) {
            let mut elf = Elf {
                header: Elf64Hdr::zeroed(),
                file,
                idx: 0,
            };
            let buf = as_slice_mut(
                (&mut elf.header) as *mut Elf64Hdr as *mut u8,
                size_of::<Elf64Hdr>(),
            );
            if let Ok(n) = elf.file.read(buf) {
                if n != buf.len() {
                    return Err(());
                }

                if &elf.header.ident[0..4] != &[0x7fu8, 0x45, 0x4c, 0x46] {
                    return Err(());
                }

                if elf.header.ident[EI_VERSION] != 1 {
                    return Err(());
                }

                if elf.header.ident[EI_CLASS] != ELFCLASS64 {
                    return Err(());
                }

                if elf.header.machine != 183 {
                    return Err(());
                }

                if elf.header.kind != ET_EXEC {
                    return Err(());
                }
            }
            Ok(elf)
        } else {
            return Err(());
        }
    }
}

impl Drop for Elf {
    fn drop(&mut self) {
        if let Ok(_) = self.file.close(true, false) {}
    }
}

pub struct PhIter<'a> {
    elf: &'a mut Elf,
    idx: usize,
}

impl<'a> PhIter<'a> {
    pub fn new(elf: &'a mut Elf) -> PhIter<'a> {
        PhIter { elf, idx: 0 }
    }

    pub fn next(&mut self, ph: *mut Elf64Phdr) -> Option<&'a mut Elf64Phdr> {
        if self.idx >= self.elf.header.phnum as usize {
            return None;
        }

        let ph = unsafe { ph.as_mut() }.unwrap();

        let offt = self.elf.header.phoff as usize + size_of::<Elf64Phdr>() * self.idx;
        self.elf.file.seek_to(offt);

        let buf = as_slice_mut(ph as *mut Elf64Phdr as *mut u8, size_of::<Elf64Phdr>());
        if let Ok(n) = self.elf.file.read(buf) {
            if n == buf.len() {
                self.idx += 1;
                Some(ph)
            } else {
                None
            }
        } else {
            None
        }
    }
}
