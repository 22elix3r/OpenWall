#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""One decoder feeds two private outputs and survives the first native window."""
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
from test_native_gl_video import fixture, identify_pixels
from test_staged_egl import solid
from test_wayland_headless import Session, wait_for

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/shared_video_outputs_probe")
    parser.add_argument("--gl-api", choices=["desktop", "gles"], default="desktop")
    parser.add_argument("--nvidia", action="store_true")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--artifact-directory", type=Path)
    args = parser.parse_args()
    if args.nvidia and args.gl_api != "desktop":
        parser.error("NVIDIA GPU-path qualification requires desktop GL")
    if args.render_node and not args.nvidia:
        parser.error("--render-node requires --nvidia")
    binary = args.binary.resolve(strict=True)
    sway = shutil.which("sway")
    assert sway and shutil.which("swaymsg") and shutil.which("grim") and shutil.which("ffmpeg")
    vendor = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    node, render_env = None, None
    if args.nvidia:
        vendor, node, render_env = nvidia_renderer(args.render_node)
    assert vendor.is_file()
    guard_env = os.environ.copy()
    guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    guard = subprocess.run([str(binary), "missing.webm", "--first", "never-first", "--second", "never-second"],
                           env=guard_env, capture_output=True, text=True, timeout=5)
    assert guard.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guard.stderr and not guard.stdout.strip(), guard
    with tempfile.TemporaryDirectory(prefix="openwall-shared-video-") as temporary:
        directory = Path(temporary)
        video, colors, scale = fixture(directory, args.nvidia)
        session_dir = directory / "session"
        session_dir.mkdir()
        session = Session(session_dir, sway, software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland", GST_GL_WINDOW="surfaceless",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor),
                           GST_GL_PLATFORM="egl", GST_GL_API="opengl3" if args.gl_api == "desktop" else "gles2",
                           GST_PLUGIN_FEATURE_RANK="nvvp9dec:MAX" if args.nvidia else "vp9dec:MAX",
                           GST_REGISTRY_1_0=str(directory / "registry.bin"))
        if args.nvidia:
            session.env.update(GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        try:
            session.connect()
            first, second = sorted(item["name"] for item in session.outputs() if item["active"])
            background = wait_for(lambda: session.capture(first), "initial private baseline").getpixel((10, 10))
            outputs = {}

            def spawn(path=video):
                log = (session.directory / f"shared-video-{len(session.logs)}.log").open("w+")
                result = (session.directory / f"shared-video-{len(session.logs)}.json").open("w+")
                session.logs.extend([log, result])
                child = subprocess.Popen([str(binary), str(path), "--first", first, "--second", second,
                                          "--gl-api", args.gl_api], env=session.env, stdout=result, stderr=log, text=True)
                session.workers.append(child)
                outputs[child.pid] = result
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(outputs[child.pid])

            def empty(name):
                image = session.capture(name)
                return image is not None and solid(image, background)

            seen = [set(), set()]
            maximum_error = 0

            def frame(child, log, index, *, transition=False):
                nonlocal maximum_error
                if child.poll() is not None:
                    return None
                image = session.capture([first, second][index])
                if child.poll() is not None or "Shared video removing private surfaces" in session.text(log):
                    return None
                assert image is not None
                identity, error = identify_pixels(image, colors)
                if error > 3 and transition:
                    return None
                assert error <= 3, ("shared video pixels differ from every original frame", index, error, session.diagnostics())
                maximum_error = max(maximum_error, error)
                seen[index].add(identity)
                return identity, error

            child, log = spawn()
            session.expect_log(child, log, "Shared video presented on both private outputs; pointer reached witness")
            until = time.monotonic() + 1.1
            while time.monotonic() < until:
                assert frame(child, log, 0) is not None and frame(child, log, 1) is not None, session.diagnostics()
                time.sleep(.05)
            session.ipc(f"output {second} mode 800x500")
            wait_for(lambda: frame(child, log, 1, transition=True), "corrected second output resize")
            assert frame(child, log, 0) is not None
            session.ipc(f"output {first} disable")
            session.expect_log(child, log, "Shared video primary retired; decoder retained")
            for command in [f"output {second} scale 1.25", f"output {second} scale 2", f"output {second} transform 90"]:
                session.ipc(command)
                wait_for(lambda: frame(child, log, 1, transition=True), "corrected surviving output geometry")
            until = time.monotonic() + 25
            while child.poll() is None:
                assert time.monotonic() < until, session.diagnostics()
                if frame(child, log, 1) is None:
                    break
                time.sleep(.05)
            report = json.loads(finish(child))
            assert child.returncode == 0 and report["status"] == "passed", session.diagnostics()
            assert report["scope"] == "one_video_decoder_two_private_outputs" and report["completed_loops"] == 2, report
            assert not report["interrupted"] and report["witness_button_events"] >= 2, report
            decoder = "nvvp9dec" if args.nvidia else "vp9dec"
            assert report["decoders"] == [decoder], report
            trace = next(item for item in report["decoder_buffers"].values() if item["factory"] == decoder)
            assert trace["memories"] > 0 and trace["all_memory_is_gl"] == args.nvidia, report
            assert "memory:GLMemory" in report["input_memory"]["caps"], report
            assert report["input_memory"]["memory"]["shared_context"] and report["input_memory"]["memory"]["producer_sync_meta"], report
            assert len(seen[0]) >= 2 and len(seen[1]) >= 4, seen
            assert report["consumer_cpu_buffer_maps"] == report["consumer_readbacks"] == report["consumer_offscreen_targets"] == 0, report
            assert report["source_size"] == [130 * scale, 68 * scale] and report["gl"]["api"] == args.gl_api, report
            assert ("NVIDIA" in report["gl"]["vendor"] if args.nvidia else "llvmpipe" in report["gl"]["renderer"].lower()), report
            before = {"display_initializations": 1, "context_creations": 1, "surface_creations": 2,
                      "surface_destructions": 1, "context_destructions": 0, "display_terminations": 0}
            assert report["first_retired_lifecycle"] == before, report
            assert report["egl_lifecycle"] == dict(before, surface_destructions=2, context_destructions=1, display_terminations=1), report
            fences = report["consumer_fences"]
            assert fences["submitted"] == fences["retired"] == len(report["frames"]), report
            assert 2 <= fences["peak_in_flight"] <= fences["limit"] == 3 and fences["cleanup_finish_calls"] == 0, report
            presented = [set(report["first_presented_ids"]), set(report["second_presented_ids"])]
            for index in range(2):
                records = [record for record in report["frames"] if record["output"] == index]
                assert presented[index] and presented[index] <= {record["id"] for record in records}, report
                for size in {tuple(record["logical_size"]) for record in records}:
                    assert any(tuple(record["logical_size"]) == size and record["id"] in presented[index]
                               and not record["geometry_refresh_required"] for record in records), report
            shared = {}
            for record in report["frames"]:
                shared.setdefault(record["source_serial"], {})[record["output"]] = record
            pairs = [pair for pair in shared.values() if len(pair) == 2]
            assert len(pairs) >= 2 and all((pair[0]["pts_ns"], pair[0]["texture_id"], pair[0]["loop"])
                                         == (pair[1]["pts_ns"], pair[1]["texture_id"], pair[1]["loop"]) for pair in pairs), report
            assert len({pair[0]["pts_ns"] for pair in pairs}) >= 2, report
            survivor = [record for record in report["frames"] if record["after_primary_retired"]
                        and record["output"] == 1 and record["id"] in presented[1]]
            assert len({record["pts_ns"] for record in survivor}) >= 4, report
            assert any(record["loop"] == 1 and record["pts_ns"] == 0 for record in survivor), report
            if args.nvidia:
                native_log = session.text(log)
                assert "memory:GLMemory" in trace["caps"] and "using GL memory" in native_log and "fallback to system memory" not in native_log
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-16_384:]
            wait_for(lambda: empty(second), "surviving video cleanup after EOS")
            session.ipc(f"output {first} enable")
            wait_for(lambda: any(item["name"] == first and item["active"] for item in session.outputs()), "first output reenabled")

            interrupted, log = spawn()
            session.expect_log(interrupted, log, "Shared video presented on both private outputs; pointer reached witness")
            interrupted.send_signal(signal.SIGINT)
            assert json.loads(finish(interrupted))["interrupted"] and interrupted.returncode == 0
            wait_for(lambda: empty(first) and empty(second), "two-output cleanup after SIGINT")
            malformed = directory / "malformed.webm"
            malformed.write_bytes(b"not a video")
            invalid, log = spawn(malformed)
            assert not finish(invalid).strip() and invalid.returncode != 0, session.diagnostics()
            wait_for(lambda: empty(first) and empty(second), "malformed two-output media cleanup")
            crashed, log = spawn()
            session.expect_log(crashed, log, "Shared video presented on both private outputs; pointer reached witness")
            crashed.kill()
            assert not finish(crashed).strip() and crashed.returncode == -signal.SIGKILL
            wait_for(lambda: empty(first) and empty(second), "two-output cleanup after SIGKILL")
            lost, log = spawn()
            session.expect_log(lost, log, "Shared video presented on both private outputs; pointer reached witness")
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
    print(json.dumps({"shared_video_outputs": "passed", "gl_api": args.gl_api, "nvidia": args.nvidia,
                      "gl": report["gl"], "egl_lifecycle": report["egl_lifecycle"], "visible_markers": [sorted(values) for values in seen],
                      "max_compositor_channel_error": maximum_error, "frames": len(report["frames"]), "consumer_fences": fences,
                      "checks": ["one actual decoder", "same texture/PTS shared across outputs", "both outputs animate",
                                 "first native window retired without terminating display/context", "surviving video after mode/scale/rotation",
                                 "loop after draining both output leases", "real input on first output", "SIGINT", "malformed media", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
