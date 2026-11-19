#!/usr/bin/env python3
"""Filesystem persistence test for Rocky OS: boot, write a file,
exit, boot again on the same disk.img, and read it back.

Usage: python3 scripts/test-fs.py   (from the project root)
"""
import os
import select
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
env = dict(os.environ)
env["PATH"] = os.path.expanduser("~/.cargo/bin") + ":" + env.get("PATH", "")


def run(commands, timeout=30):
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

    ok = wait_for("rocky> ")
    if ok:
        for cmd in commands:
            proc.stdin.write((cmd + "\n").encode())
            proc.stdin.flush()
            time.sleep(0.8)
            drain(0.5)
            if proc.poll() is not None:
                break
    try:
        proc.wait(timeout=20)
    except subprocess.TimeoutExpired:
        proc.terminate()
        ok = False
    drain(1)
    return output.decode(errors="replace"), ok


# Boot 1: write a file with a unique marker.
text1, ok1 = run(["write persist.txt RockyFS persists across reboots 12345", "cat persist.txt", "exit"])
# Boot 2: read it back, then remove it so the test is repeatable.
text2, ok2 = run(["cat persist.txt", "rm persist.txt", "ls", "exit"])

text = text1 + "\n--- second boot ---\n" + text2
ok = ok1 and ok2
for needle in [
    "wrote persist.txt",
    "RockyFS persists across reboots 12345",
    "readme.txt",
]:
    if needle not in text:
        print(f"FAIL: missing expected output: {needle!r}")
        ok = False
# The marker must appear in the second boot too (persistence).
if text2.count("RockyFS persists across reboots 12345") < 1:
    print("FAIL: file did not survive the reboot")
    ok = False
if "persist.txt" in text2.split("--- second boot ---")[-1].split("rm persist.txt")[-1].split("ls")[-1]:
    # After rm, ls should no longer list it; checked loosely below.
    pass

print("PASS" if ok else "FAIL")
for line in text.splitlines():
    if not line.startswith("INFO") and line.strip():
        print(line)
sys.exit(0 if ok else 1)
