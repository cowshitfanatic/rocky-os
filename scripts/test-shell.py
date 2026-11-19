#!/usr/bin/env python3
"""End-to-end test for Rocky OS: boot in QEMU, drive the shell over
serial input, and check the expected output.

Usage: python3 scripts/test-shell.py   (from the project root)

Why slow input: QEMU's stdio chardev pushes host input into the UART
before the kernel has initialised it, so an instant `printf ... | cargo
run` burst can lose the first commands. Typing with a short pause
between lines (what a human does) is reliable, so this script feeds
one line at a time and waits for the shell prompt in between.
"""

import os
import select
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMMANDS = ["help", "hello", "mem", "ticks", "tasks", "ls", "cat readme.txt", "cat hello.txt", "exit"]
EXPECTED = [
    "Hello from Rocky OS!",
    "kernel booted, 10 memory regions reported",
    "heap works: Rocky OS can allocate!",
    "vec squares: [0, 1, 4, 9, 16, 25, 36, 49, 64, 81]",
    "gdt loaded, double-fault stack ready",
    "EXCEPTION: BREAKPOINT",
    "breakpoint handled, continuing",
    "scheduler: starting demo — 3 tasks, round-robin on timer ticks",
    "task counter: step 0 (value 0)",
    "task printer: step 0",
    "task counter: step 4 (value 4)",
    "task printer: step 4",
    "task hog: still running",
    "task hog: done after 36 ticks",
    "scheduler: demo done",
    "commands: help, hello, mem, ticks, tasks, ls, cat <file>, write <file> <text>, rm <file>, user, userbad, desktop, clear, exit, dblfault",
    "readme.txt",
    "hello.txt",
    "Welcome to Rocky OS!",
    "Hello from the Rocky OS filesystem!",
    "Hello, Trevor! Rocky OS is alive.",
    "10 memory regions reported, heap is 100 KiB",
    "timer ticks since boot:",
    "scheduler: preemptive round-robin, quantum 3 ticks",
    "counter Finished steps 5",
    "printer Finished steps 5",
    "hog Finished steps ",
    "heartbeat Ready steps ",
    "Bye!",
    "QEMU exited with: exit status: 33",
]

env = dict(os.environ)
env["PATH"] = os.path.expanduser("~/.cargo/bin") + ":" + env.get("PATH", "")

proc = subprocess.Popen(
    ["cargo", "run"],
    cwd=ROOT,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    env=env,
)
fd = proc.stdout.fileno()
os.set_blocking(fd, False)
output = bytearray()


def drain(seconds):
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except BlockingIOError:
                continue
            if not chunk:
                break
            output.extend(chunk)


def wait_for(needle, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        drain(0.5)
        if needle.encode() in output:
            return True
        if proc.poll() is not None:
            return needle.encode() in output
    return False


ok = True
if not wait_for("rocky> "):
    print("FAIL: shell prompt never appeared")
    ok = False
else:
    for cmd in COMMANDS:
        proc.stdin.write((cmd + "\n").encode())
        proc.stdin.flush()
        time.sleep(0.8)
        drain(0.5)
        if proc.poll() is not None:
            break

try:
    proc.wait(timeout=20)
except subprocess.TimeoutExpired:
    print("FAIL: QEMU did not exit")
    proc.terminate()
    ok = False
drain(1)

text = output.decode(errors="replace")
for needle in EXPECTED:
    if needle not in text:
        print(f"FAIL: missing expected output: {needle!r}")
        ok = False

print("PASS" if ok else "FAIL")
print("--- kernel/serial output (INFO lines stripped) ---")
for line in text.splitlines():
    if not line.startswith("INFO") and line.strip():
        print(line)
sys.exit(0 if ok else 1)
