//! Paging, frame allocation and the kernel heap.
//!
//! The bootloader maps all physical memory at the offset reported in
//! `BootInfo.physical_memory_offset` (enabled via our custom
//! `BOOTLOADER_CONFIG`). From that we build an `OffsetPageTable` over the
//! bootloader's level-4 table, hand out 4 KiB frames from the usable
//! regions of the memory map, and map a 100 KiB heap for `alloc`.

use bootloader_api::info::{MemoryRegion, MemoryRegionKind};
use linked_list_allocator::LockedHeap;
use x86_64::VirtAddr;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    mapper::OffsetPageTable,
};

pub const HEAP_START: usize = 0x_4444_4444_0000;
pub const HEAP_SIZE: usize = 100 * 1024;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// A frame allocator that hands out frames from the usable memory regions.
pub struct BootInfoFrameAllocator {
    memory_map: &'static [MemoryRegion],
    next: usize,
}

impl BootInfoFrameAllocator {
    /// # Safety
    /// The caller must guarantee the memory map is valid and that the
    /// returned allocator is the only one using these frames.
    pub unsafe fn init(memory_map: &'static [MemoryRegion]) -> Self {
        BootInfoFrameAllocator {
            memory_map,
            next: 0,
        }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> + '_ {
        let regions = self.memory_map.iter();
        let usable = regions.filter(|r| r.kind == MemoryRegionKind::Usable);
        let ranges = usable.map(|r| r.start..r.end);
        let frames = ranges.flat_map(|range| range.step_by(4096));
        frames.map(|addr| PhysFrame::containing_address(x86_64::PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

/// Initialise the page-table mapper from the bootloader's level-4 table.
pub fn init_page_table(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    unsafe { OffsetPageTable::new(&mut *page_table_ptr, physical_memory_offset) }
}

/// Map the heap pages and initialise the global allocator.
pub fn init_heap(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), x86_64::structures::paging::mapper::MapToError<Size4KiB>> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(x86_64::structures::paging::mapper::MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    Ok(())
}
