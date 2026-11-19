//! Interrupt handling: IDT, PIC, timer and PS/2 keyboard.
//!
//! The double-fault handler runs on the dedicated stack from
//! `gdt.rs` (IST index 0), so a double fault is handled even when
//! the normal kernel stack is broken.

use crate::{exit_qemu, QemuExitCode};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = 40;

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

/// Set by the breakpoint handler, so `kernel_main` can report it.
pub static BREAKPOINT_HIT: AtomicBool = AtomicBool::new(false);
/// Timer ticks since boot (PIT, ~18.2 Hz).
pub static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

/// PS/2 scancode queue, filled by the keyboard IRQ handler.
///
/// A lock-free single-producer/single-consumer ring buffer: the
/// keyboard handler is the only producer, the shell/desktop the
/// only consumer. A spin lock here would deadlock the same way
/// the mouse/FB lock did — the handler runs with interrupts off,
/// so if it spun on a lock the interrupted code held, the CPU
/// would never run the code that releases it.
const SCANCODE_CAP: usize = 128;
static SCANCODE_BUF: [AtomicU8; SCANCODE_CAP] =
    [const { AtomicU8::new(0) }; SCANCODE_CAP];
static SCANCODE_HEAD: AtomicUsize = AtomicUsize::new(0);
static SCANCODE_TAIL: AtomicUsize = AtomicUsize::new(0);

/// Push a scancode from the IRQ handler. Drops it if full.
pub fn scancode_push(byte: u8) {
    let head = SCANCODE_HEAD.load(Ordering::SeqCst);
    let next = (head + 1) % SCANCODE_CAP;
    if next == SCANCODE_TAIL.load(Ordering::SeqCst) {
        return; // full: drop
    }
    SCANCODE_BUF[head].store(byte, Ordering::SeqCst);
    SCANCODE_HEAD.store(next, Ordering::SeqCst);
}

/// Drain all queued scancodes (called from normal code).
pub fn scancode_drain() -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    loop {
        let tail = SCANCODE_TAIL.load(Ordering::SeqCst);
        if tail == SCANCODE_HEAD.load(Ordering::SeqCst) {
            break;
        }
        out.push(SCANCODE_BUF[tail].load(Ordering::SeqCst));
        SCANCODE_TAIL.store((tail + 1) % SCANCODE_CAP, Ordering::SeqCst);
    }
    out
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
    /// IRQ12: PS/2 mouse.
    Mouse = PIC_2_OFFSET + 4,
    /// IRQ14: primary IDE (ATA) — the disk asserts this when a
    /// PIO transfer completes. Without a handler the CPU faults
    /// (#NP) trying to deliver it, escalating to a double fault.
    IdePrimary = PIC_2_OFFSET + 6,
    /// IRQ15: secondary IDE.
    IdeSecondary = PIC_2_OFFSET + 7,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.general_protection_fault
            .set_handler_fn(general_protection_handler);
        idt.stack_segment_fault
            .set_handler_fn(stack_segment_handler);
        idt.segment_not_present
            .set_handler_fn(segment_not_present_handler);
        idt.invalid_tss.set_handler_fn(invalid_tss_handler);
        idt.x87_floating_point.set_handler_fn(x87_handler);
        idt.simd_floating_point
            .set_handler_fn(simd_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(crate::gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        idt[InterruptIndex::Mouse.as_u8()].set_handler_fn(mouse_interrupt_handler);
        idt[InterruptIndex::IdePrimary.as_u8()].set_handler_fn(ide_primary_handler);
        idt[InterruptIndex::IdeSecondary.as_u8()].set_handler_fn(ide_secondary_handler);
        // Syscall gate: int 0x80, callable from ring 3.
        unsafe {
            idt[0x80]
                .set_handler_addr(x86_64::VirtAddr::new(crate::usermode::syscall_entry_addr()))
                .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
        }
        idt
    };
}

pub fn init_idt() {
    IDT.load();
    unsafe { PICS.lock().initialize() };
    x86_64::instructions::interrupts::enable();
}

fn serial_write(msg: &str) {
    // Non-blocking: never wait on the serial lock from an interrupt.
    crate::serial_print_try(format_args!("{}\n", msg));
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    BREAKPOINT_HIT.store(true, Ordering::SeqCst);
    serial_write("EXCEPTION: BREAKPOINT");
    let _ = stack_frame;
}

macro_rules! diag_handler {
    ($name:ident, $label:expr) => {
        extern "x86-interrupt" fn $name(stack_frame: InterruptStackFrame, error_code: u64) {
            if stack_frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3 {
                crate::usermode::faulted_user_program();
            }
            crate::serial_print_try(format_args!(
                "EXCEPTION: {} error={:#x} rip={:#x} rsp={:#x} cs={:#x}\n",
                $label,
                error_code,
                stack_frame.instruction_pointer.as_u64(),
                stack_frame.stack_pointer.as_u64(),
                stack_frame.code_segment.0
            ));
            exit_qemu(QemuExitCode::Failed);
        }
    };
}

diag_handler!(general_protection_handler, "GENERAL PROTECTION");
diag_handler!(stack_segment_handler, "STACK SEGMENT");
diag_handler!(segment_not_present_handler, "SEGMENT NOT PRESENT");
diag_handler!(invalid_tss_handler, "INVALID TSS");

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    if stack_frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3 {
        crate::usermode::faulted_user_program();
    }
    crate::serial_print_try(format_args!(
        "EXCEPTION: INVALID OPCODE rip={:#x} rsp={:#x}\n",
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.stack_pointer.as_u64()
    ));
    exit_qemu(QemuExitCode::Failed);
}

extern "x86-interrupt" fn x87_handler(stack_frame: InterruptStackFrame) {
    crate::serial_print_try(format_args!(
        "EXCEPTION: X87 FP rip={:#x}\n",
        stack_frame.instruction_pointer.as_u64()
    ));
    exit_qemu(QemuExitCode::Failed);
}

extern "x86-interrupt" fn simd_handler(stack_frame: InterruptStackFrame) {
    crate::serial_print_try(format_args!(
        "EXCEPTION: SIMD FP rip={:#x}\n",
        stack_frame.instruction_pointer.as_u64()
    ));
    exit_qemu(QemuExitCode::Failed);
}

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    if stack_frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3 {
        crate::usermode::faulted_user_program();
    }
    serial_write("EXCEPTION: DIVIDE BY ZERO");
    // Returning would re-execute the faulting instruction and fault again,
    // so halt here rather than looping forever.
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    serial_write("EXCEPTION: DOUBLE FAULT");
    crate::serial_print_try(format_args!(
        "  double fault: error={:#x} rip={:#x} rsp={:#x}\n",
        error_code,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.stack_pointer.as_u64()
    ));
    exit_qemu(QemuExitCode::Failed);
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    let cr2 = Cr2::read().unwrap_or(x86_64::VirtAddr::new(0));
    crate::serial_print_try(format_args!(
        "EXCEPTION: PAGE FAULT at {:#x} error={:?} rip={:#x}\n",
        cr2.as_u64(),
        error_code,
        stack_frame.instruction_pointer.as_u64()
    ));
    // A fault from a user-mode program (CPL 3) kills just that
    // program: return to the kernel via the user-mode exit path.
    if stack_frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3 {
        crate::usermode::faulted_user_program();
    }
    exit_qemu(QemuExitCode::Failed);
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    TIMER_TICKS.fetch_add(1, Ordering::SeqCst);
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
    // EOI is sent first (see scheduler docs): preempting before it
    // would block the next timer IRQ behind this one.
    crate::scheduler::maybe_preempt();
}

extern "x86-interrupt" fn ide_primary_handler(_stack_frame: InterruptStackFrame) {
    // The ATA driver polls, so there is nothing to read here;
    // just acknowledge the interrupt so the PIC can deliver more.
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::IdePrimary.as_u8());
    }
}

extern "x86-interrupt" fn ide_secondary_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::IdeSecondary.as_u8());
    }
}

extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port = Port::new(0x60);
    let byte: u8 = unsafe { port.read() };
    crate::mouse::handle_byte(byte);
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Mouse.as_u8());
    }
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    scancode_push(scancode);
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}
