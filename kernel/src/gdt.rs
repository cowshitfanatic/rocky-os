//! Custom GDT + TSS with a dedicated double-fault stack.
//!
//! The bootloader provides its own GDT, but it has no TSS, so a
//! double fault would run on the (possibly already broken) kernel
//! stack. Here we load our own GDT with a kernel code segment and a
//! TSS whose interrupt-stack-table entry 0 points at a dedicated
//! 5-page double-fault stack, preceded by an unmapped guard page:
//!
//! ```text
//! 0x4444_4445_9000  guard page (unmapped — a stack overflow faults
//!                    here instead of silently corrupting the heap)
//! 0x4444_4445_A000  double-fault stack, 5 pages (20 KiB)
//! 0x4444_4445_F000  stack top (stacks grow down)
//! ```
//!
//! The IDT double-fault entry is wired to IST index 0 (see
//! `interrupts.rs`), so the CPU switches to this stack automatically
//! when a double fault occurs.

use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB, mapper::MapToError,
};
use x86_64::structures::tss::TaskStateSegment;

/// IST index used by the double-fault IDT entry.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Virtual address of the (unmapped) guard page below the stack.
pub const DOUBLE_FAULT_STACK_GUARD: u64 = 0x_4444_4445_9000;
/// Virtual address of the bottom of the double-fault stack.
pub const DOUBLE_FAULT_STACK_BASE: u64 = DOUBLE_FAULT_STACK_GUARD + 0x1000;
/// Number of 4 KiB pages in the double-fault stack.
pub const DOUBLE_FAULT_STACK_PAGES: u64 = 5;
/// Virtual address of the top of the double-fault stack.
pub const DOUBLE_FAULT_STACK_TOP: u64 =
    DOUBLE_FAULT_STACK_BASE + DOUBLE_FAULT_STACK_PAGES * 0x1000;

/// Top of the stack, filled in by `init()` before the TSS is built.
static STACK_TOP: AtomicU64 = AtomicU64::new(0);
/// Virtual address of the kernel stack for ring-3 traps: the CPU
/// switches to this stack via `TSS.privilege_stack_table[0]` when
/// a ring-3 program traps into the kernel.
pub const USER_KERNEL_STACK_BASE: u64 = 0x_4444_4447_0000;
pub const USER_KERNEL_STACK_PAGES: u64 = 4;
pub const USER_KERNEL_STACK_TOP: u64 =
    USER_KERNEL_STACK_BASE + USER_KERNEL_STACK_PAGES * 0x1000;

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
}

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            VirtAddr::new(STACK_TOP.load(Ordering::SeqCst));
        tss.privilege_stack_table[0] = VirtAddr::new(USER_KERNEL_STACK_TOP);
        tss
    };
}

/// Ring-3 code segment selector (index 5, RPL 3).
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code_selector
}

/// Ring-3 data segment selector (index 6, RPL 3).
pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data_selector
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        // The data segment must sit at index 2 (selector 0x10): the
        // bootloader's SS/DS still hold 0x10 when we load this GDT,
        // and `iretq` validates SS on every interrupt return. If
        // index 2 were the TSS instead, returning from the first
        // interrupt would #GP and escalate to a double fault.
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
        // User segments come after the TSS (which takes two GDT
        // entries, indices 3-4), so user code is index 5 and user
        // data index 6; the selectors carry RPL 3.
        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        (
            gdt,
            Selectors {
                code_selector,
                data_selector,
                tss_selector,
                user_code_selector,
                user_data_selector,
            },
        )
    };
}

/// Map the double-fault stack (leaving the guard page unmapped) and
/// load our GDT + TSS. Must run before `interrupts::init_idt()`.
pub fn init(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    // Map the 5 stack pages; the guard page below them is left unmapped.
    for i in 0..DOUBLE_FAULT_STACK_PAGES {
        let page = Page::containing_address(VirtAddr::new(DOUBLE_FAULT_STACK_BASE + i * 0x1000));
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    STACK_TOP.store(DOUBLE_FAULT_STACK_TOP, Ordering::SeqCst);

    // Map the kernel stack for ring-3 traps (TSS rsp0).
    for i in 0..USER_KERNEL_STACK_PAGES {
        let page = Page::containing_address(VirtAddr::new(USER_KERNEL_STACK_BASE + i * 0x1000));
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }
    // Touch the lazy statics (building the TSS from STACK_TOP) and load.
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.code_selector);
        SS::set_reg(GDT.1.data_selector);
        DS::set_reg(GDT.1.data_selector);
        ES::set_reg(GDT.1.data_selector);
        load_tss(GDT.1.tss_selector);
    }
    Ok(())
}
