#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Check GStreamer 1.28's app-local runtime and its fail-closed startup boundary."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall")
    parser.add_argument("--expect-bundled", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="openwall-runtime-test-") as temporary:
        directory = Path(temporary)
        copy = directory / "openwall"
        try:
            os.link(binary, copy)
        except OSError:
            shutil.copyfile(binary, copy)
            copy.chmod(0o755)
        env = os.environ.copy()
        for key in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "LD_PRELOAD"]:
            env.pop(key, None)
        env["GST_REGISTRY_1_0"] = str(directory / "registry.bin")
        runtime_directory = directory / "runtime"
        runtime_directory.mkdir(mode=0o700)
        env["XDG_RUNTIME_DIR"] = str(runtime_directory)

        def run(command, executable=copy):
            return subprocess.run([str(executable), *command], env=env, capture_output=True,
                                  text=True, timeout=25)

        result = run(["doctor"])
        assert result.returncode == 0, result.stderr
        report = json.loads(result.stdout)["linux_video"]
        assert report["gstreamer"].startswith("GStreamer 1.28."), report
        assert not report["available"] and "output-change crash" in report["reason"], report
        video = directory / "original.webm"
        video.write_bytes(b"original invalid media; runtime must be rejected before decoding\n")
        result = run(["apply", str(video), "--seconds", "1"])
        assert result.returncode == 1 and "output-change crash" in result.stderr, result
        assert "First video buffer" not in result.stderr, result

        runtime = directory / "openwall-runtime/gstreamer-1.28"
        runtime.mkdir(parents=True)
        # Invalid metadata must be rejected before attempting to load native code.
        (runtime / "libgstwayland-1.0.so.0").write_bytes(b"not a native library\n")
        for contents, expected in [(b"x" * 16_385, "manifest exceeds limits"),
                                   (b'{"schema_version":1,"core_abi":"1.26"}', "incompatible build metadata")]:
            (runtime / "build.json").write_bytes(contents)
            result = run(["doctor"])
            assert result.returncode == 0, result.stderr
            report = json.loads(result.stdout)["linux_video"]
            assert not report["available"] and expected in report["reason"], report

        if args.expect_bundled:
            runtime = binary.parent / "openwall-runtime/gstreamer-1.28"
            manifest = json.loads((runtime / "build.json").read_text())
            assert manifest["license"] == "LGPL-2.1-or-later", manifest
            for filename, key in [("libgstwayland-1.0.so.0", "library_sha256"),
                                  ("gst-plugins-bad-1.28.8.tar.xz", "source_sha256"),
                                  ("0001-wayland-register-output-once.patch", "patch_sha256")]:
                assert hashlib.sha256((runtime / filename).read_bytes()).hexdigest() == manifest[key], filename
            assert (runtime / "COPYING").is_file() and (runtime / "build_wayland_runtime.py").is_file()
            assert (runtime / "README.md").is_file()
            result = run(["doctor"], binary)
            assert result.returncode == 0, result.stderr
            report = json.loads(result.stdout)["linux_video"]
            assert report["available"] and report["helper"].startswith("app-local"), report
        print(json.dumps({"wayland_runtime_boundary": "passed", "bundled": args.expect_bundled}))


if __name__ == "__main__":
    main()
