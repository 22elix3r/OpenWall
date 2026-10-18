#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Shared-texture video pixels, presentation, looping and lifecycle in private Sway."""
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
from video_color_fixtures import WIDTH, HEIGHT, patches, original_yuv, expected_rgba

ROOT = Path(__file__).resolve().parents[1]
FRAMES, FPS = 12, 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/native_gl_video_probe")
    parser.add_argument("--gl-api", choices=["gles", "desktop"], default="desktop")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    sway = shutil.which("sway")
    assert sway and shutil.which("swaymsg") and shutil.which("grim") and shutil.which("ffmpeg")
    mesa = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    assert mesa.is_file(), "Mesa EGL configuration is required"
    guard_env = os.environ.copy()
    guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    rejected = subprocess.run([str(binary), "missing.webm", "--output", "never-connect"], env=guard_env,
                              capture_output=True, text=True, timeout=5)
    assert rejected.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in rejected.stderr, rejected
    with tempfile.TemporaryDirectory(prefix="openwall-native-video-") as temporary:
        directory = Path(temporary)
        colors = []
        base = patches(False)
        for index in range(FRAMES):
            frame = base[index % len(base):] + base[:index % len(base)]
            frame[0] = (16 + 8 * index, 128, 128)
            colors.append(frame)
        raw_bytes = b"".join(original_yuv(frame) for frame in colors)
        raw = directory / "original.yuv"
        raw.write_bytes(raw_bytes)
        video = directory / "original ! name='native'.webm"
        tags = ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]
        subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pixel_format", "yuv420p",
                        "-video_size", f"{WIDTH}x{HEIGHT}", "-framerate", str(FPS), *tags, "-i", str(raw),
                        "-frames:v", str(FRAMES), "-an", "-c:v", "libvpx-vp9", "-lossless", "1", *tags, str(video)],
                       check=True, capture_output=True, timeout=30)
        decoded = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(video),
                                  "-f", "rawvideo", "-pix_fmt", "yuv420p", *tags, "pipe:1"],
                                 check=True, capture_output=True, timeout=30).stdout
        assert decoded == raw_bytes, "encoder changed original YUV samples"
        session_dir = directory / "session"
        session_dir.mkdir()
        session = Session(session_dir, sway, software_vulkan=False)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland", LIBGL_ALWAYS_SOFTWARE="1",
                           __EGL_VENDOR_LIBRARY_FILENAMES=str(mesa), GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl",
                           GST_GL_API="opengl3" if args.gl_api == "desktop" else "gles2", GST_PLUGIN_FEATURE_RANK="vp9dec:MAX",
                           GST_REGISTRY_1_0=str(directory / "registry.bin"))
        try:
            session.connect()
            first, second = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(first), "private compositor baseline").getpixel((10, 10))
            other = session.capture(second).tobytes()
            outputs = {}

            def spawn(path=video, extra=()):
                log = (session.directory / f"native-{len(session.logs)}.log").open("w+")
                session.logs.append(log)
                output = (session.directory / f"native-{len(session.logs)}.json").open("w+")
                session.logs.append(output)
                child = subprocess.Popen([str(binary), str(path), "--output", first, "--gl-api", args.gl_api, *extra],
                                         env=session.env, stdout=output, stderr=log, text=True)
                session.workers.append(child)
                outputs[child.pid] = output
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(outputs[child.pid])

            def empty():
                image = session.capture(first)
                return image is not None and all(
                    max(abs(a - b) for a, b in zip(image.getpixel((int(image.width * x), int(image.height * y))), baseline)) <= 2
                    for x, y in [(.1, .25), (.7, .25), (.3, .75), (.9, .75)])

            def identify_frame(child):
                if child.poll() is not None:
                    return None
                image = session.capture(first)
                if child.poll() is not None:
                    return None
                assert image is not None
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
                assert error <= 3, ("compositor pixels do not match any original frame", errors, session.diagnostics())
                return errors.index(error), error

            child, log = spawn()
            session.expect_log(child, log, "Native GL video presented; pointer reached underlying witness")
            seen = set()
            maximum_error = 0
            clicks = 2
            for command in [f"output {first} mode 800x500", f"output {first} scale 1.25",
                            f"output {first} scale 2", f"output {first} transform 90"]:
                result = identify_frame(child)
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
                result = identify_frame(child)
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
            assert "vp9dec" in report["decoders"], report
            assert "video/x-raw(memory:GLMemory)" in report["input_memory"]["caps"], report
            assert report["input_memory"]["memory"]["shared_context"] and report["input_memory"]["memory"]["producer_sync_meta"], report
            assert report["completed_loops"] == 2 and not report["interrupted"], report
            assert len(seen) >= 4, ("fixture did not visibly animate", seen)
            assert report["consumer_cpu_buffer_maps"] == report["consumer_readbacks"] == report["consumer_offscreen_targets"] == 0, report
            assert report["egl_context_creations"] == 1 and report["source_size"] == [WIDTH, HEIGHT], report
            assert report["gl"]["api"] == args.gl_api and "llvmpipe" in report["gl"]["renderer"].lower(), report
            assert report["witness_button_events"] >= clicks and report["input_region"] == "empty", report
            fences = report["consumer_fences"]
            assert fences["submitted"] == fences["retired"] == len(report["frames"]), report
            assert 1 <= fences["peak_in_flight"] <= fences["limit"] == 3 and fences["cleanup_finish_calls"] == 0, report
            presented = set(report["presented_frame_ids"])
            assert presented and presented <= {frame["id"] for frame in report["frames"]}, report
            assert len(presented) == report["presented"] and report["discarded"] == 0, report
            assert report["delivery"]["latest_slot_limit"] == 1 and report["delivery"]["received"] >= FRAMES * 2, report
            for loop in range(2):
                frames = [frame for frame in report["frames"] if frame["loop"] == loop and frame["id"] in presented]
                assert len(frames) >= 4 and frames[0]["pts_ns"] == 0, report
                assert all(a["pts_ns"] <= b["pts_ns"] for a, b in zip(frames, frames[1:])), report
            assert session.capture(second).tobytes() == other, "selected-output probe changed the other monitor"
            wait_for(empty, "video/witness surface cleanup after EOS and rewind")

            interrupted, log = spawn()
            session.expect_log(interrupted, log, "Native GL video presented; pointer reached underlying witness")
            interrupted.send_signal(signal.SIGINT)
            stdout = finish(interrupted)
            assert interrupted.returncode == 0 and json.loads(stdout)["interrupted"], session.diagnostics()
            wait_for(empty, "video/witness cleanup after SIGINT")

            control, log = spawn(extra=["--intercept-input"])
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
            session.expect_log(crashed, log, "Native GL video presented; pointer reached underlying witness")
            crashed.kill()
            stdout = finish(crashed)
            assert crashed.returncode == -signal.SIGKILL and not stdout.strip(), stdout
            wait_for(empty, "video/witness cleanup after SIGKILL")

            lost, log = spawn()
            session.expect_log(lost, log, "Native GL video presented; pointer reached underlying witness")
            session.process.terminate()
            session.process.wait(timeout=10)
            stdout = finish(lost)
            assert lost.returncode != 0 and not stdout.strip(), session.diagnostics()
        except Exception:
            print(session.diagnostics())
            raise
        finally:
            session.close()
    print(json.dumps({"native_gl_video": "passed", "gl_api": args.gl_api,
                      "visible_frame_ids": sorted(seen), "max_compositor_channel_error": maximum_error,
                      "presented": report["presented"], "frames": len(report["frames"]),
                      "checks": ["original animated pixels", "per-frame presentation feedback", "loop after draining samples",
                                 "real click-through after resize/scale/rotation", "selected output", "bounded fences",
                                 "no consumer map/readback/offscreen target", "EOS", "SIGINT", "opaque-input negative control",
                                 "malformed media", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
