#!/usr/bin/env python3
"""Double-fault test for Rocky OS: boot, run the `dblfault` shell
command, and check the double-fault handler runs on its IST stack.

The handler prints "EXCEPTION: DOUBLE FAULT" and exits QEMU with
the failure code (QEMU exit status 35). If the IST stack were not
set up, the CPU would triple-fault instead and QEMU would reset /
hang rather than exiting cleanly.

Usage: python3 scripts/test-double-fault.py   (from the project root)
"""

import os
import select
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

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
    proc.stdin.write(b"dblfault\n")
    proc.stdin.flush()

try:
    proc.wait(timeout=20)
except subprocess.TimeoutExpired:
    print("FAIL: QEMU did not exit (probable triple fault)")
    proc.terminate()
    ok = False
drain(1)

text = output.decode(errors="replace")
for needle in [
    "gdt loaded, double-fault stack ready",
    "EXCEPTION: DOUBLE FAULT",
    # QEMU exit status 35 = our failure code 0x11, i.e. the
    # double-fault handler ran and exited cleanly (not a triple
    # fault / reset). Note: this is the exit status printed by the
    # runner, not cargo's own exit code, which is always 0.
    "QEMU exited with: exit status: 35",
]:
    if needle not in text:
        print(f"FAIL: missing expected output: {needle!r}")
        ok = False

print("PASS" if ok else "FAIL")
print("--- kernel/serial output (INFO lines stripped) ---")
for line in text.splitlines():
    if not line.startswith("INFO") and line.strip():
        print(line)
sys.exit(0 if ok else 1)
