#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Keep a previous root image visible through nested EGL publication/rollback."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time

from private_gpu import nvidia_renderer
from test_owned_egl_wayland import cpu_seconds
from test_staged_egl import LIFECYCLE, quadrants, solid, solid_without_pointer
from test_wayland_headless import Session, wait_for

ROOT = Path(__file__).resolve().parents[1]
PREVIOUS = (47, 81, 135)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/nested_egl_publication_probe")
    parser.add_argument("--gl-api", choices=["desktop", "gles"], default="desktop")
    parser.add_argument("--nvidia", action="store_true")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--artifact-directory", type=Path)
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
    assert vendor.is_file()
    guard_env = os.environ.copy()
    guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    guard = subprocess.run([str(binary), "--output", "never-connect", "--control-socket", "/never-created.sock"],
                           env=guard_env, capture_output=True, text=True, timeout=5)
    assert guard.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guard.stderr and not guard.stdout.strip(), guard
    with tempfile.TemporaryDirectory(prefix="openwall-nested-egl-") as temporary:
        session = Session(Path(temporary), sway, software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor))
        captures = 0
        try:
            session.connect()
            output, other = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(output), "private baseline").getpixel((10, 10))
            other_baseline = wait_for(lambda: session.capture(other), "other output baseline").tobytes()
            outputs, controls = {}, {}

            def spawn(extra=()):
                index = len(session.logs)
                log = (session.directory / f"nested-{index}.log").open("w+")
                result = (session.directory / f"nested-{index}.json").open("w+")
                session.logs.extend([log, result])
                path = session.directory / f"control-{index}.sock"
                child = subprocess.Popen([str(binary), "--output", output, "--gl-api", args.gl_api,
                                          "--control-socket", str(path), *extra], env=session.env, stdout=result, stderr=log, text=True)
                session.workers.append(child)
                outputs[child.pid], controls[child.pid] = result, path
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(outputs[child.pid])

            def command(child, message):
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                    control.sendto(message, str(controls[child.pid]))

            def capture():
                nonlocal captures
                image = session.capture(output)
                assert image is not None, session.diagnostics()
                captures += 1
                return image

            def empty():
                image = session.capture(output)
                return image is not None and solid(image, baseline)

            def held(child, seconds=.3):
                until = time.monotonic() + seconds
                while time.monotonic() < until:
                    assert child.poll() is None and solid_without_pointer(capture(), PREVIOUS), session.diagnostics()
                    time.sleep(.03)

            def publish(child, log):
                command(child, b"publish\n")
                until = time.monotonic() + 8
                while time.monotonic() < until:
                    assert child.poll() is None, session.diagnostics()
                    image = capture()
                    assert solid_without_pointer(image, PREVIOUS) or quadrants(image), session.diagnostics()
                    if "Nested candidate presented; pointer reached witness" in session.text(log) and quadrants(image):
                        return
                raise AssertionError("nested publication did not complete")

            child, log = spawn()
            session.expect_log(child, log, "Nested candidate cached 640x360; previous root visible")
            # The private seat's initial cursor starts away from the later click
            # position. Let its configured hide timeout elapse in screencopy
            # before the continuous previous-image assertions begin.
            wait_for(lambda: solid_without_pointer(capture(), PREVIOUS), "initial previous root pixels")
            held(child)
            mark = len(session.text(log))
            session.ipc(f"output {output} mode 800x500")
            session.expect_log(child, log, "Nested candidate cached 800x500; previous root visible", after=mark)
            held(child)
            before, mark = cpu_seconds(child.pid), session.text(log)
            held(child, 1)
            idle_cpu = cpu_seconds(child.pid) - before
            assert idle_cpu < .25 and session.text(log) == mark, (idle_cpu, session.diagnostics())
            publish(child, log)
            command(child, b"rollback\n")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                assert child.poll() is None, session.diagnostics()
                image = capture()
                assert solid_without_pointer(image, PREVIOUS) or quadrants(image), session.diagnostics()
                if "Nested previous image restored; pointer reached witness" in session.text(log) and solid_without_pointer(image, PREVIOUS):
                    break
            else:
                raise AssertionError("nested rollback did not complete")
            held(child)
            child.send_signal(signal.SIGINT)
            report = json.loads(finish(child))
            assert child.returncode == 0 and report["status"] == "passed", session.diagnostics()
            assert report["scope"] == "persistent_root_nested_candidate" and report["previous_rgb"] == list(PREVIOUS), report
            assert report["candidate_presented"] and report["previous_restored"] and not report["candidate_aborted"], report
            assert report["presented_frame_ids"] == [0, 1, 2] and report["witness_button_events"] == 4, report
            assert report["egl_lifecycle"] == LIFECYCLE and report["logical_size"] == [800, 500], report
            assert report["gl"]["api"] == args.gl_api and report["root_surface_id"] > 1, report
            assert ("NVIDIA" in report["gl"]["vendor"] if args.nvidia else "llvmpipe" in report["gl"]["renderer"].lower()), report
            assert session.capture(other).tobytes() == other_baseline, "nested proof changed other output"
            wait_for(empty, "persistent root cleanup after rollback")

            aborted, log = spawn()
            session.expect_log(aborted, log, "Nested candidate cached 800x500; previous root visible")
            command(aborted, b"abort\n")
            session.expect_log(aborted, log, "Nested candidate aborted; previous root retained")
            held(aborted, 1)
            aborted.send_signal(signal.SIGINT)
            cancellation = json.loads(finish(aborted))
            assert aborted.returncode == 0 and cancellation["candidate_aborted"], cancellation
            assert not cancellation["candidate_presented"] and not cancellation["previous_restored"], cancellation
            assert cancellation["presented_frame_ids"] == [0] and cancellation["witness_button_events"] == 0, cancellation
            assert cancellation["aborted_lifecycle"] == dict(LIFECYCLE, context_destructions=0, display_terminations=0), cancellation
            assert cancellation["egl_lifecycle"] == LIFECYCLE, cancellation
            wait_for(empty, "aborted candidate and root cleanup")

            for message, error in [(b"rollback\n", "invalid nested command order"), (b"invalid\n", "invalid nested diagnostic command"),
                                   (b"publish\n" + b"x" * 64, "invalid nested diagnostic command")]:
                invalid, log = spawn()
                session.expect_log(invalid, log, "Nested candidate cached 800x500; previous root visible")
                command(invalid, message)
                assert not finish(invalid).strip() and invalid.returncode != 0, session.diagnostics()
                assert error in session.text(log), session.diagnostics()
                wait_for(empty, "invalid nested command cleanup")
            negative, log = spawn(["--intercept-input"])
            session.expect_log(negative, log, "Nested candidate cached 800x500; previous root visible")
            command(negative, b"publish\n")
            assert not finish(negative).strip() and negative.returncode != 0, session.diagnostics()
            assert "nested candidate intercepted private input" in session.text(log), session.diagnostics()
            wait_for(empty, "nested input negative-control cleanup")
            for visible in [False, True]:
                crashed, log = spawn()
                session.expect_log(crashed, log, "Nested candidate cached 800x500; previous root visible")
                if visible:
                    publish(crashed, log)
                crashed.kill()
                assert not finish(crashed).strip() and crashed.returncode == -signal.SIGKILL
                wait_for(empty, "nested root cleanup after SIGKILL")
            if args.nvidia:
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-16_384:]
            lost, log = spawn()
            session.expect_log(lost, log, "Nested candidate cached 800x500; previous root visible")
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
    print(json.dumps({"nested_egl": "passed", "gl_api": args.gl_api, "nvidia": args.nvidia, "gl": report["gl"],
                      "egl_lifecycle": report["egl_lifecycle"], "sampled_compositor_frames": captures,
                      "unpublished_idle_cpu_seconds": idle_cpu, "geometry_refreshes": report["geometry_refreshes"],
                      "checks": ["previous root stays mapped", "nested hidden resize", "sampled publication/rollback pixels",
                                 "actual old/candidate/restored presentation", "real input after publish/rollback", "abort retains previous image",
                                 "other output unchanged", "idle sleep", "invalid commands", "input negative control", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
