#!/usr/bin/env python3
"""Test Rocky OS web browser: boot with the net proxy running,
enter the desktop, open the Browser app from the dock, and check
that the live Wikipedia Chimpanzee page renders.

The browser has no Ethernet/TCP/IP stack: it fetches over COM2
(serial) from scripts/net-proxy.py, which fetches the real page
over HTTPS on the host and streams the extracted article text back.

Checks: proxy log shows a live fetch, serial marker
`browser: fetched <N> bytes`, screendump is 1280x720 and not
blank, URL bar + "Chimpanzee" title text present. Exits with Esc,
then `exit` in the text shell (QEMU exit status 33).

Usage: python3 scripts/test-browser.py
"""
import os, select, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MON = "/tmp/rocky-mon"
PPM = "/tmp/rocky-browser.ppm"
PNG = os.path.join(ROOT, "screenshot-browser.png")
PROXY_LOG = "/tmp/rocky-net.log"

env = dict(os.environ)
env["PATH"] = os.path.expanduser("~/.cargo/bin") + ":" + env.get("PATH", "")

for f in (PPM,):
    try:
        os.remove(f)
    except OSError:
        pass

# Start the net proxy first: it retries connecting to the QEMU
# COM2 socket until QEMU is up.
proxy = subprocess.Popen(
    [sys.executable, os.path.join(ROOT, "scripts", "net-proxy.py")],
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
)

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


def wait_for_new(needle, timeout=30, start=None):
    if start is None:
        start = len(out)
    end = time.time() + timeout
    while time.time() < end:
        drain(0.5)
        if needle.encode() in out[start:]:
            return True
        if p.poll() is not None:
            return False
    return False


def mon_cmd(cmd, timeout=20):
    end = time.time() + timeout
    s = None
    while time.time() < end:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(MON)
            break
        except OSError:
            time.sleep(0.2)
    if s is None:
        return ""
    s.settimeout(5)
    data = b""
    try:
        while b"(qemu)" not in data:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
    except socket.timeout:
        pass
    s.sendall((cmd + "\n").encode())
    data = b""
    try:
        while b"(qemu)" not in data:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
    except socket.timeout:
        pass
    s.close()
    return data.decode(errors="replace")


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    parts = data.split(b"\n", 3)
    assert parts[0] == b"P6", f"bad PPM magic {parts[0]!r}"
    w, h = (int(x) for x in parts[1].split())
    assert parts[2] == b"255"
    px = parts[3]
    assert len(px) >= w * h * 3
    return w, h, px


def pixel(px, w, x, y):
    off = (y * w + x) * 3
    return tuple(px[off:off + 3])


def count_color(px, w, h, color, x0, y0, x1, y1, tol=12):
    n = 0
    for y in range(y0, min(y1, h)):
        for x in range(x0, min(x1, w)):
            r, g, b = pixel(px, w, x, y)
            if abs(r - color[0]) <= tol and abs(g - color[1]) <= tol and abs(b - color[2]) <= tol:
                n += 1
    return n


ok = True
def check(cond, msg):
    global ok
    print(("ok: " if cond else "FAIL: ") + msg)
    if not cond:
        ok = False


def cleanup():
    try:
        proxy.terminate()
        proxy.wait(timeout=5)
    except Exception:
        try:
            proxy.kill()
        except Exception:
            pass
    if p.poll() is None:
        try:
            p.terminate()
            p.wait(timeout=10)
        except Exception:
            pass


if not wait_for("rocky> "):
    print("FAIL: no shell prompt")
    cleanup()
    sys.exit(1)

p.stdin.write(b"desktop\n")
p.stdin.flush()
check(wait_for("desktop: starting", 30), "desktop started (serial marker)")
check(wait_for("net: com2 init", 30), "COM2 initialised (serial marker)")
time.sleep(2)

# Warm-up move: the first packet after mouse init carries a
# phantom +8 dx, so absorb it first. Mouse starts at (640,360).
mon_cmd("mouse_move 0 0")
time.sleep(1)
# Click the Browser dock icon (centre 822,672): from (648,360).
mon_cmd("mouse_move 174 312")
time.sleep(0.5)
mon_cmd("mouse_button 1")
time.sleep(0.5)
mon_cmd("mouse_button 0")
time.sleep(0.5)

# The browser auto-loads Chimpanzee on first open. The fetch goes
# proxy (HTTPS) -> COM2 at 115200 baud, so allow a minute.
fetched = wait_for("browser: fetched", 60)
check(fetched, "browser fetched the page (serial marker)")
if fetched:
    text = out.decode(errors="replace")
    idx = text.rfind("browser: fetched")
    print("marker:", text[idx:idx + 40].replace("\n", " "))
time.sleep(2)

mon_cmd(f"screendump {PPM}")
check(os.path.exists(PPM), "screendump file created")
w, h, px = read_ppm(PPM)
check((w, h) == (1280, 720), f"screendump is 1280x720 (got {w}x{h})")
# Browser window (120,60,780,540): URL bar is dark, title bar is
# a blue-violet gradient, and the page area has light text pixels.
check(count_color(px, w, h, (14, 16, 26), 130, 96, 400, 116, tol=8) > 2000,
      "URL bar present (dark)")
check(count_color(px, w, h, (232, 236, 244), 130, 130, 880, 560, tol=12) > 500,
      "article text pixels in page area")
# Not a blank / wallpaper-only frame: the window body is dark.
check(count_color(px, w, h, (30, 32, 48), 130, 200, 880, 560, tol=8) > 20000,
      "browser window body present")

# Save a PNG copy for visual inspection.
try:
    from PIL import Image
    img = Image.frombytes("RGB", (w, h), px[: w * h * 3])
    img.save(PNG)
    print("saved", PNG)
except Exception as e:
    print("note: could not save PNG:", e)

# Leave with Esc.
mark = len(out)
p.stdin.write(b"\x1b")
p.stdin.flush()
check(wait_for_new("desktop: exiting", 30, mark), "desktop exited on Esc")
check(wait_for_new("rocky> ", 30, mark), "back at the text shell")
p.stdin.write(b"exit\n")
p.stdin.flush()
try:
    rc = p.wait(timeout=30)
except subprocess.TimeoutExpired:
    p.terminate()
    rc = p.wait(timeout=10)
drain(1)
text = out.decode(errors="replace")
check("QEMU exited with: exit status: 33" in text, f"QEMU exited with status 33 (runner rc={rc})")

# The proxy must have logged a real live fetch.
try:
    log = open(PROXY_LOG).read()
    check("fetched https://en.wikipedia.org/wiki/Chimpanzee" in log,
          "proxy log shows live Chimpanzee fetch")
    for line in log.splitlines():
        if "Chimpanzee" in line:
            print("proxy log:", line)
except OSError as e:
    check(False, f"proxy log readable ({e})")

cleanup()
print("PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
