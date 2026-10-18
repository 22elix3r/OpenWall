#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Build an app-local, source-included GStreamer 1.28 Wayland helper; no system install."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.28.8"
SOURCE_SHA256 = "670d888bde8242579b0d8e91f60cf337aa7be20ca7c315ebfbdd6e5a9218d104"
SOURCE_URL = f"https://gstreamer.freedesktop.org/src/gst-plugins-bad/gst-plugins-bad-{VERSION}.tar.xz"
PATCH_NAME = "0001-wayland-register-output-once.patch"
PATCH = ROOT / "integrations/gstreamer" / PATCH_NAME
if not PATCH.is_file():
    PATCH = Path(__file__).resolve().parent / PATCH_NAME
NOTICE = ROOT / "integrations/gstreamer/README.md"
if not NOTICE.is_file():
    NOTICE = Path(__file__).resolve().parent / "README.md"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall")
    parser.add_argument("--source-archive", type=Path, help="offline copy of the pinned upstream archive")
    parser.add_argument("--output", type=Path, help="packaging staging directory (normally adjacent to the binary)")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("this helper is only needed on Linux")
    for tool in ["meson", "ninja", "pkg-config", "cc", "patch"]:
        if not shutil.which(tool):
            parser.error(f"missing build tool: {tool}; see integrations/gstreamer/README.md")
    version = subprocess.check_output(["pkg-config", "--modversion", "gstreamer-1.0"], text=True).strip()
    if version.split(".")[:2] != ["1", "28"]:
        parser.error(f"build against GStreamer 1.28 development packages, found {version}")
    binary = args.binary.resolve(strict=True)
    destination = (args.output.resolve() if args.output else
                   binary.parent / "openwall-runtime/gstreamer-1.28")
    destination.parent.mkdir(parents=True, exist_ok=True)
    # A complete directory is renamed into place only after a successful build.
    if destination.exists():
        parser.error(f"runtime already exists: {destination}; move it aside before rebuilding")
    with tempfile.TemporaryDirectory(prefix="openwall-wayland-build-") as temporary:
        work = Path(temporary)
        archive = work / f"gst-plugins-bad-{VERSION}.tar.xz"
        if args.source_archive:
            with args.source_archive.open("rb") as source:
                data = source.read(32 * 1024 * 1024 + 1)
        else:
            with urllib.request.urlopen(SOURCE_URL, timeout=30) as response:
                data = response.read(32 * 1024 * 1024 + 1)
        if len(data) > 32 * 1024 * 1024:
            raise RuntimeError("upstream archive exceeds limits")
        if hashlib.sha256(data).hexdigest() != SOURCE_SHA256:
            raise RuntimeError("upstream archive checksum does not match the pinned source")
        archive.write_bytes(data)
        del data
        with tarfile.open(archive) as source:
            source.extractall(work, filter="data")
        source = work / f"gst-plugins-bad-{VERSION}"
        subprocess.run(["patch", "--batch", "--fuzz=0", "-p1", "--input", str(PATCH)],
                       cwd=source, check=True)
        build = work / "build"
        subprocess.run(["meson", "setup", str(build), str(source), "--buildtype=release",
                        "--wrap-mode=nodownload", "-Dauto_features=disabled", "-Dwayland=enabled",
                        "-Dgpl=disabled"], check=True)
        targets = json.loads(subprocess.check_output(["meson", "introspect", str(build), "--targets"]))
        target = next(item for item in targets if item["name"] == "gstwayland-1.0")
        library = Path(target["filename"][0])
        subprocess.run(["ninja", "-C", str(build), str(library.relative_to(build))], check=True)
        with tempfile.TemporaryDirectory(prefix=".openwall-runtime-", dir=destination.parent) as stage:
            stage = Path(stage)
            shutil.copyfile(library, stage / "libgstwayland-1.0.so.0")
            shutil.copyfile(archive, stage / archive.name)
            shutil.copyfile(source / "COPYING", stage / "COPYING")
            shutil.copyfile(PATCH, stage / PATCH.name)
            shutil.copyfile(Path(__file__), stage / "build_wayland_runtime.py")
            shutil.copyfile(NOTICE, stage / "README.md")
            manifest = {"schema_version": 1, "upstream_version": VERSION, "core_abi": "1.28",
                        "license": "LGPL-2.1-or-later", "source_url": SOURCE_URL,
                        "source_sha256": SOURCE_SHA256,
                        "patch_sha256": hashlib.sha256(PATCH.read_bytes()).hexdigest(),
                        "library_sha256": hashlib.sha256(library.read_bytes()).hexdigest(),
                        "build_core_version": version,
                        "scope": "Wayland helper only; codecs and core use system libraries"}
            (stage / "build.json").write_text(json.dumps(manifest, indent=2) + "\n")
            stage.chmod(0o755)
            stage.rename(destination)
    print(f"Built source-included Wayland helper at {destination}")


if __name__ == "__main__":
    main()
