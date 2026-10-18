#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Opt-in, windowless NVDEC GL interop/color qualification on an existing NVIDIA driver."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from PIL import Image
from video_color_fixtures import WIDTH, HEIGHT, PATCH_WIDTH, PATCH_HEIGHT, patches, expected_rgba, enlarged_yuv

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/owned_gl_video_probe")
    parser.add_argument("--nvidia", action="store_true", required=True,
                        help="explicitly enable physical GPU use; no desktop surface is created")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    vendor = Path("/usr/share/glvnd/egl_vendor.d/10_nvidia.json")
    assert vendor.is_file(), "this opt-in test requires an already installed NVIDIA EGL driver"
    results = []
    with tempfile.TemporaryDirectory(prefix="openwall-nvidia-interop-") as temporary:
        directory = Path(temporary)
        env = os.environ.copy()
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "LIBGL_ALWAYS_SOFTWARE"]:
            env.pop(key, None)
        env.update(GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl", EGL_PLATFORM="surfaceless",
                   __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor), GST_PLUGIN_FEATURE_RANK="nvvp9dec:MAX",
                   GST_REGISTRY_1_0=str(directory / "registry.bin"), GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        for matrix in ["bt709", "smpte170m"]:
            for full_range in [False, True]:
                label = f"{matrix}-{'full' if full_range else 'limited'}"
                colors = patches(full_range)
                raw_bytes = enlarged_yuv(colors) + enlarged_yuv([(128, 128, 128)] * 10)
                raw = directory / f"{label}.yuv"
                raw.write_bytes(raw_bytes)
                video = directory / f"{label} ! name='original'.webm"
                tags = ["-colorspace", matrix, "-color_primaries", matrix, "-color_trc", matrix,
                        "-color_range", "pc" if full_range else "tv"]
                subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "rawvideo",
                                "-pixel_format", "yuv420p", "-video_size", f"{WIDTH * 2}x{HEIGHT * 2}",
                                "-framerate", "2", *tags, "-i", str(raw), "-frames:v", "2", "-an", "-c:v", "libvpx-vp9",
                                "-lossless", "1", *tags, str(video)], check=True, capture_output=True, timeout=30)
                decoded = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(video),
                                          "-f", "rawvideo", "-pix_fmt", "yuv420p", *tags, "pipe:1"],
                                         check=True, capture_output=True, timeout=30).stdout
                assert decoded == raw_bytes, "encoder changed original YUV samples"
                for api, gst_api in [("gles", "gles2"), ("desktop", "opengl3")]:
                    capture = directory / f"{label}-{api}.png"
                    run = subprocess.run([str(binary), str(video), "--gl-api", api, "--first-frame", str(capture)],
                                         env=dict(env, GST_GL_API=gst_api), capture_output=True, text=True, timeout=30)
                    assert run.returncode == 0, run.stderr[-32_768:]
                    report = json.loads(run.stdout)
                    assert report["gl_api"] == api and "NVIDIA" in report["gl"]["vendor"], report
                    decoder = next((item for item in report["decoders"] if item["factory"] == "nvvp9dec"), None)
                    assert decoder is not None, ("NVIDIA decoder was not selected", report)
                    actual = decoder["first_actual_buffer"]
                    assert actual and actual["memories"] > 0 and actual["allocator_types"], report
                    explicit_gl = "video/x-raw(memory:GLMemory)" in actual["caps"]
                    fallback = "fallback to system memory" in run.stderr
                    if api == "desktop":
                        assert explicit_gl and actual["all_memory_is_gl"], report
                        # Allocator/caps alone cannot detect a late failed GL copy.
                        # Require the native selection log and reject its explicit
                        # per-frame fallback; changed log formats need investigation.
                        assert "using GL memory" in run.stderr and not fallback, run.stderr[-32_768:]
                    elif report["gstreamer"] == "GStreamer 1.26.11":
                        assert not explicit_gl and fallback and "using system memory" in run.stderr, run.stderr[-32_768:]
                    assert report["consumer_cpu_buffer_maps"] == 0 and report["snapshot_readbacks"] == 1, report
                    assert report["consumer_fences"]["submitted"] == report["consumer_fences"]["retired"] == 1, report
                    assert report["consumer_fences"]["cleanup_finish_calls"] == 0, report
                    assert report["first_frame_capture"]["pts_ns"] == 0, report
                    maximum_error = 0
                    with Image.open(capture) as image:
                        assert image.mode == "RGBA" and image.size == (WIDTH * 2, HEIGHT * 2), image.size
                        for index, yuv in enumerate(colors):
                            expected = expected_rgba(yuv, matrix, full_range)
                            for dx, dy in [(-6, -6), (0, 0), (6, 6)]:
                                point = ((index % 5 * PATCH_WIDTH + PATCH_WIDTH // 2) * 2 + dx,
                                         (index // 5 * PATCH_HEIGHT + PATCH_HEIGHT // 2) * 2 + dy)
                                pixel = image.getpixel(point)
                                error = max(abs(a - b) for a, b in zip(pixel[:3], expected[:3]))
                                maximum_error = max(maximum_error, error)
                                assert pixel[3] == 255 and error <= 1, (label, api, point, pixel, expected, report)
                    results.append({"fixture": label, "api": api, "max_channel_error": maximum_error,
                                    "explicit_gl_memory_contract": explicit_gl, "all_decoder_memory_is_gl": actual["all_memory_is_gl"],
                                    "decoder_allocators": actual["allocator_types"], "native_fallback_warning": fallback,
                                    "gstreamer": report["gstreamer"], "gl": report["gl"]})
    print(json.dumps({"nvidia_gl_interop": "passed", "scope": "two_frame_original_SDR_fixtures_no_presentation_or_performance_claim", "results": results}))


if __name__ == "__main__":
    main()
