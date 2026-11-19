# Next step (proposed, not started): ELF loader for Rocky OS

Status: plan only. Trevor has not authorized this yet — it was offered
2026-11-05 and is still awaiting his call (ELF loader vs. desktop in
user mode vs. stop at the desktop). Nothing below has been implemented.

## Why this step, in order

Today a "user program" is an assembly blob embedded in
`kernel/src/usermode.rs` (`user_prog_start` / `user_bad_start`),
copied to fixed pages at `0x400000`. An ELF loader lets real,
separately-built programs run from RockyFS instead. It is also the
prerequisite for the other option (desktop in user mode), so it comes
first either way.

## What exists today (verified 2026-11-06 from the code)

- Ring 3 entry via `iretq`, user CS/SS from GDT, stack top
  `0x802000` (2 pages, `0x800000..0x802000`); syscalls via
  `int 0x80`: 0 exit, 1 write, 2 yield, 3 ticks
  (`kernel/src/usermode.rs`).
- User-accessible range enforced by the write syscall:
  `0x400000..0x900000`. Only 2 code pages are mapped today
  (`0x400000`, `0x401000`).
- RockyFS (`kernel/src/fs.rs`): 512-byte sectors, 8 MiB disk,
  sector 0 superblock (`"ROCKYFS1"`), sectors 1..17 directory
  (128 entries x 64 bytes: name[32], size u32, start_sector u32,
  used u8), data from sector 17, bump allocation, deleted files'
  sectors are never reused. `read_file` / `write_file` already exist
  in-kernel; `disk.img` persists across reboots.

## The loader (kernel side, new module e.g. `kernel/src/elf.rs`)

1. `read_file(name)` the ELF bytes from RockyFS into the heap.
2. Validate: `0x7F 'E' 'L' 'F'`, ELF64, little-endian,
   `e_machine == EM_X86_64` (62), `e_type` ET_EXEC (2) to start
   (ET_DYN later, if position-independent loading is wanted).
3. Walk program headers; for each `PT_LOAD`: map one 4 KiB page
   per page spanned (replacing the current fixed 2-page mapping —
   map dynamically, still inside `0x400000..0x900000`), copy
   `p_filesz` bytes, zero-fill to `p_memsz` (BSS).
4. Reject: segments outside the user range, overlapping the stack
   region, `p_memsz < p_filesz`, or an entry point outside a loaded
   segment.
5. Enter at `e_entry` with the existing `run_user(entry)` path and
   stack at `0x802000`. Syscall ABI unchanged (0–3).
6. Shell: add a `run <name>` command (in `kernel/src/shell.rs`)
   that loads and runs a RockyFS file, reporting Exited(n) /
   Faulted the way the current `usermode` test does.

## The wrinkle, solved: getting an ELF onto the disk

There is no in-kernel way to receive a binary yet, so this step
needs a host-side injector — it does **not** need a new on-disk
format:

- A small host tool (Rust binary in `tools/` or a Python script)
  that opens `disk.img` directly and implements the RockyFS layout
  above: read the superblock, find a free directory entry, append
  the file's bytes at the bump pointer (sector-aligned), write the
  entry. It must mirror `fs.rs` exactly — same magic, same 64-byte
  entries, same `DATA_START = 17` — and must never reuse sectors
  of deleted files, matching the kernel's limitation.
- Build flow: compile a tiny freestanding test program (no libc,
  raw `int 0x80` syscalls, linked at `0x400000`), inject it as
  e.g. `hello.elf`, boot, `run hello.elf`.
- Keep the embedded demo blobs until the ELF path is proven, so
  `test-usermode.py` keeps passing throughout.

## Test plan (new `scripts/test-elf.py`)

1. Build + inject a hello-world ELF, boot, `run hello.elf`,
   assert its `write` output appears and it exits 0.
2. Inject a faulting ELF (read from a kernel address), assert the
   kernel survives and reports Faulted.
3. Re-run all six existing tests — no regressions.

## Estimated shape

Small: one new kernel module (~150–250 lines), the injector
(~100 lines host-side), one shell command, one test script.
No changes to the scheduler, GDT/TSS, desktop, or syscall ABI.

## Decision needed from Trevor

- Go with the ELF loader (recommended), or
- skip to desktop-in-user-mode (needs the loader first anyway), or
- leave Rocky OS at the desktop.
