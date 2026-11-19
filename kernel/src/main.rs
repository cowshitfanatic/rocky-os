#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod ata;
mod desktop;
mod framebuffer;
mod fs;
mod gdt;
mod interrupts;
mod memory;
mod mouse;
mod scheduler;
mod shell;
mod usermode;

use alloc::string::String;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use bootloader_api::config::Mapping;
use core::fmt::Write;
use uart_16550::backend::PioBackend;
use uart_16550::{Config, Uart16550Tty};
use x86_64::VirtAddr;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

#[repr(u32)]
pub(crate) enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

pub(crate) fn exit_qemu(code: QemuExitCode) -> ! {
    use x86_64::instructions::{nop, port::Port};

    unsafe {
        let mut port = Port::new(0xf4);
        port.write(code as u32);
    }

    loop {
        nop();
    }
}

/// The single COM1 port, initialised once. Re-creating the UART object
/// re-initialises the hardware and wipes the receive FIFO, so every
/// writer must share this one instead of calling `new_port` again.
pub(crate) static SERIAL: spin::Mutex<Option<Uart16550Tty<PioBackend>>> =
    spin::Mutex::new(None);

pub(crate) fn init_serial() {
    let port = unsafe { Uart16550Tty::new_port(0x3F8, Config::default()) }
        .expect("COM1 port and default config are always valid");
    *SERIAL.lock() = Some(port);
}

pub(crate) fn serial_print(args: core::fmt::Arguments) {
    if let Some(port) = SERIAL.lock().as_mut() {
        let _ = port.write_fmt(args);
    }
}

/// Non-blocking variant for interrupt/panic context: never waits on the
/// lock, just drops the message if the port is busy.
pub(crate) fn serial_print_try(args: core::fmt::Arguments) {
    if let Some(mut guard) = SERIAL.try_lock() {
        if let Some(port) = guard.as_mut() {
            let _ = port.write_fmt(args);
        }
    }
}

#[macro_export]
macro_rules! sprintln {
    ($($arg:tt)*) => {{
        crate::serial_print(format_args!($($arg)*));
        crate::serial_print(format_args!("\n"));
    }};
}

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init_serial();

    // Framebuffer first, so the greeting is visible on screen too.
    if let Some(fb) = boot_info.framebuffer.take() {
        framebuffer::init(fb);
    }

    sprintln!("Hello from Rocky OS!");
    fb_println!("Hello from Rocky OS!");
    sprintln!(
        "kernel booted, {} memory regions reported",
        boot_info.memory_regions.len()
    );
    fb_println!(
        "kernel booted, {} memory regions reported",
        boot_info.memory_regions.len()
    );

    // Paging + heap.
    let phys_mem_offset = match boot_info.physical_memory_offset.take() {
        Some(offset) => VirtAddr::new(offset),
        None => {
            sprintln!("ERROR: no physical memory offset from bootloader");
            fb_println!("ERROR: no physical memory offset");
            exit_qemu(QemuExitCode::Failed);
        }
    };
    let mut mapper = memory::init_page_table(phys_mem_offset);
    let mut frame_allocator =
        unsafe { memory::BootInfoFrameAllocator::init(&boot_info.memory_regions) };
    if let Err(e) = memory::init_heap(&mut mapper, &mut frame_allocator) {
        sprintln!("ERROR: heap init failed: {:?}", e);
        fb_println!("ERROR: heap init failed");
        exit_qemu(QemuExitCode::Failed);
    }

    // Heap demo: allocate a Vec and a String, prove they work.
    let mut numbers = Vec::new();
    for i in 0..10u32 {
        numbers.push(i * i);
    }
    let greeting = String::from("heap works: Rocky OS can allocate!");
    sprintln!("{}", greeting);
    fb_println!("{}", greeting);
    sprintln!("vec squares: {:?}", numbers);
    fb_println!("vec squares: {:?}", numbers);

    // Filesystem on the ATA data disk (if attached).
    match fs::init() {
        Ok(true) => {}
        Ok(false) => {
            sprintln!("fs: no data disk attached");
            fb_println!("fs: no data disk attached");
        }
        Err(e) => {
            sprintln!("fs: disk error: {}", e);
            fb_println!("fs: disk error");
        }
    }

    // GDT + TSS with a dedicated double-fault stack (must come
    // before the IDT, which wires double fault to the IST stack).
    if let Err(e) = gdt::init(&mut mapper, &mut frame_allocator) {
        sprintln!("ERROR: gdt init failed: {:?}", e);
        fb_println!("ERROR: gdt init failed");
        exit_qemu(QemuExitCode::Failed);
    }
    sprintln!("gdt loaded, double-fault stack ready");
    fb_println!("gdt loaded, double-fault stack ready");

    // User mode: map the user code/stack pages and copy the
    // embedded ring-3 programs into them.
    if let Err(e) = usermode::setup_user(&mut mapper, &mut frame_allocator) {
        sprintln!("ERROR: user mode setup failed: {}", e);
        fb_println!("ERROR: user mode setup failed");
        exit_qemu(QemuExitCode::Failed);
    }
    sprintln!("user mode ready (ring 3, syscalls via int 0x80)");
    fb_println!("user mode ready");

    // Interrupts: IDT + PIC, then prove the breakpoint handler works.
    interrupts::init_idt();
    x86_64::instructions::interrupts::int3();
    if interrupts::BREAKPOINT_HIT.load(core::sync::atomic::Ordering::SeqCst) {
        sprintln!("breakpoint handled, continuing");
        fb_println!("breakpoint handled, continuing");
    } else {
        sprintln!("ERROR: breakpoint handler did not run");
        fb_println!("ERROR: breakpoint handler did not run");
        exit_qemu(QemuExitCode::Failed);
    }
    sprintln!(
        "timer ticks so far: {}",
        interrupts::TIMER_TICKS.load(core::sync::atomic::Ordering::SeqCst)
    );

    // Scheduler: create the tasks and run the boot demo, switching
    // between them on timer ticks.
    scheduler::init();
    scheduler::run_demo();

    // Hand over to the interactive shell (serial + PS/2 keyboard).
    let region_count = boot_info.memory_regions.len();
    shell::run(region_count);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_print_try(format_args!("PANIC: {info}\n"));
    exit_qemu(QemuExitCode::Failed);
}
