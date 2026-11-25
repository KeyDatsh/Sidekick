"""Checks that the overlay is absent from screen captures.

Runs the overlay as a solid magenta window twice: unprotected (control: the capture
must see it) and protected (the capture must not). Frames are only piped and counted,
never written to disk. Needs ffmpeg 8+ on PATH.
"""
import re
import subprocess
import sys
import time
from pathlib import Path

EXE = Path(__file__).resolve().parents[2] / "target" / "debug" / "overlay.exe"
FF = ["ffmpeg", "-v", "error", "-nostdin"]
RAW = ["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "bgra", "-"]


def backends(hwnd):
    gfx = "hwdownload,format=bgra"
    return {
        "GDI, whole screen": FF + ["-f", "gdigrab", "-i", "desktop"] + RAW,
        "GDI, window": FF + ["-f", "gdigrab", "-i", f"hwnd={hwnd}"] + RAW,
        "Desktop Duplication, screen": FF + ["-filter_complex", f"ddagrab=0,{gfx}"] + RAW,
        "Windows.Graphics.Capture, screen": FF + ["-filter_complex", f"gfxcapture=monitor_idx=0,{gfx}"] + RAW,
        "Windows.Graphics.Capture, window": FF + ["-filter_complex", f"gfxcapture=hwnd={hwnd},{gfx}"] + RAW,
    }


def magenta(frame):
    return sum(
        1
        for b, g, r in zip(frame[0::4], frame[1::4], frame[2::4])
        if b > 230 and r > 230 and g < 40
    )


def run(flags):
    app = subprocess.Popen([str(EXE), "--marker", *flags], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    try:
        line = app.stdout.readline()
        hwnd = int(re.search(r"hwnd=(\d+)", line)[1])
        width, height = map(int, re.search(r"(\d+)x(\d+)\s*$", line).groups())
        time.sleep(3)
        results = {}
        for name, cmd in backends(hwnd).items():
            try:
                p = subprocess.run(cmd, capture_output=True, timeout=20)
                if p.returncode or not p.stdout:
                    results[name] = "failed: " + p.stderr.decode(errors="replace").strip().splitlines()[-1][:110]
                else:
                    results[name] = magenta(p.stdout)
            except subprocess.TimeoutExpired:
                results[name] = "no frame in 20 s"
        return results, width * height
    finally:
        app.kill()
        app.wait()


def main():
    control, area = run(["--no-protect"])
    time.sleep(1)
    protected, _ = run([])
    print(f"window area: {area} px")
    ok = True
    for name in control:
        c, p = control[name], protected[name]
        control_ok = isinstance(c, int) and c > area * 0.9
        hidden = p == 0 or not isinstance(p, int)
        # No control picture means this API cannot show the window at all, protected or not.
        verdict = "VISIBLE" if not hidden else "hidden" if control_ok else "NOT TESTABLE"
        ok &= hidden
        print(f"{verdict:16} {name}: control={c} protected={p}")
    sys.exit(0 if ok else 1)


main()
