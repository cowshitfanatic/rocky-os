#!/usr/bin/env python3
"""Preemption test for Rocky OS: boot in QEMU and prove the scheduler
is truly preemptive, not just cooperative.

The `hog` demo task spins for ~2 s and never yields. If scheduling
were still cooperative, all of `counter`/`printer`'s steps would
appear either before `hog` starts or after it finishes. With
preemption, their steps interleave with `hog`'s progress lines.

Usage: python3 scripts/test-preemption.py   (from the project root)
"""

import os
import re
import select
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMMANDS = ["tasks", "exit"]

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
lines = [l for l in text.splitlines() if not l.startswith("INFO") and l.strip()]

# Positions of hog progress lines vs counter/printer steps.
hog_pos = [i for i, l in enumerate(lines) if "task hog: still running" in l]
step_pos = [
    i
    for i, l in enumerate(lines)
    if "task counter: step" in l or "task printer: step" in l
]
if len(hog_pos) < 2:
    print(f"FAIL: expected >=2 hog progress lines, saw {len(hog_pos)}")
    ok = False
elif not any(hog_pos[0] < p < hog_pos[-1] for p in step_pos):
    print("FAIL: no counter/printer step ran between hog progress lines")
    print("  (that is what cooperative scheduling would look like)")
    ok = False
else:
    interleaved = sum(1 for p in step_pos if hog_pos[0] < p < hog_pos[-1])
    print(f"interleaved steps between hog progress lines: {interleaved}")

m = re.search(r"scheduler: demo done \((\d+) preemptions\)", text)
if not m or int(m.group(1)) == 0:
    print("FAIL: demo reported no preemptions")
    ok = False
else:
    print(f"preemptions during demo: {m.group(1)}")

for needle in [
    "task hog: done after 36 ticks",
    "counter Finished steps 5",
    "printer Finished steps 5",
    "hog Finished steps ",
    "Bye!",
    "QEMU exited with: exit status: 33",
]:
    if needle not in text:
        print(f"FAIL: missing expected output: {needle!r}")
        ok = False

print("PASS" if ok else "FAIL")
print("--- kernel/serial output (INFO lines stripped) ---")
for line in lines:
    print(line)
sys.exit(0 if ok else 1)
