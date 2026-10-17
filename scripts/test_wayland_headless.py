#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Real compositor regressions in a private, headless Sway session.

Requires sway, swaymsg, grim, Pillow, and a Vulkan ICD (Mesa lavapipe works).
No commands, surfaces, or configuration are sent to the user's desktop session.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import tempfile
import time

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
COLORS = [(220, 20, 30), (20, 210, 40), (30, 60, 220), (230, 210, 20)]


def wait_for(check, description, timeout=15):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        value = check()
        if value:
            return value
        time.sleep(.1)
    raise AssertionError(f"Timed out: {description}")


class Session:
    def __init__(self, directory, sway, software_vulkan):
        self.directory = directory
        runtime = directory / "runtime"
        runtime.mkdir(mode=0o700)
        self.env = os.environ.copy()
        if software_vulkan:
            # Fedora uses an architecture suffix; current Ubuntu Mesa uses a generic name.
            driver_dir = Path("/usr/share/vulkan/icd.d")
            candidates = [driver_dir / f"lvp_icd.{platform.machine()}.json",
                          driver_dir / "lvp_icd.json"]
            icd = next((path for path in candidates if path.is_file()), None)
            assert icd, f"Mesa lavapipe ICD not found; installed: {list(driver_dir.glob('*.json'))}"
            self.env.update(VK_DRIVER_FILES=str(icd), VK_ICD_FILENAMES=str(icd))
        for key in ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "SWAYSOCK",
                    "HYPRLAND_INSTANCE_SIGNATURE", "DBUS_SESSION_BUS_ADDRESS"]:
            self.env.pop(key, None)
        self.env.update(XDG_RUNTIME_DIR=str(runtime), WLR_BACKENDS="headless",
                        WLR_HEADLESS_OUTPUTS="2", WLR_RENDERER="pixman",
                        XDG_CURRENT_DESKTOP="sway", XDG_CONFIG_HOME=str(directory / "config"),
                        XDG_CACHE_HOME=str(directory / "cache"))
        config = directory / "sway.conf"
        config.write_text("xwayland disable\nfocus_follows_mouse no\n"
                          "output * mode 640x360\nseat * hide_cursor 10\n")
        self.log = (directory / "sway.log").open("w+")
        self.process = subprocess.Popen([sway, "--config", str(config), "--unsupported-gpu"],
                                        env=self.env, stdout=self.log, stderr=self.log)
        self.workers = []
        self.logs = []

    def connect(self):
        runtime = Path(self.env["XDG_RUNTIME_DIR"])

        def sockets():
            assert self.process.poll() is None, self.diagnostics()
            ipc = list(runtime.glob("sway-ipc.*.sock"))
            wayland = [p for p in runtime.glob("wayland-*") if p.is_socket()]
            return (ipc[0], wayland[0]) if ipc and wayland else None

        ipc, wayland = wait_for(sockets, "private compositor sockets")
        self.env.update(SWAYSOCK=str(ipc), WAYLAND_DISPLAY=wayland.name)
        wait_for(lambda: len(self.outputs()) == 2, "two headless outputs")

    def ipc(self, command, kind="command"):
        result = subprocess.run(["swaymsg", "--socket", self.env["SWAYSOCK"],
                                 "--raw", "--type", kind, command], env=self.env,
                                capture_output=True, text=True, timeout=5, check=True)
        data = json.loads(result.stdout)
        if kind == "command":
            assert all(item.get("success") for item in data), data
        return data

    def outputs(self):
        return self.ipc("", "get_outputs")

    def start_worker(self, binary, path, output=None):
        log = (self.directory / f"worker-{len(self.logs)}.log").open("w+")
        self.logs.append(log)
        command = [str(binary), "apply", str(path)]
        if output:
            command.extend(["--output", output])
        child = subprocess.Popen(command, env=self.env, stdout=log, stderr=log)
        self.workers.append(child)
        return child, log

    @staticmethod
    def text(log):
        log.flush()
        return Path(log.name).read_text(errors="replace")

    def frame(self, output, expected=COLORS):
        image = self.capture(output)
        if image is None:
            return False
        for (x, y), color in zip([(.25, .25), (.75, .25), (.25, .75), (.75, .75)], expected):
            actual = image.getpixel((int(image.width * x), int(image.height * y)))
            if max(abs(a - b) for a, b in zip(actual, color)) > 5:
                return False
        return True

    def capture(self, output):
        path = self.directory / f"{output}.png"
        result = subprocess.run(["grim", "-s", "1", "-o", output, str(path)],
                                env=self.env, capture_output=True, timeout=5)
        if result.returncode:
            return None
        with Image.open(path) as image:
            return image.convert("RGB")

    def video_visible(self, output):
        image = self.capture(output)
        return image is not None and max(image.getextrema()[0]) > 100

    def expect_frame(self, child, output):
        def ready():
            assert child.poll() is None, self.diagnostics()
            return self.frame(output)
        wait_for(ready, f"correct, upright quadrant pixels on {output}")

    def expect_log(self, child, log, text, after=0):
        def ready():
            assert child.poll() is None, self.diagnostics()
            return text in self.text(log)[after:]
        wait_for(ready, text)

    def stop_worker(self, child):
        child.send_signal(signal.SIGINT)
        assert child.wait(timeout=10) == 0, self.diagnostics()

    def diagnostics(self):
        return "\n".join(self.text(log)[-12000:] for log in [self.log, *self.logs])

    def close(self):
        for child in self.workers:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=10)
        for log in [self.log, *self.logs]:
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall")
    parser.add_argument("--sway", default="sway")
    parser.add_argument("--software-vulkan", action="store_true", help="discover and use Mesa lavapipe")
    parser.add_argument("--video-fixture", type=Path)
    args = parser.parse_args()
    for tool in [args.sway, "swaymsg", "grim"]:
        assert shutil.which(tool), f"Missing test dependency: {tool}"
    with tempfile.TemporaryDirectory(prefix="openwall-headless-") as temp:
        directory = Path(temp)
        fixture = directory / "quadrants.png"
        image = Image.new("RGB", (640, 360))
        draw = ImageDraw.Draw(image)
        for rect, color in zip([(0, 0, 319, 179), (320, 0, 639, 179),
                                (0, 180, 319, 359), (320, 180, 639, 359)], COLORS):
            draw.rectangle(rect, fill=color)
        image.save(fixture)
        session = Session(directory, args.sway, args.software_vulkan)
        try:
            session.connect()
            names = sorted(item["name"] for item in session.outputs())
            first, second = names
            session.ipc(f"output {second} mode 800x450 scale 1.25 position 640 0")
            child, log = session.start_worker(args.binary, fixture)
            for name in names:
                session.expect_frame(child, name)
            session.expect_log(child, log, f"{second} (800×450, logical 640×360)")
            time.sleep(.5)
            idle_log = session.text(log)
            time.sleep(1)
            assert session.text(log) == idle_log, "static scene continued presenting while idle"

            # A scale change keeps physical resolution fixed; the logical size changes.
            offset = len(session.text(log))
            session.ipc(f"output {second} mode 960x540 scale 1.5")
            session.expect_log(child, log, f"{second} (960×540, logical 640×360)", offset)
            session.expect_frame(child, second)
            offset = len(session.text(log))
            session.ipc(f"output {second} scale 2")
            session.expect_log(child, log, f"{second} (960×540, logical 480×270)", offset)
            session.expect_frame(child, second)
            offset = len(session.text(log))
            session.ipc(f"output {second} transform 90")
            session.expect_log(child, log, f"{second} (540×960, logical 270×480)", offset)
            session.expect_frame(child, second)

            # Disabling removes the wl_output global; enabling supplies a fresh binding.
            session.ipc(f"output {second} disable")
            wait_for(lambda: len([x for x in session.outputs() if x["active"]]) == 1,
                     "output removal")
            session.expect_frame(child, first)
            offset = len(session.text(log))
            session.ipc(f"output {second} enable")
            session.expect_log(child, log, f"Wallpaper presented on {second}", offset)
            session.expect_frame(child, second)

            # New output after startup, followed by complete output disappearance/recovery.
            session.ipc("create_output")
            added = wait_for(lambda: [x["name"] for x in session.outputs()
                                     if x["name"] not in names], "hot-plugged output")[0]
            session.expect_frame(child, added)
            session.ipc("output * disable")
            assert child.poll() is None, session.diagnostics()
            for name in [*names, added]:
                session.ipc(f"output {name} enable")
                session.expect_frame(child, name)
            session.stop_worker(child)
            wait_for(lambda: not session.frame(first), "graceful surface cleanup")

            # Selected output never places the fixture on the other monitor.
            child, log = session.start_worker(args.binary, fixture, first)
            session.expect_frame(child, first)
            assert not session.frame(second), "worker ignored selected output"
            child.kill()
            child.wait(timeout=10)
            wait_for(lambda: not session.frame(first), "surface cleanup after SIGKILL")

            if args.video_fixture:
                session.ipc(f"output {second} transform normal mode 800x450 scale 1.25")
                child, log = session.start_worker(args.binary, args.video_fixture.resolve(), second)
                session.expect_log(child, log, "First video buffer:")
                session.expect_log(child, log, "Video loop restarted")
                wait_for(lambda: session.video_visible(second), "video pixels at fractional scale")
                decoders = session.text(log).count("GStreamer decoder:")
                offset = len(session.text(log))
                session.ipc(f"output {second} mode 960x540 scale 2")
                session.expect_log(child, log, "logical 480×270", offset)
                session.expect_log(child, log, "Video loop restarted", offset)
                wait_for(lambda: session.video_visible(second), "video pixels after resize")
                assert session.text(log).count("GStreamer decoder:") == decoders, session.diagnostics()
                assert "OpenWall GPU:" not in session.text(log), session.diagnostics()
                session.stop_worker(child)

            # A dead compositor must terminate the worker, with no poll-loop spin.
            child, log = session.start_worker(args.binary, fixture, first)
            session.expect_frame(child, first)
            session.process.kill()
            session.process.wait(timeout=10)
            assert child.wait(timeout=10) != 0, "compositor loss reported success"
            print(json.dumps({"headless_lifecycle": "passed", "checks": [
                "mixed 1x/1.25x", "static idle", "1.5x/2x updates", "rotation", "output disable/enable",
                "new output", "all outputs disable/enable",
                "selected output", "graceful cleanup", "SIGKILL cleanup", "compositor loss"],
                "video_resize_without_decoder_restart": bool(args.video_fixture),
                "vulkan_icd": session.env.get("VK_DRIVER_FILES", "loader default")}, indent=2))
        except BaseException:
            print(session.diagnostics())
            raise
        finally:
            session.close()


if __name__ == "__main__":
    main()
