//! PS/2 mouse on the 8042 controller.
//!
//! The mouse shares the keyboard controller's data port (0x60)
//! but raises IRQ12. Initialisation (done once, with interrupts
//! disabled so ACK bytes are read here, not by the IRQ handler):
//! enable the AUX port, set the controller config byte (mouse IRQ
//! on), then send the mouse "enable data reporting" (0xF4) and
//! wait for ACK (0xFA). Packets are 3 bytes: status, dx, dy.
//! The IRQ handler feeds bytes to `handle_byte`, which assembles
//! packets and updates the shared position/buttons.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;

/// Mouse state shared between the IRQ12 handler and the desktop.
/// All atomics: an interrupt handler must never spin on a lock
/// that the interrupted code may hold (that deadlocks the CPU
/// silently — the handler runs with interrupts off, so the
/// interrupted code can never release the lock).
pub static MOUSE_X: AtomicI32 = AtomicI32::new(640);
pub static MOUSE_Y: AtomicI32 = AtomicI32::new(360);
/// Bit 0 = left, 1 = right, 2 = middle.
pub static MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);
pub static MOUSE_CHANGED: AtomicBool = AtomicBool::new(false);
pub static MOUSE_READY: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy)]
pub struct MouseState {
    pub x: i32,
    pub y: i32,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub changed: bool,
    pub ready: bool,
}

/// Snapshot of the current mouse state (lock-free read).
pub fn state() -> MouseState {
    let buttons = MOUSE_BUTTONS.load(Ordering::SeqCst);
    MouseState {
        x: MOUSE_X.load(Ordering::SeqCst),
        y: MOUSE_Y.load(Ordering::SeqCst),
        left: buttons & 0x01 != 0,
        right: buttons & 0x02 != 0,
        middle: buttons & 0x04 != 0,
        changed: MOUSE_CHANGED.load(Ordering::SeqCst),
        ready: MOUSE_READY.load(Ordering::SeqCst),
    }
}

/// Clear the changed flag (called by the desktop after reading).
pub fn clear_changed() {
    MOUSE_CHANGED.store(false, Ordering::SeqCst);
}

/// Packet assembler state: bytes collected so far (0..3).
static PACKET: Mutex<(u8, [u8; 3])> = Mutex::new((0, [0; 3]));

fn status() -> u8 {
    unsafe { Port::new(0x64).read() }
}

fn wait_input_empty() {
    for _ in 0..100_000 {
        if status() & 0x02 == 0 {
            return;
        }
    }
}

fn wait_output_full() -> bool {
    for _ in 0..100_000 {
        if status() & 0x01 != 0 {
            return true;
        }
    }
    false
}

fn read_data() -> Option<u8> {
    if wait_output_full() {
        Some(unsafe { Port::new(0x60).read() })
    } else {
        None
    }
}

/// Send a command byte to the 8042 command port (0x64).
fn cmd(byte: u8) {
    wait_input_empty();
    unsafe { Port::new(0x64).write(byte) };
}

/// Send a byte to the mouse (via 0xD4), then the byte itself.
fn mouse_cmd(byte: u8) {
    cmd(0xD4);
    wait_input_empty();
    unsafe { Port::new(0x60).write(byte) };
}

fn drain_output() {
    while status() & 0x01 != 0 {
        let _: u8 = unsafe { Port::new(0x60).read() };
    }
}

/// Initialise the PS/2 mouse. Call with interrupts disabled.
/// Returns true if the mouse acknowledged.
pub fn init() -> bool {
    drain_output();
    // Enable the AUX (mouse) port.
    cmd(0xA8);
    // Read the controller config byte, set mouse IRQ (bit 1) and
    // keyboard IRQ (bit 0), clear mouse-clock disable (bit 5).
    cmd(0x20);
    let config = read_data().unwrap_or(0);
    let config = (config | 0x03) & !0x20;
    cmd(0x60);
    wait_input_empty();
    unsafe { Port::new(0x60).write(config) };
    // Set defaults, then enable data reporting; each must ACK.
    mouse_cmd(0xF6);
    let ack1 = read_data();
    mouse_cmd(0xF4);
    let ack2 = read_data();
    let ok = ack1 == Some(0xFA) && ack2 == Some(0xFA);
    if ok {
        MOUSE_READY.store(true, Ordering::SeqCst);
    }
    crate::serial_print_try(format_args!("mouse: init {}\n", if ok { "ok" } else { "no ack" }));
    ok
}

/// Feed one byte from the IRQ12 handler into the packet assembler.
pub fn handle_byte(b: u8) {
    let mut packet = PACKET.lock();
    let (idx, bytes) = &mut *packet;
    if *idx == 0 {
        // A packet's first byte always has bit 3 set.
        if b & 0x08 == 0 {
            return;
        }
        bytes[0] = b;
        *idx = 1;
        return;
    }
    bytes[*idx as usize] = b;
    *idx += 1;
    if *idx == 3 {
        *idx = 0;
        let (status, dx, dy) = (bytes[0], bytes[1] as i8, bytes[2] as i8);
        drop(packet);
        let w = crate::framebuffer::width().max(1) as i32;
        let h = crate::framebuffer::height().max(1) as i32;
        let x = (MOUSE_X.load(Ordering::SeqCst) + dx as i32).clamp(0, w - 1);
        let y = (MOUSE_Y.load(Ordering::SeqCst) - dy as i32).clamp(0, h - 1);
        MOUSE_X.store(x, Ordering::SeqCst);
        MOUSE_Y.store(y, Ordering::SeqCst);
        MOUSE_BUTTONS.store(status & 0x07, Ordering::SeqCst);
        MOUSE_CHANGED.store(true, Ordering::SeqCst);
        MOUSE_READY.store(true, Ordering::SeqCst);
    }
}
