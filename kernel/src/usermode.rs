//! Ring-3 user mode.
//!
//! A user program is a small blob of machine code, assembled in
//! this file, copied into a user-accessible page and entered with
//! `iretq` (user CS/SS, user stack, RFLAGS with IF set). It talks
//! to the kernel only through `int 0x80` syscalls:
//!
//! | number | name   | args (rbx, rcx, rdx) | returns |
//! |--------|--------|----------------------|---------|
//! | 0      | exit   | code in rbx          | —       |
//! | 1      | write  | fd (ignored), ptr, len | bytes written, or -1 |
//! | 2      | yield  | —                    | 0       |
//! | 3      | ticks  | —                    | timer ticks |
//!
//! Getting back to the kernel: `int 0x80` switches the CPU to
//! ring 0 on the TSS rsp0 stack. The syscall entry stub (assembly
//! below) saves the user registers, calls the Rust handler, and
//! either `iret`s back to the user program or — on exit — jumps
//! straight back to the return point in `run_user`, restoring the
//! kernel stack pointer saved before entry. A fault in ring 3
//! (page fault, #GP, …) does the same jump from the fault handler
//! via `faulted_user_program`, so a crashing user program can
//! never take the kernel down with it.

use crate::interrupts::TIMER_TICKS;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use x86_64::VirtAddr;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

pub const USER_CODE_ADDR: u64 = 0x400_000;
pub const USER_BAD_ADDR: u64 = 0x401_000;
pub const USER_STACK_TOP: u64 = 0x802_000;
pub const USER_STACK_PAGES: u64 = 2;
/// User programs may only touch this address range (their code
/// pages, 0x400000..0x402000, and their stack, 0x800000..0x802000).
const USER_RANGE_START: u64 = 0x400_000;
const USER_RANGE_END: u64 = 0x900_000;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_TICKS: u64 = 3;

static USER_EXITED: AtomicBool = AtomicBool::new(false);
static USER_FAULTED: AtomicBool = AtomicBool::new(false);
static USER_EXIT_CODE: AtomicU64 = AtomicU64::new(0);
static USER_SHOULD_RETURN: AtomicBool = AtomicBool::new(false);
/// Return point saved by `run_user` before `iretq`: the kernel
/// stack pointer and the address just after the `iretq`.
static ENTER_RETURN_RSP: AtomicUsize = AtomicUsize::new(0);
static ENTER_RETURN_RIP: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserResult {
    Exited(u64),
    Faulted,
}

// The user programs, assembled here so they are position
// independent (RIP-relative `lea`) and can be copied to any
// user page. `user_prog` prints, asks for the tick count and
// exits 0. `user_bad_prog` reads from a kernel address, which
// must page-fault in ring 3.
core::arch::global_asm!(
    ".global user_prog_start",
    ".global user_prog_end",
    ".global user_bad_start",
    ".global user_bad_end",
    "user_prog_start:",
    "  mov rax, 1", // SYS_WRITE
    "  mov rbx, 1",
    "  lea rcx, [rip + user_msg]",
    "  mov rdx, OFFSET user_msg_len",
    "  int 0x80",
    "  mov rax, 3", // SYS_TICKS
    "  int 0x80",
    "  mov rax, 0", // SYS_EXIT
    "  mov rbx, 0",
    "  int 0x80",
    "  hlt",
    "user_msg:",
    "  .ascii \"Hello from ring 3!\\n\"",
    "  .equ user_msg_len, . - user_msg",
    "user_prog_end:",
    "user_bad_start:",
    "  mov rax, 0x444444440000", // kernel heap: not user-accessible
    "  mov rax, [rax]",
    "  mov rax, 0",
    "  mov rbx, 0",
    "  int 0x80",
    "  hlt",
    "user_bad_end:",
);

unsafe extern "C" {
    fn user_prog_start();
    fn user_prog_end();
    fn user_bad_start();
    fn user_bad_end();
    fn syscall_entry();
}

/// Address of the `int 0x80` assembly entry point, for the IDT.
pub fn syscall_entry_addr() -> u64 {
    syscall_entry as *const () as u64
}

/// Saved user registers, in the order the syscall stub pushes
/// them (last pushed first in memory).
#[repr(C)]
pub struct SavedRegs {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

// The `int 0x80` entry point. The CPU has already switched to
// the TSS rsp0 stack and pushed SS/RSP/RFLAGS/CS/RIP. Save the
// user registers, call the Rust handler, write its return value
// into the saved rax slot, and either `iret` back to the user
// program or — on exit — jump back to the kernel caller.
core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    "  push r15",
    "  push r14",
    "  push r13",
    "  push r12",
    "  push rbp",
    "  push rbx",
    "  push r11",
    "  push r10",
    "  push r9",
    "  push r8",
    "  push rsi",
    "  push rdi",
    "  push rdx",
    "  push rcx",
    "  push rax",
    "  mov rdi, rsp",
    "  call syscall_rust",
    "  mov [rsp], rax",
    "  call should_return",
    "  cmp al, 0",
    "  jne .Lreturn_to_kernel",
    "  pop rax",
    "  pop rcx",
    "  pop rdx",
    "  pop rdi",
    "  pop rsi",
    "  pop r8",
    "  pop r9",
    "  pop r10",
    "  pop r11",
    "  pop rbx",
    "  pop rbp",
    "  pop r12",
    "  pop r13",
    "  pop r14",
    "  pop r15",
    "  iretq",
    ".Lreturn_to_kernel:",
    "  call return_to_kernel",
);

/// Rust syscall handler. Called from the assembly stub with a
/// pointer to the saved user registers. Returns the value to
/// place in the user program's rax.
#[unsafe(no_mangle)]
pub extern "C" fn syscall_rust(regs: *mut SavedRegs) -> u64 {
    let regs = unsafe { &*regs };
    match regs.rax {
        SYS_EXIT => {
            USER_EXIT_CODE.store(regs.rbx, Ordering::SeqCst);
            USER_EXITED.store(true, Ordering::SeqCst);
            USER_SHOULD_RETURN.store(true, Ordering::SeqCst);
            0
        }
        SYS_WRITE => {
            let ptr = regs.rcx;
            let len = regs.rdx as usize;
            if ptr < USER_RANGE_START || ptr.saturating_add(len as u64) > USER_RANGE_END {
                return u64::MAX;
            }
            let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
            let text = String::from_utf8_lossy(bytes);
            crate::serial_print(format_args!("{}", text));
            crate::fb_print!("{}", text);
            len as u64
        }
        SYS_YIELD => 0,
        SYS_TICKS => TIMER_TICKS.load(Ordering::SeqCst),
        _ => u64::MAX,
    }
}

/// Jump back to `run_user`'s return point, restoring the kernel
/// stack. Called from kernel mode (syscall exit / ring-3 fault)
/// and never returns.
#[unsafe(no_mangle)]
pub extern "C" fn return_to_kernel() -> ! {
    let rsp = ENTER_RETURN_RSP.load(Ordering::SeqCst);
    let rip = ENTER_RETURN_RIP.load(Ordering::SeqCst);
    unsafe {
        core::arch::asm!(
            "mov rsp, {0}",
            "jmp {1}",
            in(reg) rsp,
            in(reg) rip,
            options(noreturn)
        );
    }
}

/// Called from a fault handler when the faulting code was ring 3:
/// mark the program faulted and return to the kernel caller.
pub fn faulted_user_program() -> ! {
    USER_FAULTED.store(true, Ordering::SeqCst);
    USER_SHOULD_RETURN.store(true, Ordering::SeqCst);
    return_to_kernel();
}

fn map_user_page(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    virt: u64,
) -> Result<(), &'static str> {
    let page = Page::containing_address(VirtAddr::new(virt));
    let frame = frame_allocator
        .allocate_frame()
        .ok_or("no frame for user page")?;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    unsafe {
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .map_err(|_| "map user page failed")?
            .flush();
    }
    Ok(())
}

/// Map the user code pages and stack, and copy the embedded
/// programs into them. Call once, after paging/heap/GDT are up
/// and before the shell runs.
pub fn setup_user(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), &'static str> {
    map_user_page(mapper, frame_allocator, USER_CODE_ADDR)?;
    map_user_page(mapper, frame_allocator, USER_BAD_ADDR)?;
    for i in 0..USER_STACK_PAGES {
        map_user_page(mapper, frame_allocator, 0x800_000 + i * 0x1000)?;
    }
    unsafe {
        let prog = core::slice::from_raw_parts(
            user_prog_start as *const u8,
            (user_prog_end as *const () as usize)
                .wrapping_sub(user_prog_start as *const () as usize),
        );
        let dst = core::slice::from_raw_parts_mut(USER_CODE_ADDR as *mut u8, prog.len());
        dst.copy_from_slice(prog);
        let bad = core::slice::from_raw_parts(
            user_bad_start as *const u8,
            (user_bad_end as *const () as usize)
                .wrapping_sub(user_bad_start as *const () as usize),
        );
        let dst = core::slice::from_raw_parts_mut(USER_BAD_ADDR as *mut u8, bad.len());
        dst.copy_from_slice(bad);
    }
    Ok(())
}

/// Run a user program at `entry` (USER_CODE_ADDR or USER_BAD_ADDR)
/// until it exits or faults. Returns how it ended.
pub fn run_user(entry: u64) -> UserResult {
    USER_EXITED.store(false, Ordering::SeqCst);
    USER_FAULTED.store(false, Ordering::SeqCst);
    USER_SHOULD_RETURN.store(false, Ordering::SeqCst);
    let user_cs = crate::gdt::user_code_selector().0 as u64;
    let user_ss = crate::gdt::user_data_selector().0 as u64;
    unsafe {
        core::arch::asm!(
            // Save the return point before pushing the iret frame.
            "mov {ret_rsp}, rsp",
            "lea {ret_rip}, [rip + 2f]",
            "mov [{ret_rsp_ptr}], {ret_rsp}",
            "mov [{ret_rip_ptr}], {ret_rip}",
            // iret frame: SS, RSP, RFLAGS, CS, RIP.
            "push {user_ss}",
            "push {user_rsp}",
            "pushfq",
            "push {user_cs}",
            "push {user_rip}",
            "iretq",
            "2:",
            ret_rsp = out(reg) _,
            ret_rip = out(reg) _,
            ret_rsp_ptr = in(reg) ENTER_RETURN_RSP.as_ptr() as u64,
            ret_rip_ptr = in(reg) ENTER_RETURN_RIP.as_ptr() as u64,
            user_ss = in(reg) user_ss,
            user_rsp = in(reg) USER_STACK_TOP,
            user_cs = in(reg) user_cs,
            user_rip = in(reg) entry,
        );
    }
    if USER_FAULTED.load(Ordering::SeqCst) {
        UserResult::Faulted
    } else {
        UserResult::Exited(USER_EXIT_CODE.load(Ordering::SeqCst))
    }
}

/// Whether the user program asked to exit (checked by the syscall
/// stub after each syscall).
#[unsafe(no_mangle)]
pub extern "C" fn should_return() -> bool {
    USER_SHOULD_RETURN.load(Ordering::SeqCst)
}
