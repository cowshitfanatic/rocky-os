//! A tiny interactive shell.
//!
//! Input comes from two sources, both feeding the same line buffer:
//! * PS/2 keyboard (interrupt-driven, decoded with `pc-keyboard`)
//! * COM1 serial input (polled — QEMU forwards the host terminal/stdin
//!   to the serial port, so the shell is scriptable from the host)
//!
//! Commands: `help`, `hello`, `mem`, `ticks`, `tasks`, `clear`,
//! `exit`, `dblfault` (diagnostic: deliberately triggers a double
//! fault to prove the IST stack works; exits QEMU with the failure
//! code).

use crate::interrupts::TIMER_TICKS;
use crate::{QemuExitCode, exit_qemu, fb_print, serial_print};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use pc_keyboard::{DecodedKey, HandleControl, KeyCode, PS2Keyboard, ScancodeSet1, layouts::Us104Key};
use spin::Mutex;
use x86_64::instructions::port::Port;

static KEYBOARD: Mutex<PS2Keyboard<Us104Key, ScancodeSet1>> = Mutex::new(PS2Keyboard::new(
    ScancodeSet1::new(),
    Us104Key,
    HandleControl::Ignore,
));

fn out(s: &str) {
    // Write via the shared serial port (never re-initialise the UART —
    // that would wipe the receive FIFO and drop typed input).
    serial_print(format_args!("{}", s));
    fb_print!("{}", s);
}

fn outln(s: &str) {
    out(s);
    out("\n");
}

/// Poll COM1 for one byte, if one is waiting.
pub fn serial_read_byte() -> Option<u8> {
    unsafe {
        let mut lsr = Port::new(0x3FD);
        let status: u8 = lsr.read();
        if status & 1 == 0 {
            return None;
        }
        let mut data = Port::new(0x3F8);
        Some(data.read())
    }
}

/// Drain the PS/2 scancode queue and return any decoded characters.
/// Escape is returned as '\u{1b}' (the desktop uses it to exit).
pub fn keyboard_chars() -> Vec<char> {
    keyboard_keys()
        .into_iter()
        .filter_map(|k| match k {
            KeyInput::Char(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// A decoded keyboard input: a printable character, or a
/// navigation key the desktop browser uses for scrolling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyInput {
    Char(char),
    Up,
    Down,
    PageUp,
    PageDown,
}

/// Drain the PS/2 scancode queue, keeping arrow / page keys.
pub fn keyboard_keys() -> Vec<KeyInput> {
    let mut keys = Vec::new();
    let scancodes = crate::interrupts::scancode_drain();
    if scancodes.is_empty() {
        return keys;
    }
    let mut keyboard = KEYBOARD.lock();
    for scancode in scancodes {
        if let Ok(Some(event)) = keyboard.add_byte(scancode) {
            match keyboard.process_keyevent(event) {
                Some(DecodedKey::Unicode(c)) => keys.push(KeyInput::Char(c)),
                Some(DecodedKey::RawKey(KeyCode::Return)) => {
                    keys.push(KeyInput::Char('\n'))
                }
                Some(DecodedKey::RawKey(KeyCode::Backspace)) => {
                    keys.push(KeyInput::Char('\u{8}'))
                }
                Some(DecodedKey::RawKey(KeyCode::Escape)) => {
                    keys.push(KeyInput::Char('\u{1b}'))
                }
                Some(DecodedKey::RawKey(KeyCode::ArrowUp)) => keys.push(KeyInput::Up),
                Some(DecodedKey::RawKey(KeyCode::ArrowDown)) => {
                    keys.push(KeyInput::Down)
                }
                Some(DecodedKey::RawKey(KeyCode::PageUp)) => {
                    keys.push(KeyInput::PageUp)
                }
                Some(DecodedKey::RawKey(KeyCode::PageDown)) => {
                    keys.push(KeyInput::PageDown)
                }
                _ => {}
            }
        }
    }
    keys
}

/// Read one line, echoing as we go. Returns the line without the newline.
fn read_line() -> String {
    let mut line = String::new();
    loop {
        // Serial input first.
        if let Some(byte) = serial_read_byte() {
            let c = byte as char;
            match c {
                '\n' | '\r' => {
                    out("\n");
                    return line;
                }
                '\u{8}' | '\u{7f}' => {
                    if line.pop().is_some() {
                        out("\u{8}");
                    }
                }
                c if !c.is_control() => {
                    line.push(c);
                    let mut buf = [0u8; 4];
                    out(c.encode_utf8(&mut buf));
                }
                _ => {}
            }
            continue;
        }
        // Then PS/2 keyboard input.
        for c in keyboard_chars() {
            match c {
                '\n' => {
                    out("\n");
                    return line;
                }
                '\u{8}' => {
                    if line.pop().is_some() {
                        out("\u{8}");
                    }
                }
                c if !c.is_control() => {
                    line.push(c);
                    let mut buf = [0u8; 4];
                    out(c.encode_utf8(&mut buf));
                }
                _ => {}
            }
        }
        // Nothing waiting: give the background (heartbeat) task a
        // step if it is due, then sleep until the next interrupt
        // (timer tick wakes us ~18 times a second, so serial polling
        // stays responsive).
        crate::scheduler::background_tick();
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

/// Output sink for `run_command`: the caller decides where the
/// text goes (serial+framebuffer for the text shell, the
/// terminal window's scrollback for the desktop).
struct Out<'a> {
    sink: &'a mut dyn FnMut(&str),
}

impl Out<'_> {
    fn out(&mut self, s: &str) {
        (self.sink)(s);
    }
    fn outln(&mut self, s: &str) {
        (self.sink)(s);
        (self.sink)("\n");
    }
}

pub fn run_command(line: &str, memory_regions: usize, sink: &mut dyn FnMut(&str), in_desktop: bool) {
    let mut o = Out { sink };
    let mut parts = line.split_whitespace();
    match parts.next() {
        None => {}
        Some("help") => {
            o.outln("commands: help, hello, mem, ticks, tasks, ls, cat <file>, write <file> <text>, rm <file>, user, userbad, desktop, clear, exit, dblfault");
        }
        Some("user") => {
            o.outln("running user program in ring 3...");
            match crate::usermode::run_user(crate::usermode::USER_CODE_ADDR) {
                crate::usermode::UserResult::Exited(code) => {
                    o.outln(&alloc::format!("user program exited with code {}", code))
                }
                crate::usermode::UserResult::Faulted => {
                    o.outln("user program faulted (kernel survived)")
                }
            }
        }
        Some("userbad") => {
            o.outln("running bad user program (reads kernel memory)...");
            match crate::usermode::run_user(crate::usermode::USER_BAD_ADDR) {
                crate::usermode::UserResult::Exited(code) => {
                    o.outln(&alloc::format!("user program exited with code {} (unexpected!)", code))
                }
                crate::usermode::UserResult::Faulted => {
                    o.outln("user program faulted as expected (kernel survived)")
                }
            }
        }
        Some("ls") => {
            if !crate::fs::available() {
                o.outln("fs: no disk");
            } else {
                let files = crate::fs::list();
                if files.is_empty() {
                    o.outln("(empty)");
                }
                for f in files {
                    o.outln(&alloc::format!("{}  {} bytes", f.name, f.size));
                }
            }
        }
        Some("cat") => {
            let name = match parts.next() {
                Some(n) => n,
                None => {
                    o.outln("usage: cat <file>");
                    return;
                }
            };
            match crate::fs::read_file(name) {
                Ok(data) => {
                    o.out(&String::from_utf8_lossy(&data));
                    o.out("\n");
                }
                Err(e) => o.outln(&alloc::format!("cat: {}", e)),
            }
        }
        Some("write") => {
            let name = match parts.next() {
                Some(n) => n,
                None => {
                    o.outln("usage: write <file> <text>");
                    return;
                }
            };
            let text: alloc::vec::Vec<&str> = parts.collect();
            let text = text.join(" ");
            match crate::fs::write_file(name, text.as_bytes()) {
                Ok(()) => o.outln(&alloc::format!("wrote {} ({} bytes)", name, text.len())),
                Err(e) => o.outln(&alloc::format!("write: {}", e)),
            }
        }
        Some("rm") => {
            let name = match parts.next() {
                Some(n) => n,
                None => {
                    o.outln("usage: rm <file>");
                    return;
                }
            };
            match crate::fs::delete_file(name) {
                Ok(()) => o.outln(&alloc::format!("removed {}", name)),
                Err(e) => o.outln(&alloc::format!("rm: {}", e)),
            }
        }
        Some("desktop") => {
            if in_desktop {
                o.outln("already in the desktop");
            } else {
                crate::desktop::run(memory_regions);
            }
        }
        Some("hello") => {
            o.outln("Hello, Trevor! Rocky OS is alive.");
        }
        Some("mem") => {
            o.outln(&alloc::format!(
                "{} memory regions reported, heap is 512 KiB",
                memory_regions
            ));
        }
        Some("ticks") => {
            o.outln(&alloc::format!(
                "timer ticks since boot: {}",
                TIMER_TICKS.load(Ordering::SeqCst)
            ));
        }
        Some("tasks") => {
            o.outln("scheduler: preemptive round-robin, quantum 3 ticks (~165 ms)");
            o.outln("  shell (main) Running");
            for (id, name, state, steps) in crate::scheduler::list_tasks() {
                o.outln(&alloc::format!(
                    "  #{} {} {:?} steps {}",
                    id, name, state, steps
                ));
            }
        }
        Some("dblfault") => {
            o.outln("triggering a double fault (test)...");
            // Move the stack pointer somewhere unmapped, then take an
            // exception: pushing the interrupt frame faults during
            // delivery, which is a double fault. It must run on the
            // IST stack from gdt.rs, or the CPU triple-faults.
            unsafe {
                core::arch::asm!(
                    "mov rsp, {0}",
                    "int3",
                    in(reg) 0x0000_dead_0000_0000u64,
                    options(noreturn)
                );
            }
        }
        Some("clear") => {
            crate::framebuffer::clear();
            o.out("\n");
        }
        Some("exit") => {
            if in_desktop {
                o.outln("use Esc or the Exit button to leave the desktop");
            } else {
                o.outln("Bye!");
                exit_qemu(QemuExitCode::Success);
            }
        }
        Some(other) => {
            o.outln(&alloc::format!("unknown command: {} (try 'help')", other));
        }
    }
}

pub fn run(memory_regions: usize) -> ! {
    outln("Rocky OS shell — type 'help' for commands.");
    loop {
        out("rocky> ");
        let line = read_line();
        let mut sink = |s: &str| {
            serial_print(format_args!("{}", s));
            fb_print!("{}", s);
        };
        run_command(line.trim(), memory_regions, &mut sink, false);
    }
}
