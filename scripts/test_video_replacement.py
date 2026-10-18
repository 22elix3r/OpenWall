#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Replace and restore distinct video sources in a private headless compositor."""
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
from test_native_gl_video import fixture, identify_pixels
from test_wayland_headless import Session, wait_for

ROOT = Path(__file__).resolve().parents[1]
LIFECYCLE = {"context_creations": 1, "context_destructions": 1, "display_initializations": 1,
             "display_terminations": 1, "surface_creations": 2, "surface_destructions": 2}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/video_replacement_probe")
    parser.add_argument("--gl-api", choices=["desktop", "gles"], default="desktop")
    parser.add_argument("--nvidia", action="store_true")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--artifact-directory", type=Path)
    args = parser.parse_args()
    if args.nvidia and args.gl_api != "desktop":
        parser.error("NVIDIA video transfer qualification requires desktop GL")
    if args.render_node and not args.nvidia:
        parser.error("--render-node requires --nvidia")
    binary = args.binary.resolve(strict=True)
    assert all(shutil.which(tool) for tool in ["sway", "swaymsg", "grim", "ffmpeg"])
    vendor = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    node, render_env = None, None
    if args.nvidia:
        vendor, node, render_env = nvidia_renderer(args.render_node)
    assert vendor.is_file()
    env = os.environ.copy()
    env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    guard = subprocess.run([str(binary), "missing.webm", "candidate.webm", "--output", "never-connect",
                            "--control-socket", "/never-created.sock"], env=env, capture_output=True, text=True, timeout=5)
    assert guard.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guard.stderr and not guard.stdout.strip(), guard
    with tempfile.TemporaryDirectory(prefix="openwall-video-replacement-") as temporary:
        directory = Path(temporary)
        videos, colors = [], []
        for index in range(2):
            source = directory / f"source-{index}"
            source.mkdir()
            video, palette, _ = fixture(source, args.nvidia, variant=index)
            videos.append(video)
            colors.append(palette)
        runtime = directory / "session"
        runtime.mkdir()
        session = Session(runtime, shutil.which("sway"), software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor),
                           GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl", GST_GL_API="opengl3" if args.gl_api == "desktop" else "gles2",
                           GST_PLUGIN_FEATURE_RANK="nvvp9dec:MAX" if args.nvidia else "vp9dec:MAX", GST_REGISTRY_1_0=str(directory / "registry.bin"))
        if args.nvidia:
            session.env.update(GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        maximum_error = 0
        captures = 0
        try:
            session.connect()
            output, other = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(output), "baseline").getpixel((10, 10))
            other_baseline = session.capture(other).tobytes()
            results, controls = {}, {}

            def spawn(candidate=None, extra=()):
                index = len(session.logs)
                log = (runtime / f"replace-{index}.log").open("w+")
                result = (runtime / f"replace-{index}.json").open("w+")
                session.logs.extend([log, result])
                path = runtime / f"control-{index}.sock"
                child = subprocess.Popen([str(binary), str(videos[0]), str(candidate or videos[1]), "--output", output,
                                          "--control-socket", str(path), "--gl-api", args.gl_api, *extra],
                                         env=session.env, stdout=result, stderr=log, text=True)
                session.workers.append(child)
                results[child.pid], controls[child.pid] = result, path
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(results[child.pid])

            def command(child, message):
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                    control.sendto(message, str(controls[child.pid]))

            def pixels(child, allowed):
                nonlocal maximum_error, captures
                assert child.poll() is None, session.diagnostics()
                image = session.capture(output)
                assert image is not None, session.diagnostics()
                captures += 1
                matches = [(identify_pixels(image, colors[index]), index) for index in allowed]
                # Sort by channel error before marker/source identity.
                (identity, error), source = min(matches, key=lambda item: item[0][1])
                maximum_error = max(maximum_error, error)
                assert error <= 3, ("pixels outside permitted source frames", error, allowed, session.diagnostics())
                return source, identity

            def moving(child, index, seconds=1.2):
                seen = set()
                until = time.monotonic() + seconds
                while time.monotonic() < until:
                    source, identity = pixels(child, [index])
                    assert source == index
                    seen.add(identity)
                    time.sleep(.025)
                assert len(seen) >= 2, ("visible video stopped", index, seen, session.diagnostics())
                return seen

            def empty():
                image = session.capture(output)
                return image is not None and all(max(abs(a-b) for a,b in zip(image.getpixel((int(image.width*x), int(image.height*y))), baseline)) <= 2
                                                for x,y in [(.1,.25),(.7,.25),(.3,.75),(.9,.75)])

            def validate(report):
                assert report["egl_lifecycle"] == LIFECYCLE, report
                fences = report["consumer_fences"]
                assert fences["submitted"] == fences["retired"] == len(report["frames"]), report
                assert 0 < fences["peak_in_flight"] <= fences["limit"] == 3 and fences["cleanup_finish_calls"] == 0, report
                assert all(report[key] == 0 for key in ["consumer_cpu_buffer_maps", "consumer_readbacks", "consumer_offscreen_targets"]), report
                for label in ["previous", "candidate"]:
                    source = report[label]
                    if not source or not source["prepared"]:
                        continue
                    assert len(source["decoders"]) == 1 and "memory:GLMemory" in source["input_memory"]["caps"], report
                    assert source["input_memory"]["memory"]["shared_context"] and source["input_memory"]["memory"]["producer_sync_meta"], report
                    if args.nvidia:
                        assert source["decoders"] == ["nvvp9dec"], report
                        trace = next(iter(source["decoder_buffers"].values()))
                        assert trace["factory"] == "nvvp9dec" and trace["all_memory_is_gl"] and "memory:GLMemory" in trace["caps"], report
                presented = set(report["presented_frame_ids"])
                assert all(frame["id"] not in presented for frame in report["frames"] if not frame["visible"]), report
                assert all(start["presented_frame_id"] in presented for start in report["clock_starts"]), report
                assert report["previous"]["playback_started"], report

            child, log = spawn()
            session.expect_log(child, log, "Replacement candidate cached; previous video continues")
            old_seen = moving(child, 0, 7)
            command(child, b"publish\n")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                source, _ = pixels(child, [0, 1])
                if source == 1 and "Replacement candidate video presented; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("candidate publication did not complete")
            candidate_seen = moving(child, 1)
            command(child, b"rollback\n")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                source, _ = pixels(child, [0, 1])
                if source == 0 and "Replacement previous video restored; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("previous video restoration did not complete")
            restored_seen = moving(child, 0)
            child.send_signal(signal.SIGINT)
            report = json.loads(finish(child))
            assert child.returncode == 0 and report["status"] == "passed", session.diagnostics()
            validate(report)
            assert report["previous"]["completed_loops"] >= 1, report
            assert report["candidate_presented"] and report["previous_restored"] and report["witness_button_events"] == 6, report
            assert report["candidate"]["playback_started"] and len(report["candidate"]["decoders"]) == 1, report
            assert [start["source"] for start in report["clock_starts"]] == [0, 1, 0], report
            assert next(frame for frame in report["frames"] if frame["kind"] == "publish")["pts_ns"] == 0, report
            assert next(frame for frame in report["frames"] if frame["kind"] == "rollback")["pts_ns"] == report["rollback_pts_ns"], report
            assert all(frame["pts_ns"] == 0 for frame in report["frames"] if frame["source"] == 1 and not frame["visible"]), report
            assert session.capture(other).tobytes() == other_baseline, "other output changed"
            wait_for(empty, "rollback cleanup")

            aborted, log = spawn()
            session.expect_log(aborted, log, "Replacement candidate cached; previous video continues")
            command(aborted, b"abort\n")
            session.expect_log(aborted, log, "Replacement candidate removed; previous video retained")
            moving(aborted, 0)
            aborted.send_signal(signal.SIGINT)
            cancellation = json.loads(finish(aborted))
            assert aborted.returncode == 0 and cancellation["candidate_aborted"], session.diagnostics()
            validate(cancellation)
            assert not cancellation["candidate"]["playback_started"] and not cancellation["candidate_presented"], cancellation
            assert cancellation["candidate"]["delivery"]["received"] == 1, cancellation
            assert cancellation["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), cancellation
            wait_for(empty, "aborted video cleanup")

            invalid_media = directory / "invalid.webm"
            invalid_media.write_bytes(b"not a media container\n")
            failed, log = spawn(invalid_media)
            session.expect_log(failed, log, "Replacement candidate removed; previous video retained")
            moving(failed, 0)
            failed.send_signal(signal.SIGINT)
            failure = json.loads(finish(failed))
            assert failed.returncode == 0 and failure["candidate_failure"] and not failure["candidate_presented"], session.diagnostics()
            validate(failure)
            assert failure["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), failure
            wait_for(empty, "failed candidate leaves previous alive until shutdown")

            for message, error in [(b"rollback\n", "invalid replacement command order"),
                                   (b"unknown\n", "invalid nested diagnostic command"),
                                   (b"publish\n" + b"x"*64, "invalid nested diagnostic command")]:
                invalid, log = spawn()
                session.expect_log(invalid, log, "Replacement candidate cached; previous video continues")
                command(invalid, message)
                assert not finish(invalid).strip() and invalid.returncode != 0, session.diagnostics()
                assert error in session.text(log), session.diagnostics()
                wait_for(empty, "invalid replacement command cleanup")

            interrupted, log = spawn()
            session.expect_log(interrupted, log, "Replacement candidate cached; previous video continues")
            interrupted.send_signal(signal.SIGINT)
            interrupt_report = json.loads(finish(interrupted))
            assert interrupted.returncode == 0 and interrupt_report["status"] == "interrupted", session.diagnostics()
            validate(interrupt_report)
            assert not interrupt_report["candidate"]["playback_started"], interrupt_report
            wait_for(empty, "interrupted replacement cleanup")

            negative, log = spawn(extra=["--intercept-input"])
            assert not finish(negative).strip() and negative.returncode != 0, session.diagnostics()
            assert "replacement surface intercepted private input" in session.text(log), session.diagnostics()
            wait_for(empty, "input negative control cleanup")
            for visible in [False, True]:
                crashed, log = spawn()
                session.expect_log(crashed, log, "Replacement candidate cached; previous video continues")
                if visible:
                    command(crashed, b"publish\n")
                    session.expect_log(crashed, log, "Replacement candidate video presented; pointer reached witness")
                crashed.kill()
                assert not finish(crashed).strip() and crashed.returncode == -signal.SIGKILL
                wait_for(empty, "replacement SIGKILL cleanup")
            if args.nvidia:
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-16_384:]
                native_log = session.text(log)
                assert "using GL memory" in native_log and "fallback to system memory" not in native_log, "missing decoder GL-transfer diagnostics"
            lost, log = spawn()
            session.expect_log(lost, log, "Replacement candidate cached; previous video continues")
            session.process.terminate()
            session.process.wait(timeout=10)
            assert not finish(lost).strip() and lost.returncode != 0, session.diagnostics()
        except Exception:
            if args.artifact_directory:
                args.artifact_directory.mkdir(parents=True, exist_ok=False)
                for path in runtime.iterdir():
                    if path.is_file() and path.suffix in [".log", ".json", ".png"]:
                        shutil.copy2(path, args.artifact_directory / path.name)
            print(session.diagnostics())
            raise
        finally:
            session.close()
    print(json.dumps({"video_replacement":"passed", "gl_api":args.gl_api, "nvidia":args.nvidia,
                      "gl":report["gl"], "visible_markers":[sorted(old_seen),sorted(candidate_seen),sorted(restored_seen)],
                      "sampled_compositor_frames":captures,"max_compositor_channel_error":maximum_error,
                      "egl_lifecycle":report["egl_lifecycle"],"consumer_fences":report["consumer_fences"],
                      "checks":["distinct old/new sources", "old video moves while candidate paused", "presentation-gated clocks",
                                "sampled publication/rollback pixels", "rollback retained PTS", "video moves after restoration",
                                "abort and malformed candidate retain moving previous video", "real input", "unchanged second output",
                                "input negative control", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
