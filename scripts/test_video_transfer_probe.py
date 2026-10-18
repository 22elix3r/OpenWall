#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Exercise the windowless Rust probe; software GL is an explicit test opt-in."""
import argparse
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/video_transfer_probe")
    parser.add_argument("--video-fixture", type=Path, required=True,
                        help="original two-second VP9/WebM fixture")
    parser.add_argument("--software-gl", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="openwall-transfer-test-") as temporary:
        directory = Path(temporary)
        # Paths are URIs, never interpolated into pipeline syntax.
        video = directory / "original ! name='quoted'.webm"
        shutil.copyfile(args.video_fixture, video)
        env = os.environ.copy()
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "GST_GL_WINDOW"]:
            env.pop(key, None)
        env.update(GST_PLUGIN_FEATURE_RANK="vp9dec:MAX",
                   GST_REGISTRY_1_0=str(directory / "registry.bin"))

        def invoke(mode, path=video, extra_env=None):
            return subprocess.run([str(binary), str(path), "--mode", mode,
                                   "--seconds", "3", "--warmup", "0"],
                                  env=dict(env, **(extra_env or {})), capture_output=True,
                                  text=True, timeout=25)

        modes = [("cpu", {})]
        if args.software_gl:
            mesa = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
            assert mesa.is_file(), "Mesa EGL vendor configuration is required for the software GL test"
            modes.append(("gl-download", {"GST_GL_WINDOW": "surfaceless", "GST_GL_PLATFORM": "egl",
                          "GST_GL_API": "gles2", "EGL_PLATFORM": "surfaceless",
                          "LIBGL_ALWAYS_SOFTWARE": "1", "__EGL_VENDOR_LIBRARY_FILENAMES": str(mesa)}))
        for mode, extra in modes:
            result = invoke(mode, extra_env=extra)
            assert result.returncode == 0, result.stderr[-16_384:]
            report = json.loads(result.stdout)
            assert report["status"] == "passed" and report["scope"] == "decode_convert_cpu_consumer", report
            assert report["mode"] == mode and report["loops"] >= 1, report
            assert report["sample_seconds"] >= 3, report
            assert math.isfinite(report["cpu_percent_one_core"]) and report["cpu_percent_one_core"] >= 0, report
            assert "format=(string)RGBA" in report["first_sink_buffer"]["caps"], report
            assert report["first_sink_buffer"]["bytes"] > 0, report
            assert report["first_sink_buffer"]["allocator_types"], report
            assert not report["first_sink_buffer"]["all_memories_dmabuf"], report
            assert report["consumer"]["operation"] == "map_and_copy_each_frame", report
            assert report["consumer"]["total_frames_copied"] >= 2, report
            assert report["consumer"]["total_bytes_copied"] == (
                report["consumer"]["total_frames_copied"] * report["first_sink_buffer"]["bytes"]), report
            assert any(item["factory"] == "vp9dec" for item in report["decoders"]), report

        for mode in ["gl-download", "dmabuf"]:
            result = invoke(mode)
            assert result.returncode != 0 and "GST_GL_WINDOW=surfaceless" in result.stderr, result
        invalid = directory / "invalid.webm"
        invalid.write_bytes(b"OpenWall original invalid-media test\n")
        result = invoke("cpu", invalid)
        assert result.returncode != 0 and not result.stdout.strip(), result

        # Stop wakes the media wait immediately and cannot publish a completed sample.
        with tempfile.TemporaryFile(mode="w+") as log:
            child = subprocess.Popen([str(binary), str(video), "--seconds", "60", "--warmup", "0"],
                                     env=env, stdout=subprocess.PIPE, stderr=log,
                                     text=True, start_new_session=True)
            try:
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    log.seek(0)
                    diagnostics = log.read(16_384)
                    assert child.poll() is None, diagnostics
                    if "Transfer probe received its first sink buffer" in diagnostics:
                        break
                    time.sleep(.05)
                else:
                    raise AssertionError("probe never reached its sink")
                child.send_signal(signal.SIGINT)
                stdout, _ = child.communicate(timeout=5)
                log.seek(0)
                assert child.returncode != 0 and "probe interrupted" in log.read(16_384), child.returncode
                assert not stdout.strip(), stdout
            finally:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
        print(json.dumps({"video_transfer_probe": "passed", "software_gl": args.software_gl,
                          "checks": ["RGBA buffers", "decoder caps", "looping", "CPU accounting",
                                     "quoted path", "surfaceless guard", "invalid media", "SIGINT"]}))


if __name__ == "__main__":
    main()
