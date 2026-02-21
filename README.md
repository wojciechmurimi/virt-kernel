## virt-kernel

A small AArch64 kernel written in Rust.

Boots on the QEMU virt board and implements just enough of the Linux ABI to run BusyBox and small user programs.

This is not a serious OS.
It’s a playground for AArch64 kernel hacking.

### Features

* Boots on QEMU (`virt`, AArch64)
* UART console
* Preemptive scheduler
* Virtual memory with page tables
* Buddy allocator for physical memory
* Kernel heap on top of the buddy allocator
* Virtio-9p
* Pipes and TTY support
* Minimal Linux-style syscall layer
* BusyBox userland

You can run things like `ls`, `cat`, `stat`, `vi`, etc.

### Requirements

* `aarch64-unknown-none-softfloat` toolchain
* QEMU (AArch64 system emulator)

### Running

```bash
$ cargo run --target aarch64-unknown-none-softfloat --release
```

This builds the kernel and launches QEMU via `run.sh`.

### Notes

* Syscalls are incomplete and added as needed
* Userland assumes BusyBox behavior
* The code favors experimentation over correctness
* Much unsafe code

