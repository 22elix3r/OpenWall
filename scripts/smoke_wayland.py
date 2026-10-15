#!/usr/bin/env python3
"""Opt-in real Hyprland lifecycle test; affects only temporary OpenWall surfaces."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def surfaces():
    layers = json.loads(subprocess.check_output(["hyprctl", "-j", "layers"]))
    return [(monitor, level, layer) for monitor, data in layers.items()
            for level, items in data.get("levels", {}).items() for layer in items
            if layer.get("namespace") == "openwall"]


def ticks(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().split()
    return int(fields[13]) + int(fields[14])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall")
    args = parser.parse_args()
    if surfaces():
        raise SystemExit("An OpenWall surface already exists; stop it before running this test.")
    child = subprocess.Popen([str(args.binary), "apply", str(ROOT / "samples/aurora/project.json"), "--seconds", "20"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        until = time.monotonic() + 10
        while time.monotonic() < until and child.poll() is None:
            owned = surfaces()
            if owned and all(layer.get("w", 0) > 0 and layer.get("h", 0) > 0 for _, _, layer in owned):
                break
            time.sleep(.1)
        else:
            raise AssertionError("wallpaper did not create a configured background surface")
        assert all(str(level) == "0" for _, level, _ in owned), owned
        time.sleep(1)
        before, started = ticks(child.pid), time.monotonic()
        time.sleep(3)
        cpu = (ticks(child.pid) - before) / os.sysconf("SC_CLK_TCK") / (time.monotonic() - started) * 100
        rss = int(Path(f"/proc/{child.pid}/statm").read_text().split()[1]) * os.sysconf("SC_PAGE_SIZE")
        child.send_signal(signal.SIGINT)
        _, diagnostics = child.communicate(timeout=5)
        assert child.returncode == 0, diagnostics.decode(errors="replace")
        # Hyprland retains a closing surface while its configured fade-out runs.
        cleanup_started = time.monotonic()
        while surfaces() and time.monotonic() - cleanup_started < 3:
            time.sleep(.05)
        assert not surfaces(), "owned surfaces survived worker shutdown"
        print(json.dumps({"surface_cleanup": "passed", "idle_cpu_percent_one_core": round(cpu, 3), "rss_mib": round(rss / 1024**2, 1), "interval_seconds": 3, "diagnostics": diagnostics.decode()}, indent=2))
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()


if __name__ == "__main__":
    main()
