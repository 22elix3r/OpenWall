#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Validate EGL surface ownership and real click-through in a private compositor."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

from test_wayland_headless import Session, wait_for

ROOT = Path(__file__).resolve().parents[1]


def cpu_seconds(pid):
    # Linux procfs fields 14/15; the command name can contain spaces or parentheses.
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/owned_egl_wayland_probe")
    parser.add_argument("--gl-api", choices=["gles", "desktop"], default="gles")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    sway = shutil.which("sway")
    assert sway and shutil.which("swaymsg") and shutil.which("grim"), "Sway, swaymsg and grim are required"
    mesa = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    assert mesa.is_file(), "Mesa EGL configuration is required"
    # This must reject before connecting to any inherited desktop display.
    guard_env = os.environ.copy()
    guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    guard = subprocess.run([str(binary), "--output", "never-connect"], env=guard_env,
                           capture_output=True, text=True, timeout=5)
    assert guard.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guard.stderr, guard
    assert not guard.stdout.strip()
    with tempfile.TemporaryDirectory(prefix="openwall-egl-wayland-") as temporary:
        session = Session(Path(temporary), sway, software_vulkan=False)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="1", __EGL_VENDOR_LIBRARY_FILENAMES=str(mesa))
        try:
            session.connect()
            output = sorted(item["name"] for item in session.outputs() if item["active"])[0]
            baseline = wait_for(lambda: session.capture(output), "initial private compositor frame").getpixel((10, 10))

            def spawn(extra=()):
                log = (session.directory / f"egl-{len(session.logs)}.log").open("w+")
                session.logs.append(log)
                child = subprocess.Popen([str(binary), "--gl-api", args.gl_api, "--output", output, "--seconds", "30", *extra],
                                         env=session.env, stdout=subprocess.PIPE, stderr=log, text=True)
                session.workers.append(child)
                return child, log

            def empty():
                image = session.capture(output)
                return image is not None and all(
                    max(abs(a - b) for a, b in zip(image.getpixel((int(image.width * x), int(image.height * y))), baseline)) <= 2
                    for x, y in [(.25, .25), (.75, .25), (.25, .75), (.75, .75)])

            child, log = spawn()
            session.expect_log(child, log, "Owned EGL presented; pointer reached underlying witness")
            session.expect_frame(child, output)
            before = cpu_seconds(child.pid)
            submitted = session.text(log).count("Owned EGL submitted")
            time.sleep(1)
            idle_cpu = cpu_seconds(child.pid) - before
            assert idle_cpu < .25, ("idle event loop consumed excessive CPU", idle_cpu, session.diagnostics())
            assert session.text(log).count("Owned EGL submitted") == submitted, session.diagnostics()
            clicks = 2
            for command in [f"output {output} mode 800x500", f"output {output} scale 1.25",
                            f"output {output} scale 2", f"output {output} transform 90"]:
                mark = len(session.text(log))
                session.ipc(command)
                clicks += 2
                session.expect_log(child, log, f"Private EGL click-through confirmed ({clicks} button events)", after=mark)
                session.expect_frame(child, output)
            child.send_signal(signal.SIGINT)
            stdout, _ = child.communicate(timeout=10)
            assert child.returncode == 0, session.diagnostics()
            report = json.loads(stdout)
            assert report["status"] == "passed" and report["scope"] == "owned_egl_surface_in_private_compositor", report
            assert report["egl_context_creations"] == 1 and report["presented"] >= 5, report
            assert report["egl_lifecycle"] == {"display_initializations": 1, "context_creations": 1, "surface_creations": 1,
                                               "surface_destructions": 1, "context_destructions": 1, "display_terminations": 1}, report
            assert report["witness_button_events"] >= clicks and report["input_region"] == "empty", report
            assert "llvmpipe" in report["gl"]["renderer"].lower(), report
            assert report["gl"]["api"] == args.gl_api, report
            wait_for(empty, "EGL and witness surfaces removed after SIGINT")

            # A negative control proves that real pointer hit-testing, rather than
            # just an input-region protocol request, determines the test result.
            control, _ = spawn(["--intercept-input"])
            stdout, _ = control.communicate(timeout=15)
            assert control.returncode != 0 and not stdout.strip(), session.diagnostics()
            assert "test pointer was intercepted by EGL surface" in session.text(session.logs[-1]), session.diagnostics()
            wait_for(empty, "negative-control surfaces removed")

            crashed, log = spawn()
            session.expect_log(crashed, log, "Owned EGL presented; pointer reached underlying witness")
            session.expect_frame(crashed, output)
            crashed.kill()
            stdout, _ = crashed.communicate(timeout=10)
            assert crashed.returncode == -signal.SIGKILL and not stdout.strip(), stdout
            wait_for(empty, "EGL and witness surfaces removed after SIGKILL")

            lost, log = spawn()
            session.expect_log(lost, log, "Owned EGL presented; pointer reached underlying witness")
            session.process.terminate()
            session.process.wait(timeout=10)
            stdout, _ = lost.communicate(timeout=10)
            assert lost.returncode != 0 and not stdout.strip(), session.diagnostics()
        except Exception:
            print(session.diagnostics())
            raise
        finally:
            session.close()
    print(json.dumps({"owned_egl_wayland": "passed", "idle_cpu_seconds_over_one_second": idle_cpu,
                      "checks": ["compositor pixels", "presentation feedback", "real click-through after resize/scale/rotation",
                                 "one retained EGL context", "idle sleep", "opaque-input negative control", "SIGINT", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
