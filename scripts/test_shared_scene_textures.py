#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Windowless GPU conformance for image sharing across scene painters."""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/shared_scene_textures_probe")
    parser.add_argument("--software", action="store_true")
    parser.add_argument("--artifact-directory", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    env = os.environ.copy()
    for name in ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DBUS_SESSION_BUS_ADDRESS", "SWAYSOCK"]:
        env.pop(name, None)
    if args.software:
        if platform.system() != "Linux": parser.error("--software requires Linux lavapipe")
        drivers = Path("/usr/share/vulkan/icd.d")
        driver = next((path for path in [drivers / f"lvp_icd.{platform.machine()}.json", drivers / "lvp_icd.json"] if path.is_file()), None)
        assert driver is not None, "lavapipe ICD is missing"
        env.update(VK_DRIVER_FILES=str(driver), VK_ICD_FILENAMES=str(driver))
    with tempfile.TemporaryDirectory(prefix="openwall-shared-scene-") as temporary:
        directory = Path(temporary)
        runtime = directory / "runtime"
        runtime.mkdir(mode=0o700)
        env.update(XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(directory / "config"), XDG_CACHE_HOME=str(directory / "cache"))
        output = args.artifact_directory.resolve() if args.artifact_directory else directory / "renders"
        assert not output.exists(), "artifact directory must not exist"
        result = subprocess.run([str(binary), str(output)], env=env, capture_output=True, text=True, timeout=40)
        if args.artifact_directory and output.is_dir():
            (output / "probe.stdout").write_text(result.stdout)
            (output / "probe.stderr").write_text(result.stderr)
        assert result.returncode == 0, (result.returncode, result.stderr[-16_384:])
        report = json.loads(result.stdout)
        assert report["shared_scene_textures"] == "passed", report
        assert report["shared"] == {"uploads":2,"live_images":2,"live_rgba_bytes":512}, report
        assert report["distinct"] == {"uploads":4,"live_images":4,"live_rgba_bytes":1024}, report
        assert report["retired"] == {"uploads":4,"live_images":0,"live_rgba_bytes":0}, report
        assert report["recreated"] == {"uploads":6,"live_images":2,"live_rgba_bytes":512}, report
        assert all((output / name).is_file() for name in ["shared.png", "distinct.png"]), report
        print(json.dumps(dict(report, software=args.software)))


if __name__ == "__main__":
    main()
