#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Offscreen GUI startup and cancellation of its owned discovery query."""
import argparse
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
from urllib.parse import unquote, urlparse

ROOT = Path(__file__).resolve().parents[1]


def running(pid):
    status = subprocess.run(["ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True, timeout=5)
    return status.returncode == 0 and bool(status.stdout.strip()) and not status.stdout.lstrip().startswith("Z")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall-ui")
    parser.add_argument("--worker", type=Path, default=ROOT / "target/debug/openwall")
    args = parser.parse_args()
    binary, worker = args.binary.resolve(strict=True), args.worker.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="openwall-gui-query-") as temporary:
        directory = Path(temporary)
        runtime = directory / "runtime"
        runtime.mkdir(mode=0o700)
        env = os.environ.copy()
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DBUS_SESSION_BUS_ADDRESS", "SWAYSOCK"]:
            env.pop(key, None)
        env.update(QT_QPA_PLATFORM="offscreen", QT_QUICK_BACKEND="software", QT_QUICK_CONTROLS_STYLE="Basic",
                   QT_FORCE_STDERR_LOGGING="1", QT_LOGGING_RULES="qml.info=true",
                   XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(directory / "config"),
                   XDG_CACHE_HOME=str(directory / "cache"))
        normal = subprocess.run([str(binary), "--check"], env=dict(env, OPENWALL_BIN=str(worker)),
                                capture_output=True, text=True, timeout=15)
        assert normal.returncode == 0, normal.stderr[-16_384:]
        # Exercise the actual GUI selection/worker/PNG path, not only its helpers.
        image = directory / "海 夜.png"
        shutil.copyfile(ROOT / "samples/aurora/preview.png", image)
        broken = directory / "broken.png"
        broken.write_bytes(b"not an image")
        imports = []
        for source, expected_preview in [(ROOT / "samples/aurora/project.json", True),
                                         (image, True), (ROOT / "samples/aurora-packed/project.json", False),
                                         (broken, None)]:
            result = subprocess.run([str(binary), "--check", "--open", str(source)],
                                    env=dict(env, OPENWALL_BIN=str(worker)),
                                    capture_output=True, text=True, timeout=25)
            assert result.returncode == 0, result.stderr[-16_384:]
            reports = [json.loads(line.partition("OPENWALL_GUI_CHECK ")[2])
                       for line in (result.stdout + result.stderr).splitlines() if "OPENWALL_GUI_CHECK " in line]
            assert len(reports) == 1, result.stderr[-16_384:]
            report = reports[0]
            assert not report["busy"], report
            if expected_preview is None:
                assert report["selected"] != str(source) and report["status"] and report["status"] != "Ready", report
            else:
                assert report["selected"] == str(source), report
                assert report["status"] == "Ready", report
                if expected_preview:
                    assert report["image_status"] == 1, report # Qt Image.Ready
                    parsed = urlparse(report["preview_url"])
                    assert parsed.scheme == "file", report
                    generated = Path(unquote(parsed.path))
                    assert generated != source and generated.parent != source.parent, report
                    assert not generated.parent.exists(), "GUI exit left its generated preview directory behind"
                else:
                    assert not report["preview_url"] and report["image_status"] == 0, report
            imports.append(source.name)
        pid_file = directory / "query.pid"
        fake = directory / "hung-worker.py"
        fake.write_text(f"#!{sys.executable}\nimport os, pathlib, signal, time\n"
                        "pathlib.Path(os.environ['OPENWALL_QUERY_PID_FILE']).write_text(str(os.getpid()))\n"
                        "signal.signal(signal.SIGINT, signal.SIG_IGN)\ntime.sleep(60)\n")
        fake.chmod(0o700)
        query_env = dict(env, OPENWALL_BIN=str(fake), OPENWALL_QUERY_PID_FILE=str(pid_file))
        gui = subprocess.Popen([str(binary), "--check"], env=query_env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        started = time.monotonic()
        try:
            _, errors = gui.communicate(timeout=15)
            assert gui.returncode == 0, errors[-16_384:]
            assert pid_file.exists(), "GUI did not start its discovery query"
            pid = int(pid_file.read_text())
            assert not running(pid), "GUI exit left its discovery worker running"
        finally:
            if gui.poll() is None:
                gui.kill()
                gui.communicate(timeout=5)
            if pid_file.exists():
                pid = int(pid_file.read_text())
                command = subprocess.run(["ps", "-p", str(pid), "-o", "command="],
                                         capture_output=True, text=True, timeout=5)
                # Clean up an owned fault fixture if this regression fails; never
                # signal an unrelated process after numeric PID reuse.
                if str(fake) in command.stdout and running(pid):
                    os.kill(pid, signal.SIGKILL)
        print(json.dumps({"gui_queries":"passed", "hung_query_gui_seconds":time.monotonic()-started,
                          "imports":imports, "checks":["offscreen startup", "real file/scene inspection", "Qt loads generated PNG", "missing preview keeps selection", "malformed image keeps previous selection", "generated preview cleanup", "discovery worker started", "GUI exit cancels and reaps query"]}))


if __name__ == "__main__":
    main()
