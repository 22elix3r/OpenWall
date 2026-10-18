#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Hold native EGL child commits behind a parent in a private compositor."""
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

from PIL import ImageChops
from private_gpu import nvidia_renderer
from test_owned_egl_wayland import cpu_seconds
from test_wayland_headless import COLORS, Session, wait_for

ROOT = Path(__file__).resolve().parents[1]
LIFECYCLE = {"display_initializations": 1, "context_creations": 1, "surface_creations": 1,
             "surface_destructions": 1, "context_destructions": 1, "display_terminations": 1}


def solid(image, color):
    return all(max(abs(low - target), abs(high - target)) <= 2
               for (low, high), target in zip(image.getextrema(), color))


def quadrants(image):
    # Check entire interiors, including near the outer edges, rather than just
    # four center pixels. Leave a two-pixel tolerance around fractional boundaries.
    # Sway 1.9's software cursor can be included in screencopy briefly after the
    # injected center click, despite grim's default cursor exclusion. Ignore only
    # its small central footprint; retain the rest of each quadrant and all edges.
    cx, cy = image.width // 2, image.height // 2
    visible = [(0, 0, image.width, cy - 4), (0, cy + 28, image.width, image.height),
               (0, cy - 4, cx - 4, cy + 28), (cx + 20, cy - 4, image.width, cy + 28)]
    for (left, top, right, bottom), color in zip(
            [(0, 0, .5, .5), (.5, 0, 1, .5), (0, .5, .5, 1), (.5, .5, 1, 1)], COLORS):
        box = (int(image.width * left) + 2, int(image.height * top) + 2,
               int(image.width * right) - 2, int(image.height * bottom) - 2)
        for mask in visible:
            region = (max(box[0], mask[0]), max(box[1], mask[1]), min(box[2], mask[2]), min(box[3], mask[3]))
            if region[0] < region[2] and region[1] < region[3] and not solid(image.crop(region), color):
                return False
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/staged_egl_subsurface_probe")
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
    with tempfile.TemporaryDirectory(prefix="openwall-staged-egl-") as temporary:
        session = Session(Path(temporary), sway, software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor))
        captures = 0
        try:
            session.connect()
            output, other = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(output), "private baseline")
            background = baseline.getpixel((10, 10))
            other_baseline = wait_for(lambda: session.capture(other), "other private baseline")
            outputs, sockets = {}, {}

            def spawn(extra=()):
                index = len(session.logs)
                log = (session.directory / f"staged-{index}.log").open("w+")
                result = (session.directory / f"staged-{index}.json").open("w+")
                session.logs.extend([log, result])
                path = session.directory / f"control-{index}.sock"
                child = subprocess.Popen([str(binary), "--gl-api", args.gl_api, "--output", output,
                                          "--control-socket", str(path), "--seconds", "30", *extra],
                                         env=session.env, stdout=result, stderr=log, text=True)
                session.workers.append(child)
                outputs[child.pid], sockets[child.pid] = result, path
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(outputs[child.pid])

            def command(child, message=b"publish\n"):
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                    control.sendto(message, str(sockets[child.pid]))

            def capture():
                nonlocal captures
                image = session.capture(output)
                assert image is not None, session.diagnostics()
                captures += 1
                return image

            def hidden_for(child, seconds=.3):
                # Native staging can finish before the independent XDG witness
                # maps, especially on older Sway. Begin the held-image check only
                # after the witness itself has appeared.
                wait_for(lambda: child.poll() is None and solid(capture(), (34, 34, 34)), "witness mapped before held-image check")
                until = time.monotonic() + seconds
                while time.monotonic() < until:
                    assert child.poll() is None, session.diagnostics()
                    assert solid(capture(), (34, 34, 34)), session.diagnostics()

            def empty():
                image = session.capture(output)
                return image is not None and solid(image, background)

            def publish(child, log):
                wait_for(lambda: child.poll() is None and solid(capture(), (34, 34, 34)), "witness mapped before publication")
                command(child)
                # Sample actual compositor frames during publication. Any observed
                # frame must be the witness or the complete, correctly sized image.
                until = time.monotonic() + 8
                while time.monotonic() < until:
                    assert child.poll() is None, session.diagnostics()
                    image = capture()
                    assert solid(image, (34, 34, 34)) or quadrants(image), session.diagnostics()
                    if "Staged EGL presented; pointer reached underlying witness" in session.text(log) and quadrants(image):
                        return
                raise AssertionError("staged publication/input timed out")

            child, log = spawn()
            session.expect_log(child, log, "Staged EGL cached 640x360; parent held")
            wait_for(lambda: solid(capture(), (34, 34, 34)), "underlying witness")
            hidden_for(child)
            mark = len(session.text(log))
            session.ipc(f"output {output} mode 800x500")
            session.expect_log(child, log, "Staged EGL cached 800x500; parent held", after=mark)
            wait_for(lambda: solid(capture(), (34, 34, 34)), "resized witness")
            hidden_for(child)
            before = cpu_seconds(child.pid)
            mark = session.text(log)
            hidden_for(child, 1)
            idle_cpu = cpu_seconds(child.pid) - before
            assert idle_cpu < .25 and session.text(log) == mark, (idle_cpu, session.diagnostics())
            assert ImageChops.difference(session.capture(other), other_baseline).getbbox() is None
            publish(child, log)
            child.send_signal(signal.SIGINT)
            report = json.loads(finish(child))
            assert child.returncode == 0 and report["status"] == "passed", session.diagnostics()
            assert report["scope"] == "synchronized_egl_child_in_private_compositor", report
            assert report["published"] and report["presented"] == 1 and report["presented_frame_ids"] == [1], report
            assert report["witness_button_events"] == 2 and report["logical_size"] == [800, 500], report
            assert 3 <= report["swaps"] <= 4 and 0 <= report["geometry_refreshes"] <= 1, report
            assert report["egl_lifecycle"] == LIFECYCLE and report["gl"]["api"] == args.gl_api, report
            assert ("NVIDIA" in report["gl"]["vendor"] if args.nvidia else "llvmpipe" in report["gl"]["renderer"].lower()), report
            wait_for(empty, "published native/parent/witness cleanup")

            cancelled, log = spawn()
            session.expect_log(cancelled, log, "Staged EGL cached 800x500; parent held")
            hidden_for(cancelled)
            cancelled.send_signal(signal.SIGINT)
            cancellation = json.loads(finish(cancelled))
            assert cancelled.returncode == 0 and cancellation["status"] == "cancelled_before_publish", cancellation
            assert not cancellation["published"] and cancellation["presented"] == 0, cancellation
            assert cancellation["witness_button_events"] == 0 and cancellation["egl_lifecycle"] == LIFECYCLE, cancellation
            wait_for(empty, "unpublished cancellation cleanup")

            # Binding a diagnostic path must never replace an existing file.
            occupied = session.directory / "occupied.sock"
            occupied.write_bytes(b"keep this diagnostic sentinel")
            collision = subprocess.run([str(binary), "--output", output, "--control-socket", str(occupied)],
                                       env=session.env, capture_output=True, text=True, timeout=5)
            assert collision.returncode != 0 and not collision.stdout.strip(), collision
            assert occupied.read_bytes() == b"keep this diagnostic sentinel"

            for message in [b"invalid\n", b"publish\n" + b"x" * 64]:
                invalid, log = spawn()
                session.expect_log(invalid, log, "Staged EGL cached 800x500; parent held")
                command(invalid, message)
                assert not finish(invalid).strip() and invalid.returncode != 0, session.diagnostics()
                assert "invalid or duplicate staged publish command" in session.text(log), session.diagnostics()
                wait_for(empty, "malformed control cleanup")

            duplicate, log = spawn()
            session.expect_log(duplicate, log, "Staged EGL cached 800x500; parent held")
            publish(duplicate, log)
            command(duplicate)
            assert not finish(duplicate).strip() and duplicate.returncode != 0, session.diagnostics()
            assert "invalid or duplicate staged publish command" in session.text(log), session.diagnostics()
            wait_for(empty, "duplicate control cleanup")

            negative, log = spawn(["--intercept-input"])
            session.expect_log(negative, log, "Staged EGL cached 800x500; parent held")
            hidden_for(negative)
            command(negative)
            assert not finish(negative).strip() and negative.returncode != 0, session.diagnostics()
            assert "test pointer was intercepted by staged EGL child" in session.text(log), session.diagnostics()
            wait_for(empty, "opaque child negative-control cleanup")

            for published in [False, True]:
                crashed, log = spawn()
                session.expect_log(crashed, log, "Staged EGL cached 800x500; parent held")
                if published:
                    publish(crashed, log)
                else:
                    hidden_for(crashed)
                crashed.kill()
                assert not finish(crashed).strip() and crashed.returncode == -signal.SIGKILL
                wait_for(empty, "staged cleanup after SIGKILL")

            if args.nvidia:
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-16_384:]
            lost, log = spawn()
            session.expect_log(lost, log, "Staged EGL cached 800x500; parent held")
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
    print(json.dumps({"staged_egl": "passed", "gl_api": args.gl_api, "nvidia": args.nvidia,
                      "gl": report["gl"], "egl_lifecycle": report["egl_lifecycle"],
                      "swaps": report["swaps"], "geometry_refreshes": report["geometry_refreshes"],
                      "sampled_compositor_frames": captures, "unpublished_idle_cpu_seconds": idle_cpu,
                      "checks": ["held parent", "unpublished resize", "sampled publication pixels", "actual presentation",
                                 "real click-through", "other output unchanged", "unpublished cancellation",
                                 "malformed/duplicate commands", "existing path preserved", "opaque child negative control",
                                 "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
