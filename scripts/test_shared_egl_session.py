#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Two private outputs retain one EGL session through native surface teardown."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

from private_gpu import nvidia_renderer
from test_owned_egl_wayland import cpu_seconds
from test_wayland_headless import Session, wait_for

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/shared_egl_session_probe")
    parser.add_argument("--gl-api", choices=["gles", "desktop"], default="desktop")
    parser.add_argument("--nvidia", action="store_true", help="explicitly use an existing NVIDIA GPU in the private headless compositor")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--artifact-directory", type=Path, help="save private screenshots and logs on failure to a new directory")
    args = parser.parse_args()
    if args.render_node and not args.nvidia:
        parser.error("--render-node requires --nvidia")
    binary = args.binary.resolve(strict=True)
    sway = shutil.which("sway")
    assert sway and shutil.which("swaymsg") and shutil.which("grim")
    vendor = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    node, render_env = None, None
    if args.nvidia:
        vendor, node, render_env = nvidia_renderer(args.render_node)
    assert vendor.is_file(), "the selected EGL vendor configuration is required"
    guard_env = os.environ.copy()
    guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    guard = subprocess.run([str(binary), "--first", "never-first", "--second", "never-second"], env=guard_env,
                           capture_output=True, text=True, timeout=5)
    assert guard.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guard.stderr and not guard.stdout.strip(), guard
    with tempfile.TemporaryDirectory(prefix="openwall-shared-egl-") as temporary:
        session = Session(Path(temporary), sway, software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor))
        try:
            session.connect()
            first, second = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(first), "private compositor baseline").getpixel((10, 10))
            outputs = {}

            def spawn():
                log = (session.directory / f"shared-{len(session.logs)}.log").open("w+")
                session.logs.append(log)
                output = (session.directory / f"shared-{len(session.logs)}.json").open("w+")
                session.logs.append(output)
                child = subprocess.Popen([str(binary), "--first", first, "--second", second, "--gl-api", args.gl_api],
                                         env=session.env, stdout=output, stderr=log, text=True)
                session.workers.append(child)
                outputs[child.pid] = output
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(outputs[child.pid])

            def empty(name):
                image = session.capture(name)
                return image is not None and all(
                    max(abs(a - b) for a, b in zip(image.getpixel((int(image.width * x), int(image.height * y))), baseline)) <= 2
                    for x, y in [(.25, .25), (.75, .25), (.25, .75), (.75, .75)])

            child, log = spawn()
            session.expect_log(child, log, "Shared EGL presented on both private outputs")
            session.expect_frame(child, first)
            session.expect_frame(child, second)
            before = cpu_seconds(child.pid)
            mark = session.text(log)
            time.sleep(1)
            idle_cpu = cpu_seconds(child.pid) - before
            assert idle_cpu < .25 and session.text(log) == mark, (idle_cpu, session.diagnostics())
            mark = len(session.text(log))
            session.ipc(f"output {second} mode 800x500")
            session.expect_log(child, log, "Shared EGL second presented 800x500", after=mark)
            session.expect_frame(child, first)
            session.expect_frame(child, second)
            session.ipc(f"output {first} disable")
            session.expect_log(child, log, "Shared EGL primary removed; surviving texture is live")
            session.expect_frame(child, second)
            for command, size in [(f"output {second} scale 1.25", "640x400"),
                                  (f"output {second} scale 2", "400x250"),
                                  (f"output {second} transform 90", "250x400")]:
                mark = len(session.text(log))
                session.ipc(command)
                session.expect_log(child, log, f"Shared EGL second presented {size}", after=mark)
                session.expect_frame(child, second)
            child.send_signal(signal.SIGINT)
            report = json.loads(finish(child))
            assert child.returncode == 0 and report["status"] == "passed", session.diagnostics()
            assert report["scope"] == "two_private_outputs_one_egl_session", report
            assert report["first_removed"] and report["survivor_presented"] and report["texture_survived_no_windows"], report
            assert report["first_presented"] >= 1 and report["second_presented"] >= 5 and report["witness_button_events"] >= 2, report
            assert 0 <= report["geometry_refreshes"] <= 4, report
            assert report["gl"]["api"] == args.gl_api, report
            assert ("NVIDIA" in report["gl"]["vendor"] if args.nvidia else "llvmpipe" in report["gl"]["renderer"].lower()), report
            assert report["egl_lifecycle"] == {"display_initializations": 1, "context_creations": 1, "surface_creations": 2,
                                               "surface_destructions": 2, "context_destructions": 1, "display_terminations": 1}, report
            assert report["session_reused"] and report["conflicting_api_rejected"] and report["cross_thread_rejected"], report
            assert report["recreated_owner_lifecycle"] == {"display_initializations":1,"context_creations":1,"surface_creations":0,
                                                          "surface_destructions":0,"context_destructions":1,"display_terminations":1}, report
            assert report["mixed_thread_api_rejected"] and report["independent_connection_lifecycle"] == report["recreated_owner_lifecycle"], report
            if args.nvidia:
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-16_384:]
            wait_for(lambda: empty(second), "surviving surface cleanup")
            session.ipc(f"output {first} enable")
            # Sway 1.9 may not produce a screencopy frame on a newly enabled empty
            # headless output until a client damages it. The next client supplies
            # that damage; validate the active output before launching it.
            wait_for(lambda: any(item["name"] == first and item["active"] for item in session.outputs()),
                     "first output reenabled for the crash case")

            crashed, log = spawn()
            session.expect_log(crashed, log, "Shared EGL presented on both private outputs")
            session.expect_frame(crashed, first)
            session.expect_frame(crashed, second)
            crashed.kill()
            assert not finish(crashed).strip() and crashed.returncode == -signal.SIGKILL
            wait_for(lambda: empty(first) and empty(second), "both surfaces removed after SIGKILL")

            lost, log = spawn()
            session.expect_log(lost, log, "Shared EGL presented on both private outputs")
            session.process.terminate()
            session.process.wait(timeout=10)
            assert not finish(lost).strip() and lost.returncode != 0, session.diagnostics()
        except Exception:
            if args.artifact_directory:
                args.artifact_directory.mkdir(parents=True, exist_ok=False)
                for path in session.directory.glob("*"):
                    if path.is_file() and path.suffix in [".log", ".json", ".png"]:
                        shutil.copy2(path, args.artifact_directory / path.name)
            print(session.diagnostics())
            raise
        finally:
            session.close()
    print(json.dumps({"shared_egl_session": "passed", "gl_api": args.gl_api, "nvidia": args.nvidia,
                      "gl": report["gl"], "egl_lifecycle": report["egl_lifecycle"],
                      "idle_cpu_seconds_over_one_second": idle_cpu,
                      "geometry_refreshes": report["geometry_refreshes"],
                      "checks": ["both compositor outputs", "presentation feedback", "real click-through",
                                 "same-connection owner reuse", "conflicting API/cross-thread rejection",
                                 "independent owner preserves current context", "owner recreation after final release",
                                 "surviving output after first removal", "scale/rotation after removal",
                                 "GL resource retained with no windows", "single display termination", "idle sleep",
                                 "SIGINT", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
