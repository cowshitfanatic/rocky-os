# Rocky OS

A Rust toy kernel that boots in QEMU, built from scratch by Trevor
and Rocky.

## What works

- **Boot** — boots via the `bootloader` crate (BIOS, 0.11 API) and
  prints over the serial port (COM1) and on screen:
  - `Hello from Rocky OS!`
  - `kernel booted, 10 memory regions reported`
- **Screen output** — pixel framebuffer text (Noto Sans Mono bitmap
  font, 1280×720 in QEMU). Note: the bootloader sets up a graphics
  mode, so the classic VGA text buffer at `0xb8000` is *not* usable;
  text is rendered into the framebuffer from `BootInfo` instead.
- **Paging + heap** — the bootloader maps all physical memory at the
  offset in `BootInfo` (enabled with a custom `BOOTLOADER_CONFIG`),
  an `OffsetPageTable` is built over the bootloader's level-4 table,
  a frame allocator hands out 4 KiB frames from the usable memory
  regions, and a 512 KiB heap is mapped and registered as the global
  allocator (`linked_list_allocator`). The kernel proves it by
  allocating a `Vec` of squares and a `String` at boot.
- **Interrupts** — IDT with handlers for breakpoint, divide-by-zero,
  double fault and page fault; the 8259 PIC is remapped to IRQs
  32–47, with a timer handler (~18.2 Hz, counted in `TIMER_TICKS`)
  and a PS/2 keyboard handler. Boot proves the breakpoint path:
  `int3` runs the handler and execution continues.
- **GDT + TSS** — a custom GDT (kernel code + data segments and a
  TSS) with a dedicated 5-page double-fault stack behind an unmapped
  guard page. The double-fault IDT entry uses IST index 0, so a
  double fault is handled even with a broken kernel stack. The
  `dblfault` shell command deliberately triggers one to prove it:
  the handler prints `EXCEPTION: DOUBLE FAULT` and exits QEMU with
  the failure code (35). One subtlety worth knowing: the data
  segment has to sit at GDT index 2 (selector `0x10`), because the
  bootloader's SS still holds `0x10` when the new GDT is loaded and
  `iretq` validates SS on every interrupt return.
- **Scheduler** — a timer-driven, preemptive round-robin scheduler
  with real context switches: each task has its own 8 KiB stack and
  switching saves/restores the callee-saved registers and stack
  pointer. A task runs for at most a 3-tick quantum (~165 ms) before
  the timer interrupt handler preempts it to the next ready task, so
  a task that never yields cannot starve the rest. At boot three
  demo tasks run: `counter` and `printer` (5 steps each, yielding)
  and `hog`, which spins for ~2 s without ever yielding — the
  counter/printer steps interleave with its progress lines, which
  is the proof preemption works. A `heartbeat` task runs one step
  every ~2 s while the shell is idle. The `tasks` shell command
  lists the tasks and their step counts. Preemption is skipped
  while a task holds the serial/framebuffer lock or the task-table
  lock, so a preempted task can never deadlock the task switched
  in for it; see the safety notes at the top of `scheduler.rs`.
- **Shell** — an interactive shell with the prompt `rocky> `, fed by
  both the PS/2 keyboard (interrupt-driven, `pc-keyboard` US layout)
  and COM1 serial input (polled, so it is scriptable from the host).
  Commands: `help`, `hello`, `mem`, `ticks`, `tasks`, `ls`,
  `cat <file>`, `write <file> <text>`, `rm <file>`, `user`,
  `userbad`, `desktop`, `clear`, `exit` (exits QEMU cleanly with
  the success code), `dblfault` (double-fault test, exits with
  the failure code)
- **Disk + filesystem** — an ATA PIO driver (primary slave, LBA28,
  512-byte sectors, polling, IRQ14 acknowledged) and RockyFS, a
  minimal flat filesystem on an 8 MiB disk image (`disk.img`,
  created by the runner and persisted across boots): superblock
  magic `ROCKYFS1`, a 128-entry directory, contiguous file data.
  On first boot it formats and creates `readme.txt` and
  `hello.txt`; files written with `write` survive a reboot of the
  same disk image. Deleted files are marked unused; their sectors
  are not yet reused.
- **User mode** — ring-3 programs: user code/data segments in the
  GDT, a TSS `rsp0` kernel stack for traps, and syscalls via
  `int 0x80` (exit, write, yield, ticks) with a hand-written
  assembly entry stub that saves the user registers. Two embedded
  programs are copied into user pages at boot: `user` prints
  `Hello from ring 3!` through a syscall and exits 0, while
  `userbad` reads a kernel address and page-faults in ring 3 —
  the fault handler kills just the program and the kernel and
  shell carry on. Syscall write buffers are validated to lie in
  the user address range before being copied.
- **Desktop** — a graphical desktop on the framebuffer, entered
  with the `desktop` shell command and left with Esc or the Exit
  button in the top bar (which returns to the text shell). It has
  a PS/2 mouse driver (8042 init, IRQ12, 3-byte packets) and a
  polished "Aurora" look: a banded vertical-gradient wallpaper
  (indigo → violet → teal) with three glowing orbs and
  deterministic stars, a dark glass top bar (Rocky logo, focused
  app, search pill, live uptime clock, memory count, Exit), a
  floating bottom dock with seven apps and active dots, a Start
  menu, desktop icons on the left, and a live clock widget
  (large time, theme name, uptime, memory regions). Windows have
  drop shadows, rounded corners, gradient title bars and
  traffic-light buttons (red close, amber minimise to the dock,
  green maximise/restore), drag by the title bar, and focus brings
  them to the front. Seven apps ship in windows: a Terminal (runs
  the shell commands, output captured into a scrollback), a Files
  browser (lists RockyFS files, click one to view its contents),
  a Calculator (clickable buttons, `+ - * /` and parentheses,
  e.g. `7+8=` shows `15`), Notes (editable, Save writes
  `notes.txt` to RockyFS and reloads it on open), System Monitor
  (live uptime/ticks, memory, preemptions, scheduler task list,
  animated activity bar), Settings (version/specs plus three
  wallpaper themes — Aurora, Sunset, Midnight — applied on click),
  and a Web Browser (below).
- **Web Browser** — a scratch-built text browser (7th desktop
  app), honest about its transport: Rocky OS has **no Ethernet
  driver, no TCP/IP, no DNS, and no TLS in-kernel**, so it fetches
  like early dial-up over a second serial port. The kernel's
  COM2 driver (`kernel/src/netserial.rs`, UART 16550 at 0x2F8,
  115200 baud, polled) sends `GET <url>`; the host-side proxy
  (`scripts/net-proxy.py`, stdlib only) fetches the real page
  over HTTPS, extracts the article text with `html.parser`
  (title, headings, paragraphs, list items; script/style/nav
  dropped, capped at 16,000 chars), and streams it back as
  `OK <len>` + bytes + `END`. QEMU wires COM2 with
  `-serial unix:/tmp/rocky-net,server,nowait`, so booting never
  blocks when no proxy is running. The browser itself is
  scratch-built Rust in `desktop.rs`: URL bar (type a URL, Enter
  or Go fetches it), status line, word-wrapped page rendering
  (title in blue + underline, headings in amber), a clickable
  scrollbar, and Up/Down/PgUp/PgDn scrolling; the fetch is polled
  a little per desktop loop iteration so the UI stays responsive
  while ~16 KiB streams in at 115200 baud. It auto-loads
  `https://en.wikipedia.org/wiki/Chimpanzee` on first open, and
  the live fetch is proven by `scripts/test-browser.py` and
  `screenshot-browser.png`. Text only — no images, CSS, or JS —
  and the bitmap font has no IPA glyphs, so a few pronunciation
  characters render blank. Run the proxy first, then the OS:
  `python3 scripts/net-proxy.py` (in another terminal) +
  `cargo run`, then `desktop` and click the W dock icon.
  Rendering is a full redraw on each event; a per-pixel fill loop
  turned out to cost ~1.5 s per frame in a debug build, so filled
  rectangles are drawn with a row `memcpy` instead, and input is
  drained and rendered once per burst (one redraw per typed
  character would lag seconds) — a full redraw now measures
  ~120 ms in a debug build. Two rules that cost real debugging
  time, both documented in `mouse.rs` / `interrupts.rs`: an
  interrupt handler must never spin on a lock the interrupted
  code may hold (the mouse handler once locked the framebuffer
  and froze the CPU mid-render), so IRQ-shared state is lock-free
  — mouse state in atomics, the scancode queue as a
  single-producer/single-consumer ring buffer, and screen size
  cached in atomics at boot.

## Prerequisites

- Nightly Rust with `llvm-tools` and `rust-src`
  (`rust-toolchain.toml` pins nightly 1.101)
- `qemu-system-x86_64` (Ubuntu: `sudo apt install qemu-system-x86`)

> Note: apt-installed tools do **not** survive a VM restart in this
> environment — if `qemu-system-x86_64` is suddenly "not found",
> just re-run the apt install. Rust (in `~/.cargo`) and this project
> (in `~/workspace`) do survive.

## Run it

```sh
cargo run
```

Interactively, type at the `rocky> ` prompt. Or run the automated
end-to-end test, which boots QEMU, drives the shell, and checks the
output:

```sh
python3 scripts/test-shell.py
python3 scripts/test-double-fault.py
python3 scripts/test-preemption.py
python3 scripts/test-fs.py
python3 scripts/test-usermode.py
python3 scripts/test-desktop.py
python3 scripts/test-browser.py
```

`test-desktop.py` boots, enters the desktop, takes QEMU monitor
screendumps and checks the rendered pixels (aurora gradient,
top bar, dock, gradient title bars, clock, cursor), opens the
Start menu from the logo, launches Notes from the dock and types
in it, drags the Terminal window by its title bar, applies the
Sunset theme from Settings, and exits with Esc. One quirk it
works around: the first mouse packet after mouse init carries a
phantom +8 dx, so the test sends a warm-up `mouse_move 0 0`
first. Screendumps can also catch a render mid-frame (a full
redraw takes ~120 ms), so the pixel checks retry with fresh dumps.

(An instant `printf '...' | cargo run` burst is *not* reliable:
QEMU pushes host input into the UART before the kernel has
initialised it, so the first commands can be lost. Typing, or the
test script's one-line-at-a-time feed, works.)

You should see the boot lines, the heap demo, `breakpoint handled,
continuing`, then the shell output, and finally:

```
QEMU exited with: exit status: 33
```

33 is QEMU's encoding of our success code `0x10`
(`(0x10 << 1) | 1`). 35 would mean failure / panic.

The runner also opens a QEMU monitor socket at `/tmp/rocky-mon`
for `sendkey` (PS/2 keyboard) and `screendump` tests, e.g.:

```sh
socat - UNIX-CONNECT:/tmp/rocky-mon
# (QEMU monitor) sendkey h-e-l-l-o
# (QEMU monitor) screendump /tmp/rocky.ppm
```

## Layout

- `kernel/src/main.rs` — entry point, serial, boot sequence
  (framebuffer → paging/heap → GDT/TSS → user-mode setup →
  interrupts → scheduler demo → shell), panic handler
- `kernel/src/framebuffer.rs` — framebuffer text writer + graphics
  primitives (color pixels, rectangles, rounded rectangles,
  circles, text at pixel positions, scaled text)
- `kernel/src/memory.rs` — frame allocator, page table, heap
- `kernel/src/gdt.rs` — custom GDT + TSS, double-fault stack
  (5 pages + guard page), user segments, TSS `rsp0` stack
- `kernel/src/interrupts.rs` — IDT, PIC, timer/keyboard/mouse/
  IDE IRQs, syscall gate (`int 0x80`, DPL 3)
- `kernel/src/scheduler.rs` — round-robin tasks, context switch
  (`global_asm!`), demo + heartbeat tasks
- `kernel/src/shell.rs` — the `rocky> ` shell (commands are
  reusable: the desktop terminal runs them with output captured)
- `kernel/src/ata.rs` — ATA PIO disk driver
- `kernel/src/fs.rs` — RockyFS (flat filesystem on `disk.img`)
- `kernel/src/usermode.rs` — ring-3 programs, syscall entry stub,
  `int 0x80` handlers
- `kernel/src/mouse.rs` — PS/2 mouse driver (lock-free state)
- `kernel/src/desktop.rs` — window manager, compositor, wallpaper
  + themes, dock, Start menu, Terminal / Files / Calculator /
  Notes / System Monitor / Settings / Web Browser apps
- `kernel/src/netserial.rs` — COM2 UART driver (browser link)
- `build.rs` — turns the compiled kernel into a bootable BIOS
  disk image (`bios.img`)
- `src/main.rs` — the host-side runner that launches QEMU
  (`-serial stdio`, `-display none`, monitor socket, `isa-debug-exit`,
  attaches `disk.img` as the second IDE drive)
- `scripts/test-shell.py` — end-to-end boot + shell test
- `scripts/test-double-fault.py` — double-fault IST test
- `scripts/test-preemption.py` — preemptive scheduler test
- `scripts/test-fs.py` — filesystem persistence across reboots
- `scripts/test-usermode.py` — ring-3 good/bad program test
- `scripts/test-desktop.py` — fancy desktop screendump + mouse test
- `scripts/test-browser.py` — browser test: starts the net proxy,
  opens the Browser from the dock, checks the live Chimpanzee
  fetch marker + screendump, saves `screenshot-browser.png`
- `scripts/net-proxy.py` — host-side HTTPS proxy for the browser
  (COM2 serial bridge; see Web Browser above)
- `screenshot-browser.png` — QEMU screendump of the Wikipedia
  Chimpanzee page open in the scratch-built browser
- `screenshot.png` — QEMU screendump of the fancy desktop
  (`screenshot-fancy-1.png` .. `screenshot-fancy-4.png` show the
  default desktop, the Start menu, Monitor + Notes, and the
  Sunset theme)
- `disk.img` — 8 MiB RockyFS data disk (created on first run,
  persists across boots; delete it to reformat)

## Known limitations / next steps

1. **Filesystem is a toy** — flat directory, contiguous files,
   deleted sectors are never reused, no subdirectories, no
   permissions, no journaling. A crash mid-write can corrupt it.
2. **User programs are embedded blobs** — the two ring-3 programs
   are assembled into the kernel, not loaded from disk as ELF
   files; there is no process table, and only one user program
   runs at a time (from the shell).
3. **Desktop runs in the kernel** — the window manager and apps
   are kernel code, not user-space processes; a bug in a desktop
   app can still crash the kernel (unlike the ring-3 programs).
   The next step would be moving the desktop into a user-mode
   process on top of the syscall interface.
4. **Keyboard via serial is polling** — the UART IRQ (IRQ 4) is not
   wired up; the shell polls COM1 and wakes on timer ticks.
5. **No networking, no sound, no USB** — and no real hardware
   testing; everything is verified in QEMU only. The one
   exception is the browser, which fetches over a COM2 serial
   link to a host-side proxy — there is still no Ethernet driver,
   TCP/IP, DNS, or TLS inside the kernel.
7. **Browser is text-only over serial** — no images, CSS, or
   JavaScript; pages are capped at 16,000 chars of extracted
   text, and fetching needs `scripts/net-proxy.py` running on
   the host (without it the browser times out after ~10 s with
   a clear error). The bitmap font has no IPA glyphs, so a few
   pronunciation characters render blank.
6. **Desktop is a full redraw, and there is no real clock** — every
   mouse move, keypress, or clock second repaints the whole
   1280×720 screen (~120 ms in a debug build; no dirty-rect
   tracking yet), and the clock widget shows uptime, not a date
   (no RTC driver). The wallpaper orbs are banded approximations
   of a radial glow, not a true blur.
