#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Check bounded GL source leases against an original changing video sequence."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

from PIL import Image
from video_color_fixtures import WIDTH, HEIGHT, PATCH_WIDTH, PATCH_HEIGHT, patches, original_yuv, expected_rgba, enlarged_yuv

ROOT = Path(__file__).resolve().parents[1]
FRAMES, FPS = 24, 12


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/owned_gl_video_probe")
    parser.add_argument("--gl-api", choices=["gles", "desktop"], default="gles")
    parser.add_argument("--nvidia", action="store_true",
                        help="explicitly qualify the physical NVIDIA GPU instead of software GL")
    args = parser.parse_args()
    if args.nvidia and args.gl_api != "desktop":
        parser.error("NVIDIA GPU-path lease qualification requires --gl-api desktop")
    binary = args.binary.resolve(strict=True)
    results = []
    with tempfile.TemporaryDirectory(prefix="openwall-gl-sequence-") as temporary:
        directory = Path(temporary)
        vendor = Path("/usr/share/glvnd/egl_vendor.d") / ("10_nvidia.json" if args.nvidia else "50_mesa.json")
        assert vendor.is_file(), "the selected EGL vendor configuration is required"
        scale = 2 if args.nvidia else 1
        width, height = WIDTH * scale, HEIGHT * scale
        decoder_name = "nvvp9dec" if args.nvidia else "vp9dec"
        env = os.environ.copy()
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"]:
            env.pop(key, None)
        env.update(GST_PLUGIN_FEATURE_RANK=f"{decoder_name}:MAX", GST_REGISTRY_1_0=str(directory / "registry.bin"),
                   GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl", GST_GL_API=("opengl3" if args.gl_api == "desktop" else "gles2"),
                   EGL_PLATFORM="surfaceless", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor))
        if args.nvidia:
            env.pop("LIBGL_ALWAYS_SOFTWARE", None)
            env.update(GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        else:
            env["LIBGL_ALWAYS_SOFTWARE"] = "1"
        colors = []
        base = patches(False)
        for index in range(FRAMES):
            frame = base[index % len(base):] + base[:index % len(base)]
            frame[0] = (16 + 8 * index, 128, 128)  # A unique frame identity, in addition to moving patches.
            colors.append(frame)
        raw_bytes = b"".join((enlarged_yuv if args.nvidia else original_yuv)(frame) for frame in colors)
        raw = directory / "original.yuv"
        raw.write_bytes(raw_bytes)
        video = directory / "original ! name='sequence'.webm"
        tags = ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]
        subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "rawvideo",
                        "-pixel_format", "yuv420p", "-video_size", f"{width}x{height}", "-framerate", str(FPS),
                        *tags, "-i", str(raw), "-frames:v", str(FRAMES), "-an", "-c:v", "libvpx-vp9",
                        "-lossless", "1", *tags, str(video)], check=True, capture_output=True, timeout=30)
        decoded = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(video),
                                  "-f", "rawvideo", "-pix_fmt", "yuv420p", *tags, "pipe:1"],
                                 check=True, capture_output=True, timeout=30).stdout
        assert decoded == raw_bytes, "encoder changed the source YUV samples"

        def command(label, frames=FRAMES, paced=False):
            return [str(binary), str(video), "--gl-api", args.gl_api, "--frames", str(frames),
                    "--first-frame", str(directory / f"{label}-first.png"),
                    "--atlas", str(directory / f"{label}-atlas.png"), *(["--paced"] if paced else [])]

        for paced in [False, True]:
            label = "paced" if paced else "unpaced"
            run = subprocess.run(command(label, paced=paced), env=env,
                                 capture_output=True, text=True, timeout=30)
            assert run.returncode == 0, run.stderr[-16_384:]
            report = json.loads(run.stdout)
            assert report["status"] == "passed" and report["scope"] == "bounded_shared_texture_sequence_into_owned_atlas", report
            assert report["consumer_cpu_buffer_maps"] == 0 and report["snapshot_readbacks"] == 2, report
            assert report["gl_api"] == args.gl_api, report
            assert ("NVIDIA" in report["gl"]["vendor"] if args.nvidia else
                    "llvmpipe" in report["gl"]["renderer"].lower()), report
            decoder = next((item for item in report["decoders"] if item["factory"] == decoder_name), None)
            assert decoder, report
            if args.nvidia:
                assert "video/x-raw(memory:GLMemory)" in decoder["first_actual_buffer"]["caps"], report
                assert decoder["first_actual_buffer"]["all_memory_is_gl"], report
                assert "using GL memory" in run.stderr and "fallback to system memory" not in run.stderr, run.stderr[-16_384:]
            fences = report["consumer_fences"]
            assert fences["submitted"] == fences["retired"] == FRAMES, report
            assert 1 <= fences["peak_in_flight"] <= fences["limit"] == 3, report
            assert fences["cleanup_finish_calls"] == 0, report
            assert len(report["frames"]) == FRAMES and report["paced"] == paced, report
            for index, frame in enumerate(report["frames"]):
                assert frame["pts_ns"] is not None and abs(frame["pts_ns"] - index * 1e9 / FPS) <= 500_001, report
            unique = len({frame["texture_id"] for frame in report["frames"]})
            assert unique < FRAMES, ("test did not exercise producer texture reuse", report)
            maximum_error = 0
            with Image.open(directory / f"{label}-atlas.png") as atlas:
                assert atlas.mode == "RGBA" and atlas.size == (width, height * FRAMES), atlas.size
                for frame_index, frame_colors in enumerate(colors):
                    for patch, yuv in enumerate(frame_colors):
                        expected = expected_rgba(yuv, "bt709", False)
                        for dx, dy in [(-3, -3), (0, 0), (3, 3)]:
                            point = ((patch % 5 * PATCH_WIDTH + PATCH_WIDTH // 2 + dx) * scale,
                                     frame_index * height + (patch // 5 * PATCH_HEIGHT + PATCH_HEIGHT // 2 + dy) * scale)
                            actual = atlas.getpixel(point)
                            assert actual[3] == 255, (frame_index, point, actual)
                            error = max(abs(a - b) for a, b in zip(actual[:3], expected[:3]))
                            maximum_error = max(maximum_error, error)
                            assert error <= 1, (frame_index, point, actual, expected, report)
                with Image.open(directory / f"{label}-first.png") as first:
                    assert first.tobytes() == atlas.crop((0, 0, width, height)).tobytes()
            results.append({"paced": paced, "frames": FRAMES, "unique_source_textures": unique,
                            "peak_in_flight": fences["peak_in_flight"], "max_channel_error": maximum_error})

        incomplete = subprocess.run(command("incomplete", frames=FRAMES + 1), env=env,
                                    capture_output=True, text=True, timeout=30)
        assert incomplete.returncode != 0 and "ended before" in incomplete.stderr, incomplete
        assert not incomplete.stdout.strip() and not (directory / "incomplete-first.png").exists()
        assert not (directory / "incomplete-atlas.png").exists()

        for extra in [["--frames", "121"], ["--frames", "2"]]:
            invalid = subprocess.run([str(binary), str(video), "--first-frame", str(directory / "invalid.png"), *extra],
                                     env=env, capture_output=True, text=True, timeout=5)
            assert invalid.returncode != 0 and not invalid.stdout.strip(), invalid
            assert not (directory / "invalid.png").exists()

        with tempfile.TemporaryFile(mode="w+") as log:
            child = subprocess.Popen(command("interrupt", paced=True), env=env,
                                     stdout=subprocess.PIPE, stderr=log, text=True, start_new_session=True)
            try:
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    log.seek(0, os.SEEK_END)
                    length = log.tell()
                    log.seek(max(0, length - 16_384))
                    diagnostics = log.read(16_384)
                    assert child.poll() is None, diagnostics
                    if "Owned GL probe drew its first shared texture" in diagnostics:
                        break
                    time.sleep(.01)
                else:
                    raise AssertionError("probe did not draw its first texture")
                child.send_signal(signal.SIGINT)
                stdout, _ = child.communicate(timeout=5)
                log.seek(0, os.SEEK_END)
                length = log.tell()
                log.seek(max(0, length - 16_384))
                assert child.returncode != 0 and "owned GL probe interrupted" in log.read(16_384), child.returncode
                assert not stdout.strip() and not (directory / "interrupt-first.png").exists()
                assert not (directory / "interrupt-atlas.png").exists()
            finally:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
    print(json.dumps({"owned_gl_sequence": "passed", "nvidia": args.nvidia, "gl_api": args.gl_api, "results": results,
                      "checks": ["per-frame reference pixels", "PTS order", "producer texture reuse",
                                 "bounded fence retirement", "no steady-state glFinish", "incomplete input", "SIGINT"]}))


if __name__ == "__main__":
    main()
