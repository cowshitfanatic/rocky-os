#!/usr/bin/env python3
"""Test Rocky OS fancy desktop: boot, run `desktop`, take screendumps
via the QEMU monitor, and check the rendered pixels.

Checks: aurora gradient wallpaper (top + bottom), glass top bar,
dock, active/inactive gradient title bars, mouse cursor, Start
menu open/close, Notes launch from the dock + typing, a title-bar
drag moving the Terminal window, Sunset theme from Settings.
Exits with Esc, then `exit` in the text shell (QEMU exit status 33)."""
import os, select, socket, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MON = "/tmp/rocky-mon"
PPM1 = "/tmp/rocky-desktop.ppm"
PPM2 = "/tmp/rocky-desktop2.ppm"
PPM3 = "/tmp/rocky-desktop3.ppm"

env = dict(os.environ)
env["PATH"] = os.path.expanduser("~/.cargo/bin") + ":" + env.get("PATH", "")

for f in (PPM1, PPM2, PPM3):
    try:
        os.remove(f)
    except OSError:
        pass

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


def wait_for_new(needle, timeout=30, start=None):
    """Wait for `needle` to appear in output received after `start`
    (default: from now on — not text already in the buffer)."""
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


def wait_for(needle, timeout=90):
    end = time.time() + timeout
    while time.time() < end:
        drain(0.5)
        if needle.encode() in out:
            return True
        if p.poll() is not None:
            return False
    return False


def mon_cmd(cmd, timeout=20):
    """Send one HMP command, return its output text."""
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
    # P6, header lines, then raw RGB bytes.
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


def click(dx=0, dy=0):
    """Move the mouse by (dx, dy), press and release the left button."""
    if dx or dy:
        mon_cmd(f"mouse_move {dx} {dy}")
        time.sleep(0.5)
    mon_cmd("mouse_button 1")
    time.sleep(0.5)
    mon_cmd("mouse_button 0")
    time.sleep(0.5)


ok = True
def check(cond, msg):
    global ok
    print(("ok: " if cond else "FAIL: ") + msg)
    if not cond:
        ok = False


if not wait_for("rocky> "):
    print("FAIL: no shell prompt")
    sys.exit(1)

p.stdin.write(b"desktop\n")
p.stdin.flush()
check(wait_for("desktop: starting", 30), "desktop started (serial marker)")
time.sleep(3)
mon_cmd(f"screendump {PPM1}")
check(os.path.exists(PPM1), "screendump file created")
w, h, px = read_ppm(PPM1)
check((w, h) == (1280, 720), f"screendump is 1280x720 (got {w}x{h})")
# Aurora wallpaper: indigo top, teal bottom.
check(count_color(px, w, h, (20, 26, 58), 0, 36, 1280, 40, tol=12) > 4000, "wallpaper gradient top (indigo)")
check(count_color(px, w, h, (16, 110, 120), 0, 705, 400, 712, tol=15) > 2000, "wallpaper gradient bottom (teal)")
# Glass top bar (dark, not the old 30,42,56).
check(count_color(px, w, h, (14, 16, 28), 0, 0, 1280, 32, tol=8) > 20000, "top bar present")
# Floating dock pill.
check(count_color(px, w, h, (18, 20, 36), 404, 644, 876, 708, tol=8) > 8000, "dock present")
# Gradient title bars: Terminal active (blue-violet), Files inactive (grey).
check(count_color(px, w, h, (99, 93, 226), 300, 54, 500, 64, tol=25) > 1500, "Terminal title bar (active gradient)")
check(count_color(px, w, h, (85, 92, 112), 900, 54, 1100, 64, tol=20) > 1500, "Files title bar (inactive gradient)")
# The cursor is drawn last in each frame, so a screendump taken
# mid-render can miss it; retry with a fresh dump.
cursor_here = False
for attempt in range(3):
    cursor_here = count_color(px, w, h, (255, 255, 255), 630, 350, 660, 380) > 20
    if cursor_here:
        break
    time.sleep(1.5)
    mon_cmd(f"screendump {PPM1}")
    w, h, px = read_ppm(PPM1)
check(cursor_here, "mouse cursor at start position")
# Clock area (top right) should have white text pixels.
check(count_color(px, w, h, (255, 255, 255), 1100, 4, 1200, 24) > 20, "clock text in top bar")

# Warm-up move: the first packet after mouse init carries a
# phantom +8 dx (a stray byte from init), so absorb it first.
mon_cmd("mouse_move 0 0")
time.sleep(1)

# Open the Start menu by clicking the logo (20,16).
click(-628, -344)
mon_cmd(f"screendump {PPM2}")
w, h, px2 = read_ppm(PPM2)
check(count_color(px2, w, h, (124, 80, 220), 16, 300, 316, 304, tol=12) > 400, "Start menu opened (violet accent)")

# Click the Terminal row (166,362): launches/focuses Terminal and closes the menu.
click(146, 346)
mon_cmd(f"screendump {PPM2}")
w, h, px2 = read_ppm(PPM2)
check(count_color(px2, w, h, (124, 80, 220), 16, 300, 316, 304, tol=12) < 50, "Start menu closed after app click")

# Open Notes from the dock (icon centre 672,672). A screendump can
# catch a render mid-frame, so retry with fresh dumps.
click(464, 310)
notes_open = False
for attempt in range(3):
    mon_cmd(f"screendump {PPM2}")
    w, h, px2 = read_ppm(PPM2)
    notes_open = count_color(px2, w, h, (93, 101, 231), 300, 86, 500, 96, tol=25) > 1000
    if notes_open:
        break
    time.sleep(1.5)
check(notes_open, "Notes window opened from dock")

# Type in Notes: the text should appear in the content area.
p.stdin.write(b"hello")
p.stdin.flush()
time.sleep(1)
typed = False
for attempt in range(3):
    mon_cmd(f"screendump {PPM2}")
    w, h, px2 = read_ppm(PPM2)
    typed = count_color(px2, w, h, (232, 236, 244), 150, 120, 680, 260, tol=12) > 20
    if typed:
        break
    time.sleep(1.5)
check(typed, "typed text appears in Notes")

# Drag the Terminal window by its title bar: press at (200,62),
# move +100,0, release. Title bar should now start near x=140.
mon_cmd("mouse_move -430 -610")  # (630,672) -> (200,62)
time.sleep(0.5)
mon_cmd("mouse_button 1")
time.sleep(0.5)
mon_cmd("mouse_move 100 0")
time.sleep(0.5)
mon_cmd("mouse_button 0")
time.sleep(1)
# Same tearing caveat as above: retry the drag checks with a
# fresh screendump if the first frame is mid-render.
drag_moved = False
drag_cleared = False
cursor_after_drag = False
for attempt in range(3):
    mon_cmd(f"screendump {PPM3}")
    w, h, px3 = read_ppm(PPM3)
    drag_moved = count_color(px3, w, h, (99, 93, 226), 400, 54, 600, 64, tol=25) > 1500
    drag_cleared = count_color(px3, w, h, (99, 93, 226), 40, 54, 140, 64, tol=25) < 50
    cursor_after_drag = count_color(px3, w, h, (255, 255, 255), 295, 58, 315, 80) > 20
    if drag_moved and drag_cleared and cursor_after_drag:
        break
    time.sleep(1.5)
check(drag_moved, "Terminal title bar moved right after drag")
check(drag_cleared, "old title-bar area is wallpaper after drag")
check(cursor_after_drag, "cursor at drag end position")

# Open Settings from the dock (icon centre ~800,672), then click
# the Sunset swatch (centre 625,350).
click(458, 610)
click(-175, -322)
time.sleep(1)
mon_cmd(f"screendump {PPM2}")
w, h, px2 = read_ppm(PPM2)
check(count_color(px2, w, h, (248, 150, 64), 0, 705, 400, 712, tol=15) > 2000, "Sunset theme applied (orange bottom)")
check(count_color(px2, w, h, (46, 14, 52), 0, 36, 1280, 40, tol=15) > 4000, "Sunset theme applied (dark top)")

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

print("PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
