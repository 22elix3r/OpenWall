#!/usr/bin/env python3
"""Opt-in real Hyprland lifecycle test; affects only temporary OpenWall surfaces."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
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
    parser.add_argument("--video-fixture", type=Path, help="optional original looping video no longer than two seconds")
    args = parser.parse_args()
    if surfaces():
        raise SystemExit("An OpenWall surface already exists; stop it before running this test.")
    diagnostics_file = tempfile.TemporaryFile(mode="w+")
    child = subprocess.Popen([str(args.binary), "apply", str(ROOT / "samples/aurora/project.json"), "--seconds", "20"], stdout=diagnostics_file, stderr=diagnostics_file)
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
        diagnostic_size = os.fstat(diagnostics_file.fileno()).st_size
        before, started = ticks(child.pid), time.monotonic()
        time.sleep(3)
        cpu = (ticks(child.pid) - before) / os.sysconf("SC_CLK_TCK") / (time.monotonic() - started) * 100
        rss = int(Path(f"/proc/{child.pid}/statm").read_text().split()[1]) * os.sysconf("SC_PAGE_SIZE")
        idle_diagnostic_size = os.fstat(diagnostics_file.fileno()).st_size
        child.send_signal(signal.SIGINT)
        child.wait(timeout=5)
        diagnostics_file.seek(0)
        diagnostics = diagnostics_file.read(16384)
        assert child.returncode == 0, diagnostics
        assert idle_diagnostic_size == diagnostic_size, "static worker kept presenting while idle: " + diagnostics
        assert cpu < 5, f"static worker used {cpu:.2f}% of a CPU core while idle"
        # Hyprland retains a closing surface while its configured fade-out runs.
        cleanup_started = time.monotonic()
        while surfaces() and time.monotonic() - cleanup_started < 3:
            time.sleep(.05)
        assert not surfaces(), "owned surfaces survived worker shutdown"
        print(json.dumps({"surface_cleanup": "passed", "idle_cpu_percent_one_core": round(cpu, 3), "rss_mib": round(rss / 1024**2, 1), "interval_seconds": 3, "diagnostics": diagnostics}, indent=2))
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        diagnostics_file.close()
    if args.video_fixture:
        video = subprocess.run([str(args.binary), "apply", str(args.video_fixture), "--seconds", "7"], capture_output=True, timeout=15)
        diagnostics = video.stderr.decode(errors="replace")
        assert video.returncode == 0, diagnostics
        assert "no scene GPU initialized" in diagnostics, diagnostics
        assert "First video buffer:" in diagnostics, "native sink never received video"
        assert diagnostics.count("Video loop restarted") >= 2, diagnostics
        until = time.monotonic() + 3
        while surfaces() and time.monotonic() < until:
            time.sleep(.05)
        assert not surfaces(), "video surfaces survived worker shutdown"
        assert diagnostics.count("Native video attached") <= 3, "video parent kept reconfiguring while unchanged"
        print(json.dumps({"native_video": "passed", "loops": diagnostics.count("Video loop restarted"), "scene_gpu_initialized": False, "diagnostics": diagnostics[:16384]}, indent=2))


if __name__ == "__main__":
    main()
