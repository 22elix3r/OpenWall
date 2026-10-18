#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Check windowless video conversion against original lossless SDR color fixtures."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
WIDTH, HEIGHT = 130, 68  # Deliberately not a common decoder/GL row alignment.
PATCH_WIDTH, PATCH_HEIGHT = 26, 34


def patches(full_range):
    return [(0 if full_range else 16, 128, 128),
            (255 if full_range else 235, 128, 128),
            (81, 90, 240), (145, 54, 34), (41, 240, 110),
            (126, 128, 128), (200, 80, 190), (64, 180, 90),
            (100, 160, 160), (180, 100, 100)]


def original_yuv(colors):
    planes = []
    for component, scale in [(0, 1), (1, 2), (2, 2)]:
        plane = bytearray()
        for row in range(HEIGHT // scale):
            for column in range(WIDTH // scale):
                patch = (row * scale // PATCH_HEIGHT) * 5 + column * scale // PATCH_WIDTH
                plane.append(colors[patch][component])
        planes.append(bytes(plane))
    return b"".join(planes)


def expected_rgba(yuv, matrix, full_range):
    # Independently evaluate the BT.601/BT.709 Y'CbCr equations in floating point.
    # No conversion implementation from OpenWall, GStreamer or FFmpeg is reused.
    kr, kb = (.2126, .0722) if matrix == "bt709" else (.299, .114)
    y = yuv[0] / 255 if full_range else (yuv[0] - 16) / 219
    cb, cr = ((value - 128) / (255 if full_range else 224) for value in yuv[1:])
    red, blue = y + 2 * (1 - kr) * cr, y + 2 * (1 - kb) * cb
    green = (y - kr * red - kb * blue) / (1 - kr - kb)
    return tuple(round(max(0, min(1, value)) * 255) for value in [red, green, blue]) + (255,)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/video_transfer_probe")
    parser.add_argument("--software-gl", action="store_true")
    parser.add_argument("--owned-gl-binary", type=Path,
                        help="also check the experimental single-frame shared-texture consumer")
    args = parser.parse_args()
    if args.owned_gl_binary and not args.software_gl:
        parser.error("--owned-gl-binary requires --software-gl for isolated Mesa checks")
    binary = args.binary.resolve(strict=True)
    owned_gl = args.owned_gl_binary.resolve(strict=True) if args.owned_gl_binary else None
    results = []
    with tempfile.TemporaryDirectory(prefix="openwall-color-test-") as temporary:
        directory = Path(temporary)
        env = os.environ.copy()
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "GST_GL_WINDOW"]:
            env.pop(key, None)
        env.update(GST_PLUGIN_FEATURE_RANK="vp9dec:MAX",
                   GST_REGISTRY_1_0=str(directory / "registry.bin"))
        modes = [("cpu", {})]
        if args.software_gl:
            mesa = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
            assert mesa.is_file(), "Mesa EGL configuration is required for the software GL test"
            modes.append(("gl-download", {"GST_GL_WINDOW": "surfaceless", "GST_GL_PLATFORM": "egl",
                          "GST_GL_API": "gles2", "EGL_PLATFORM": "surfaceless",
                          "LIBGL_ALWAYS_SOFTWARE": "1", "__EGL_VENDOR_LIBRARY_FILENAMES": str(mesa)}))
            if owned_gl:
                modes.append(("owned-gl", modes[-1][1]))
        for matrix in ["bt709", "smpte170m"]:
            for full_range in [False, True]:
                label = f"{matrix}-{'full' if full_range else 'limited'}"
                colors = patches(full_range)
                raw = directory / f"{label}.yuv"
                # A distinct second frame makes accidental last-frame capture fail.
                raw.write_bytes(original_yuv(colors) + original_yuv([(128, 128, 128)] * 10))
                video = directory / f"{label} ! name='quoted'.webm"
                tags = ["-colorspace", matrix, "-color_primaries", matrix,
                        "-color_trc", matrix, "-color_range", "pc" if full_range else "tv"]
                subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "rawvideo",
                                "-pixel_format", "yuv420p", "-video_size", f"{WIDTH}x{HEIGHT}",
                                "-framerate", "2", *tags, "-i", str(raw), "-frames:v", "2",
                                "-an", "-c:v", "libvpx-vp9", "-lossless", "1",
                                *tags, str(video)], check=True, capture_output=True, timeout=30)
                # Verify that the encoder has preserved the original YUV samples.
                decoded = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error",
                                          "-i", str(video), "-frames:v", "1", "-f", "rawvideo",
                                          "-pix_fmt", "yuv420p", *tags, "pipe:1"],
                                         check=True, capture_output=True, timeout=30).stdout
                assert decoded == original_yuv(colors), label
                for mode, extra in modes:
                    capture = directory / f"{label}-{mode}.png"
                    command = ([str(owned_gl), str(video)] if mode == "owned-gl" else
                               [str(binary), str(video), "--mode", mode, "--seconds", "1", "--warmup", "0"])
                    run = subprocess.run([*command, "--first-frame", str(capture)],
                                         env=dict(env, **extra), capture_output=True, text=True, timeout=25)
                    assert run.returncode == 0, (label, mode, run.stderr[-16_384:])
                    report = json.loads(run.stdout)
                    assert report["first_frame_capture"]["pts_ns"] == 0, report
                    assert report["first_frame_capture"]["bytes"] == WIDTH * HEIGHT * 4, report
                    assert any(item["factory"] == "vp9dec" for item in report["decoders"]), report
                    if mode == "owned-gl":
                        assert report["scope"] == "single_shared_texture_draw_into_owned_fbo", report
                        assert report["consumer_cpu_buffer_maps"] == 0 and report["snapshot_readbacks"] == 1, report
                        assert report["input_memory"]["shared_context"] and report["input_memory"]["producer_sync_meta"], report
                        assert report["input_memory"]["allocator_type"].startswith("GstGLMemory"), report
                        assert "llvmpipe" in report["gl"]["renderer"].lower(), report
                    # This exact CI runtime selects its limited BT.601 coefficients
                    # for full-range input. Check that known behavior independently;
                    # never count it as color conformance or relax the reference limit.
                    legacy_full_range = (mode == "gl-download" and full_range
                                         and report["gstreamer"] == "GStreamer 1.24.2")
                    with Image.open(capture) as frame:
                        assert frame.mode == "RGBA" and frame.size == (WIDTH, HEIGHT), (frame.mode, frame.size)
                        maximum_error = 0
                        for index, yuv in enumerate(colors):
                            expected = expected_rgba(yuv, matrix, full_range)
                            # Sample patch interiors; this is not a chroma-resampling test.
                            for dx, dy in [(-3, -3), (0, 0), (3, 3)]:
                                point = (index % 5 * PATCH_WIDTH + PATCH_WIDTH // 2 + dx,
                                         index // 5 * PATCH_HEIGHT + PATCH_HEIGHT // 2 + dy)
                                actual = frame.getpixel(point)
                                assert actual[3] == 255, (label, mode, point, actual)
                                error = max(abs(a - b) for a, b in zip(actual[:3], expected[:3]))
                                maximum_error = max(maximum_error, error)
                                # Preserve a bound on the existing CPU fast converter's
                                # bias; this does not qualify its precision as GL parity.
                                if legacy_full_range:
                                    legacy_expected = expected_rgba(yuv, "smpte170m", False)
                                    legacy_error = max(abs(a - b) for a, b in zip(actual[:3], legacy_expected[:3]))
                                    assert legacy_error <= 1, (label, mode, point, actual, legacy_expected, report)
                                else:
                                    budget = 6 if mode == "cpu" else 1
                                    assert error <= budget, (label, mode, point, actual, expected, report)
                    results.append({"fixture": label, "mode": mode, "max_channel_error": maximum_error,
                                    "within_one_level_of_reference": maximum_error <= 1,
                                    "known_upstream_full_range_error": legacy_full_range})
        incompatible = subprocess.run([str(binary), str(video), "--mode", "dmabuf",
                                       "--first-frame", str(directory / "forbidden.png")],
                                      env=env, capture_output=True, text=True, timeout=5)
        assert incompatible.returncode != 0 and "cannot map" in incompatible.stderr, incompatible
        assert not incompatible.stdout.strip(), incompatible.stdout
        assert not (directory / "forbidden.png").exists()
        if owned_gl:
            extra = modes[-1][1]

            def reject(path, output, environment):
                failed = subprocess.run([str(owned_gl), str(path), "--first-frame", str(output)],
                                        env=environment, capture_output=True, text=True, timeout=25)
                assert failed.returncode != 0 and not failed.stdout.strip(), failed
                return failed

            guard = reject(video, directory / "no-window.png", env)
            assert "GST_GL_WINDOW=surfaceless" in guard.stderr and not (directory / "no-window.png").exists(), guard
            invalid = directory / "invalid.webm"
            invalid.write_bytes(b"OpenWall original invalid-media test\n")
            reject(invalid, directory / "invalid.png", dict(env, **extra))
            assert not (directory / "invalid.png").exists()
            existing = directory / "existing.png"
            existing.write_bytes(b"preserve this existing file")
            reject(video, existing, dict(env, **extra))
            link = directory / "existing-link.png"
            link.symlink_to(existing)
            reject(video, link, dict(env, **extra))
            assert existing.read_bytes() == b"preserve this existing file" and link.is_symlink()
    print(json.dumps({"video_color_regressions": "passed",
                      "all_within_one_level_of_reference": all(
                          result["within_one_level_of_reference"] for result in results),
                      "results": results}))


if __name__ == "__main__":
    main()
