#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Shared-texture video pixels, presentation, looping and lifecycle in private Sway."""
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

from test_wayland_headless import Session, wait_for
from private_gpu import nvidia_renderer
from test_owned_egl_wayland import cpu_seconds
from test_staged_egl import solid
from video_color_fixtures import WIDTH, HEIGHT, patches, original_yuv, expected_rgba, enlarged_yuv

ROOT = Path(__file__).resolve().parents[1]
FRAMES, FPS = 12, 2


def fixture(directory, nvidia, *, variant=0):
    assert variant in [0, 1]
    colors = []
    base = patches(False)[::1 if variant == 0 else -1]
    for index in range(FRAMES):
        frame = base[index % len(base):] + base[:index % len(base)]
        frame[0] = (16 + 8 * index, 128, 128) if variant == 0 else (140 + 8 * index, 110, 150)
        colors.append(frame)
    scale = 2 if nvidia else 1
    raw_bytes = b"".join((enlarged_yuv if nvidia else original_yuv)(frame) for frame in colors)
    raw = directory / "original.yuv"
    raw.write_bytes(raw_bytes)
    video = directory / "original ! name='native'.webm"
    tags = ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pixel_format", "yuv420p",
                    "-video_size", f"{WIDTH * scale}x{HEIGHT * scale}", "-framerate", str(FPS), *tags, "-i", str(raw),
                    "-frames:v", str(FRAMES), "-an", "-c:v", "libvpx-vp9", "-lossless", "1", *tags, str(video)],
                   check=True, capture_output=True, timeout=30)
    decoded = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(video),
                              "-f", "rawvideo", "-pix_fmt", "yuv420p", *tags, "pipe:1"],
                             check=True, capture_output=True, timeout=30).stdout
    assert decoded == raw_bytes, "encoder changed original YUV samples"
    return video, colors, scale


def identify_pixels(image, colors):
    errors = []
    for frame in colors:
        maximum = 0
        for index, yuv in enumerate(frame):
            expected = expected_rgba(yuv, "bt709", False)[:3]
            for dx, dy in [(-.02, -.04), (0, 0), (.02, .04)]:
                point = (int(image.width * ((index % 5 + .5) / 5 + dx)),
                         int(image.height * ((index // 5 + .5) / 2 + dy)))
                maximum = max(maximum, max(abs(a - b) for a, b in zip(image.getpixel(point), expected)))
        errors.append(maximum)
    error = min(errors)
    return errors.index(error), error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/native_gl_video_probe")
    parser.add_argument("--gl-api", choices=["gles", "desktop"], default="desktop")
    parser.add_argument("--nvidia", action="store_true", help="explicitly enable NVIDIA decode/render in the private headless compositor")
    parser.add_argument("--render-node", type=Path, help="existing NVIDIA DRM render node; requires --nvidia")
    parser.add_argument("--staged", action="store_true", help="preroll paused behind an unmapped parent before publishing")
    parser.add_argument("--hold-seconds", type=float, default=1, help="staged preroll hold, 1–12 seconds")
    args = parser.parse_args()
    if not 1 <= args.hold_seconds <= 12:
        parser.error("--hold-seconds must be between 1 and 12")
    if args.nvidia and args.gl_api != "desktop":
        parser.error("NVIDIA GPU-path qualification requires --gl-api desktop")
    if args.render_node and not args.nvidia:
        parser.error("--render-node requires --nvidia")
    binary = args.binary.resolve(strict=True)
    sway = shutil.which("sway")
    assert sway and shutil.which("swaymsg") and shutil.which("grim") and shutil.which("ffmpeg")
    vendor = Path("/usr/share/glvnd/egl_vendor.d") / ("10_nvidia.json" if args.nvidia else "50_mesa.json")
    assert vendor.is_file(), "the selected EGL vendor configuration is required"
    render_env = None
    render_node = None
    if args.nvidia:
        vendor, render_node, render_env = nvidia_renderer(args.render_node)
    guard_env = os.environ.copy()
    guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    rejected = subprocess.run([str(binary), "missing.webm", "--output", "never-connect"], env=guard_env,
                              capture_output=True, text=True, timeout=5)
    assert rejected.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in rejected.stderr, rejected
    with tempfile.TemporaryDirectory(prefix="openwall-native-video-") as temporary:
        directory = Path(temporary)
        video, colors, scale = fixture(directory, args.nvidia)
        session_dir = directory / "session"
        session_dir.mkdir()
        session = Session(session_dir, sway, software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland", LIBGL_ALWAYS_SOFTWARE="1",
                           __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor), GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl",
                           GST_GL_API="opengl3" if args.gl_api == "desktop" else "gles2", GST_PLUGIN_FEATURE_RANK=("nvvp9dec:MAX" if args.nvidia else "vp9dec:MAX"),
                           GST_REGISTRY_1_0=str(directory / "registry.bin"))
        if args.nvidia:
            session.env.update(LIBGL_ALWAYS_SOFTWARE="0", GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        try:
            session.connect()
            first, second = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(first), "private compositor baseline").getpixel((10, 10))
            other = session.capture(second).tobytes()
            outputs = {}
            controls = {}

            def spawn(path=video, extra=()):
                log = (session.directory / f"native-{len(session.logs)}.log").open("w+")
                session.logs.append(log)
                output = (session.directory / f"native-{len(session.logs)}.json").open("w+")
                session.logs.append(output)
                control = session.directory / f"native-{len(session.logs)}.sock"
                staged_args = ["--staged-control", str(control)] if args.staged else []
                child = subprocess.Popen([str(binary), str(path), "--output", first, "--gl-api", args.gl_api, *staged_args, *extra],
                                         env=session.env, stdout=output, stderr=log, text=True)
                session.workers.append(child)
                outputs[child.pid] = output
                controls[child.pid] = control
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(outputs[child.pid])

            def empty():
                image = session.capture(first)
                return image is not None and all(
                    max(abs(a - b) for a, b in zip(image.getpixel((int(image.width * x), int(image.height * y))), baseline)) <= 2
                    for x, y in [(.1, .25), (.7, .25), (.3, .75), (.9, .75)])

            def staged(child, log):
                session.expect_log(child, log, "Native GL video staged ")
                def witness():
                    assert child.poll() is None, session.diagnostics()
                    image = session.capture(first)
                    return image is not None and solid(image, (34, 34, 34))
                wait_for(witness, "underlying witness while video is paused")

            def publish(child, message=b"publish\n"):
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                    control.sendto(message, str(controls[child.pid]))

            def start_presentation(child, log):
                if args.staged:
                    staged(child, log)
                    publish(child)
                session.expect_log(child, log, "Native GL video presented; pointer reached underlying witness")

            def identify_frame(child, log):
                if child.poll() is not None:
                    return None
                image = session.capture(first)
                # EGL/driver teardown can outlive surface removal. A screenshot
                # racing intentional cleanup must not be judged as a video frame.
                if child.poll() is not None or "Native GL video removing private surfaces" in session.text(log):
                    return None
                assert image is not None
                identity, error = identify_pixels(image, colors)
                assert error <= 3, ("compositor pixels do not match any original frame", error, session.diagnostics())
                return identity, error

            child, log = spawn()
            idle_cpu = None
            if args.staged:
                staged(child, log)
                mark = len(session.text(log))
                session.ipc(f"output {first} mode 800x500")
                session.expect_log(child, log, "Native GL video staged 800x500; paused parent held", after=mark)
                # Wait for the separate witness to resize, then inspect held pixels
                # while preroll is idle. No playback or presentation may begin.
                wait_for(lambda: solid(session.capture(first), (34, 34, 34)), "resized video staging witness")
                mark = session.text(log)
                before = cpu_seconds(child.pid)
                until = time.monotonic() + args.hold_seconds
                while time.monotonic() < until:
                    assert child.poll() is None and solid(session.capture(first), (34, 34, 34)), session.diagnostics()
                    time.sleep(.05)
                idle_cpu = cpu_seconds(child.pid) - before
                assert idle_cpu < .25 and session.text(log) == mark, (idle_cpu, session.diagnostics())
                assert "Native GL video presented" not in mark and "parent published" not in mark, session.diagnostics()
                publish(child)
            session.expect_log(child, log, "Native GL video presented; pointer reached underlying witness")
            seen = set()
            maximum_error = 0
            clicks = 2
            commands = [] if args.staged else [f"output {first} mode 800x500", f"output {first} scale 1.25",
                                               f"output {first} scale 2", f"output {first} transform 90"]
            for command in commands:
                result = identify_frame(child, log)
                assert result is not None, session.diagnostics()
                identity, error = result
                seen.add(identity)
                maximum_error = max(maximum_error, error)
                mark = len(session.text(log))
                session.ipc(command)
                clicks += 2
                session.expect_log(child, log, f"Private video click-through confirmed ({clicks} button events)", after=mark)
            until = time.monotonic() + 25
            while child.poll() is None:
                assert time.monotonic() < until, ("bounded playback did not finish", session.diagnostics())
                result = identify_frame(child, log)
                if result is None:
                    break
                identity, error = result
                seen.add(identity)
                maximum_error = max(maximum_error, error)
                time.sleep(.1)
            stdout = finish(child)
            assert child.returncode == 0, session.diagnostics()
            report = json.loads(stdout)
            assert report["scope"] == "shared_video_into_owned_private_wayland_surface", report
            assert report["staging"]["enabled"] == args.staged and report["staging"]["published"], report
            assert report["staging"]["playback_started"], report
            assert ("nvvp9dec" if args.nvidia else "vp9dec") in report["decoders"], report
            decoder = next(item for item in report["decoder_buffers"].values()
                           if item["factory"] == ("nvvp9dec" if args.nvidia else "vp9dec"))
            assert decoder["memories"] > 0 and decoder["allocator_types"], report
            assert decoder["all_memory_is_gl"] == args.nvidia, report
            assert "video/x-raw(memory:GLMemory)" in report["input_memory"]["caps"], report
            assert report["input_memory"]["memory"]["shared_context"] and report["input_memory"]["memory"]["producer_sync_meta"], report
            assert report["completed_loops"] == 2 and not report["interrupted"], report
            assert len(seen) >= 4, ("fixture did not visibly animate", seen)
            assert report["consumer_cpu_buffer_maps"] == report["consumer_readbacks"] == report["consumer_offscreen_targets"] == 0, report
            assert report["egl_context_creations"] == 1 and report["source_size"] == [WIDTH * scale, HEIGHT * scale], report
            assert report["egl_lifecycle"] == {"display_initializations": 1, "context_creations": 1, "surface_creations": 1,
                                               "surface_destructions": 1, "context_destructions": 1, "display_terminations": 1}, report
            assert report["gl"]["api"] == args.gl_api, report
            assert ("NVIDIA" in report["gl"]["vendor"] if args.nvidia else "llvmpipe" in report["gl"]["renderer"].lower()), report
            if args.nvidia:
                assert "video/x-raw(memory:GLMemory)" in decoder["caps"], report
                native_log = session.text(log)
                assert "using GL memory" in native_log and "fallback to system memory" not in native_log, session.diagnostics()
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(render_node) in compositor_log, compositor_log[-16_384:]
            assert report["witness_button_events"] >= clicks and report["input_region"] == "empty", report
            fences = report["consumer_fences"]
            assert fences["submitted"] == fences["retired"] == len(report["frames"]), report
            assert 1 <= fences["peak_in_flight"] <= fences["limit"] == 3 and fences["cleanup_finish_calls"] == 0, report
            presented = set(report["presented_frame_ids"])
            if args.staged:
                hidden = [frame for frame in report["frames"] if not frame["published"]]
                assert report["staging"]["prepared"] and 2 <= len(hidden) <= 3, report
                assert all(frame["pts_ns"] == 0 and frame["id"] not in presented for frame in hidden), report
                assert all(not frame["geometry_refresh_required"] for frame in report["frames"] if frame["published"]), report
            assert presented and presented <= {frame["id"] for frame in report["frames"]}, report
            assert len(presented) == report["presented"] and report["discarded"] == 0, report
            sizes = {tuple(frame["logical_size"]) for frame in report["frames"] if frame["published"]}
            for size in sizes:
                assert any(tuple(frame["logical_size"]) == size and frame["id"] in presented
                           and not frame["geometry_refresh_required"] for frame in report["frames"]), report
            assert report["delivery"]["latest_slot_limit"] == 1 and report["delivery"]["received"] >= FRAMES * 2, report
            for loop in range(2):
                frames = [frame for frame in report["frames"] if frame["loop"] == loop and frame["id"] in presented]
                assert len(frames) >= 4 and frames[0]["pts_ns"] == 0, report
                assert all(a["pts_ns"] <= b["pts_ns"] for a, b in zip(frames, frames[1:])), report
            assert session.capture(second).tobytes() == other, "selected-output probe changed the other monitor"
            wait_for(empty, "video/witness surface cleanup after EOS and rewind")

            interrupted, log = spawn()
            start_presentation(interrupted, log)
            interrupted.send_signal(signal.SIGINT)
            stdout = finish(interrupted)
            assert interrupted.returncode == 0 and json.loads(stdout)["interrupted"], session.diagnostics()
            wait_for(empty, "video/witness cleanup after SIGINT")

            if args.staged:
                cancelled, log = spawn()
                staged(cancelled, log)
                cancelled.send_signal(signal.SIGINT)
                cancellation = json.loads(finish(cancelled))
                assert cancelled.returncode == 0 and cancellation["status"] == "cancelled_before_publish", cancellation
                assert not cancellation["staging"]["published"] and cancellation["presented"] == 0, cancellation
                assert not cancellation["staging"]["playback_started"], cancellation
                assert cancellation["delivery"]["received"] == 1 and cancellation["witness_button_events"] == 0, cancellation
                assert cancellation["egl_lifecycle"] == report["egl_lifecycle"], cancellation
                assert all(frame["pts_ns"] == 0 and not frame["published"] for frame in cancellation["frames"]), cancellation
                assert cancellation["consumer_fences"]["submitted"] == cancellation["consumer_fences"]["retired"], cancellation
                assert cancellation["consumer_fences"]["cleanup_finish_calls"] == 0, cancellation
                wait_for(empty, "paused preroll cancellation cleanup")

                invalid_control, log = spawn()
                staged(invalid_control, log)
                publish(invalid_control, b"invalid\n")
                assert not finish(invalid_control).strip() and invalid_control.returncode != 0, session.diagnostics()
                assert "invalid or duplicate staged publish command" in session.text(log), session.diagnostics()
                wait_for(empty, "preroll cleanup after malformed control")

                hidden_crash, log = spawn()
                staged(hidden_crash, log)
                hidden_crash.kill()
                assert not finish(hidden_crash).strip() and hidden_crash.returncode == -signal.SIGKILL
                wait_for(empty, "unpublished video cleanup after SIGKILL")

            control, log = spawn(extra=["--intercept-input"])
            if args.staged:
                staged(control, log)
                publish(control)
            stdout = finish(control)
            assert control.returncode != 0 and not stdout.strip(), session.diagnostics()
            assert "test pointer was intercepted by video surface" in session.text(log), session.diagnostics()
            wait_for(empty, "opaque-input negative-control cleanup")

            bad = directory / "malformed ! name='video'.webm"
            bad.write_bytes(b"not a video")
            invalid, log = spawn(path=bad)
            stdout = finish(invalid)
            assert invalid.returncode != 0 and not stdout.strip(), session.diagnostics()
            wait_for(empty, "malformed media cleanup")

            crashed, log = spawn()
            start_presentation(crashed, log)
            crashed.kill()
            stdout = finish(crashed)
            assert crashed.returncode == -signal.SIGKILL and not stdout.strip(), stdout
            wait_for(empty, "video/witness cleanup after SIGKILL")

            lost, log = spawn()
            start_presentation(lost, log)
            session.process.terminate()
            session.process.wait(timeout=10)
            stdout = finish(lost)
            assert lost.returncode != 0 and not stdout.strip(), session.diagnostics()
        except Exception:
            print(session.diagnostics())
            raise
        finally:
            session.close()
    print(json.dumps({"native_gl_video": "passed", "gl_api": args.gl_api, "nvidia": args.nvidia, "staged": args.staged,
                      "render_node": str(render_node) if render_node else None,
                      "gl": report["gl"], "gstreamer": report["gstreamer"],
                      "visible_frame_ids": sorted(seen), "max_compositor_channel_error": maximum_error,
                      "presented": report["presented"], "frames": len(report["frames"]),
                      "unpublished_idle_cpu_seconds": idle_cpu,
                      "unpublished_hold_seconds": args.hold_seconds if args.staged else None,
                      "checks": ["original animated pixels", "per-frame presentation feedback", "loop after draining samples",
                                 "real click-through", "selected output", "bounded fences",
                                 "no consumer map/readback/offscreen target", "EOS", "SIGINT", "opaque-input negative control",
                                 "malformed media", "SIGKILL", "compositor loss",
                                 *(["paused preroll", "unpublished resize", "held pixels and idle sleep", "publish before playback",
                                    "cancellation and malformed control while paused", "unpublished SIGKILL"] if args.staged
                                   else ["resize/scale/rotation without decoder restart"])]}))


if __name__ == "__main__":
    main()
