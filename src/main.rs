#![no_std]
#![no_main]
#![allow(unused)]

extern crate alloc;

use core::{
    arch::{asm, naked_asm},
    cell::UnsafeCell,
};

use crate::{arch::enable_fp, heap::SyncUnsafeCell};

mod arch;
mod blk;
mod cons;
mod elf;
mod fs;
mod heap;
mod p9;
mod pm;
mod rng;
mod rtc;
mod sched;
mod spin;
mod stuff;
mod svc;
mod timer;
mod trap;
mod tty;
mod uart;
mod virtio;
mod vm;

#[unsafe(no_mangle)]
fn main(b: usize, e: usize) {
    pm::init(b, e);
    vm::init(b, e);
    uart::init_tx();
    heap::init();
    trap::init();
    uart::init_rx();
    timer::init();
    rtc::init();
    virtio::init();
    enable_fp();
    sched::init();
    sched::create_user_task();
    sched::scheduler();
    loop {
        wfi!();
    }
}

unsafe extern "C" {
    pub static _boot_stack: u64;
    pub static _boot_stack_btm: u64;
    pub static _trap_vec: u64;
    pub static _kernel_begin: u64;
    pub static _kernel_end: u64;

    pub static _text_end: u64;
    pub static _data_end: u64;
    pub static _rodata_end: u64;
    pub static _bss_end: u64;
    pub static _user_end: u64;
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".boot.data")]
#[unsafe(naked)]
pub extern "C" fn _data() {
    naked_asm!(
        ".align 12",
        "l0_id: .8byte 0",
        ".align 12",
        "l0_h: .8byte 0",
        ".align 12",
        "l1_id0: .8byte 0",
        "l1_id1: .8byte 0",
        ".align 12",
        "l1_h0: .8byte 0",
        "l1_h1: .8byte 0",
    )
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".boot.text")]
#[unsafe(naked)]
pub extern "C" fn _start() {
    naked_asm!(
       "ldr x0, =0x5b0103210",
       "msr tcr_el1, x0",
       "ldr x0, =l0_id",
       "msr ttbr0_el1, x0",
       "ldr x1, =l0_h",
       "msr ttbr1_el1, x1",
       "ldr x2, =l1_id0",
       "ldr x3, =0x40000401",
       "str x3, [x2, #8]",
       "orr x2, x2, #3",
       "str x2, [x0]",
       "ldr x2, =l1_h0",
       "str x3, [x2, #8]",
       "orr x2, x2, #3",
       "str x2, [x1]",
       "mov x0, #0xff",
       "msr mair_el1, x0",
       "dsb sy",
       "isb sy",
       "mrs x0, sctlr_el1",
       "orr x0, x0, #1",
       "msr sctlr_el1, x0",
       "isb sy",
       "ldr x0, ={stack}",
       "mov sp, x0",
       "ldr x0, ={trap}",
       "msr vbar_el1, x0",
       "ldr x0, ={begin}",
       "ldr x1, ={end}",
       "bl main",
       "1:",
       "wfi",
       "b 1b",
        stack = sym _boot_stack,
        trap = sym _trap_vec,
        begin = sym _kernel_begin,
        end = sym _kernel_end,
    );
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    print!("{}", info);
    loop {
        wfi!();
    }
}

// MSR <Special-purpose_register>, Xt ; Write to Special-purpose register
// MRS Xt, <Special-purpose_register> ; Read from Special-purpose register
