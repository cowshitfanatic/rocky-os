//! COM2 serial port (UART 16550 at 0x2F8) — the browser's link to
//! the host-side net proxy (`scripts/net-proxy.py`).
//!
//! Rocky OS has no Ethernet/TCP/IP stack. The browser fetches pages
//! over this second serial port, like dial-up: the kernel sends
//! `GET <url>` and the proxy on the host fetches the real page over
//! HTTPS and streams the extracted text back.
//!
//! Polled only (no IRQ), mirroring `shell::serial_read_byte` for
//! COM1. COM1 is never touched here.

use x86_64::instructions::port::Port;

const DATA: u16 = 0x2F8;
const IER: u16 = 0x2F9;
const FCR: u16 = 0x2FA;
const LCR: u16 = 0x2FB;
const MCR: u16 = 0x2FC;
const LSR: u16 = 0x2FD;

fn read_port(port: u16) -> u8 {
    unsafe { Port::new(port).read() }
}

fn write_port(port: u16, v: u8) {
    unsafe { Port::new(port).write(v) }
}

/// Initialise COM2 at 115200 baud, 8N1, FIFO on. Safe to call
/// again (e.g. every time the desktop is entered).
pub fn init() {
    write_port(IER, 0x00); // disable interrupts
    write_port(LCR, 0x80); // DLAB: set divisor
    write_port(DATA, 0x01); // divisor low: 115200 baud
    write_port(IER, 0x00); // divisor high
    write_port(LCR, 0x03); // 8 bits, no parity, 1 stop bit
    write_port(FCR, 0xC7); // enable FIFO, clear, 14-byte threshold
    write_port(MCR, 0x0B); // DTR, RTS, OUT2
    // Drain any stale bytes.
    while read_port(LSR) & 0x01 != 0 {
        let _ = read_port(DATA);
    }
    crate::serial_print(format_args!("net: com2 init (115200 8N1)\n"));
}

/// Poll for one byte, if one is waiting.
pub fn read_byte() -> Option<u8> {
    if read_port(LSR) & 0x01 == 0 {
        return None;
    }
    Some(read_port(DATA))
}

/// Write bytes, waiting for the transmitter to be ready.
/// Bounded: gives up after ~100k polls per byte so a missing
/// peer (no proxy connected) can never hang the kernel.
pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        for _ in 0..100_000 {
            if read_port(LSR) & 0x20 != 0 {
                break;
            }
        }
        write_port(DATA, b);
    }
}

pub fn write_str(s: &str) {
    write_bytes(s.as_bytes());
}

/// Read one line (up to and including '\n') with a tick budget.
/// `budget_ticks` is in timer ticks (~18.2/s). Returns None on
/// timeout. Partial data is discarded on timeout.
#[allow(dead_code)]
pub fn read_line(budget_ticks: u64) -> Option<alloc::string::String> {
    use crate::interrupts::TIMER_TICKS;
    use core::sync::atomic::Ordering;
    let start = TIMER_TICKS.load(Ordering::SeqCst);
    let mut line = alloc::string::String::new();
    loop {
        if let Some(b) = read_byte() {
            if b == b'\n' {
                return Some(line);
            }
            if b != b'\r' && line.len() < 512 {
                line.push(b as char);
            }
        }
        if TIMER_TICKS.load(Ordering::SeqCst).wrapping_sub(start) >= budget_ticks
        {
            return None;
        }
        // Yield the CPU briefly; the timer tick wakes us.
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

/// Read exactly `len` bytes with a tick budget. Returns None on
/// timeout (partial data is kept in `out` by the caller).
#[allow(dead_code)]
pub fn read_exact(out: &mut alloc::vec::Vec<u8>, len: usize, budget_ticks: u64) -> bool {
    use crate::interrupts::TIMER_TICKS;
    use core::sync::atomic::Ordering;
    let start = TIMER_TICKS.load(Ordering::SeqCst);
    while out.len() < len {
        if let Some(b) = read_byte() {
            out.push(b);
            continue;
        }
        if TIMER_TICKS.load(Ordering::SeqCst).wrapping_sub(start) >= budget_ticks
        {
            return false;
        }
        x86_64::instructions::interrupts::enable_and_hlt();
    }
    true
}
