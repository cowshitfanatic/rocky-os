#!/usr/bin/env python3
"""Test Rocky OS user mode: a ring-3 program prints via syscall and
exits, and a bad ring-3 program that reads kernel memory faults
without crashing the kernel. The shell must still work after both."""
import os, select, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
env = dict(os.environ)
env["PATH"] = os.path.expanduser("~/.cargo/bin") + ":" + env.get("PATH", "")

p = subprocess.Popen(
    ["cargo", "run"],
    cwd=ROOT,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    env=env,
)
fd = p.stdout.fileno()
os.set_blocking(fd, False)
out = bytearray()


def drain(seconds):
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                continue
            if not chunk:
                break
            out.extend(chunk)


def wait_for(needle, timeout=90):
    end = time.time() + timeout
    while time.time() < end:
        drain(0.5)
        if needle.encode() in out:
            return True
        if p.poll() is not None:
            return False
    return False


def send(cmd):
    p.stdin.write((cmd + "\n").encode())
    p.stdin.flush()


ok = True
if not wait_for("rocky> "):
    print("FAIL: no shell prompt")
    ok = False

send("user")
if not wait_for("Hello from ring 3!", 30):
    print("FAIL: user program did not print via syscall")
    ok = False
if not wait_for("user program exited with code 0", 30):
    print("FAIL: user program did not exit with code 0")
    ok = False

send("userbad")
if not wait_for("user program faulted as expected", 30):
    print("FAIL: bad user program did not fault as expected")
    ok = False

# The kernel must still be alive: the shell still works.
send("hello")
if not wait_for("Hello from Rocky OS!", 30):
    print("FAIL: shell dead after user program fault")
    ok = False

send("exit")
try:
    rc = p.wait(timeout=30)
except subprocess.TimeoutExpired:
    p.terminate()
    rc = p.wait(timeout=10)
drain(1)
text = out.decode(errors="replace")
if rc != 0 and rc != 33:
    print(f"FAIL: exit status {rc}")
    ok = False
if "EXCEPTION: PAGE FAULT at 0x444444440000" not in text:
    print("FAIL: no ring-3 page fault record for the bad program")
    ok = False

print("PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
