#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Opt-in GNOME 49 background presentation probe with private D-Bus/XDG state.

Requires GNOME Shell, Mutter, PyGObject, Pillow, Mesa software drivers and bubblewrap.
No commands are sent to the user's GNOME session. GPU devices are hidden from this
test, and the shell's Eval/unsafe mode is enabled only inside its private test bus.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from xml.sax.saxutils import escape

from gi.repository import Gio, GLib
from PIL import Image, ImageDraw

from test_wayland_headless import COLORS, ROOT, wait_for


def evaluate(bus, code):
    reply = bus.call_sync("org.gnome.Shell", "/org/gnome/Shell", "org.gnome.Shell", "Eval",
                          GLib.Variant("(s)", (code,)), GLib.VariantType("(bs)"),
                          Gio.DBusCallFlags.NONE, 5000, None)
    success, value = reply.unpack()
    assert success, value
    return json.loads(value) if value else None


class Probe:
    def __init__(self, bus, directory, binary):
        self.bus, self.directory, self.binary = bus, directory, binary
        self.shots = 0

    def status(self):
        return evaluate(self.bus, "global._openwallProbe.status()")

    def start(self, path, environment):
        args = ",".join(json.dumps(str(arg)) if isinstance(arg, Path) else json.dumps(arg)
                        for arg in [self.binary, path, environment])
        evaluate(self.bus, f"global._openwallProbe.start({args})")

        def ready():
            status = self.status()
            assert status["state"] in ["starting", "mapped"], status
            return status["hasClone"] and status["ownsWindow"] and status["minimized"]

        wait_for(ready, "owned, minimized source cloned in GNOME background")

    def stop(self):
        started = time.monotonic()
        evaluate(self.bus, "global._openwallProbe.stop()")
        assert time.monotonic() - started < 4, "worker stop exceeded its budget"
        status = self.status()
        assert status["state"] == "idle" and not status["hasClone"] and status["pid"] is None, status

    def capture(self):
        self.shots += 1
        path = self.directory / f"frame-{self.shots}.png"
        reply = self.bus.call_sync("org.gnome.Shell.Screenshot", "/org/gnome/Shell/Screenshot",
                                  "org.gnome.Shell.Screenshot", "Screenshot",
                                  GLib.Variant("(bbs)", (False, False, str(path))),
                                  GLib.VariantType("(bs)"), Gio.DBusCallFlags.NONE, 10000, None)
        assert reply.unpack()[0], reply.unpack()
        with Image.open(path) as image:
            return image.convert("RGB")

    def scene_visible(self):
        image = self.capture()
        return all(max(abs(a - b) for a, b in zip(
            image.getpixel((int(image.width * x), int(image.height * y))), color)) <= 5
            for (x, y), color in zip([(.25, .25), (.75, .25), (.25, .75), (.75, .75)], COLORS))


def software_plugins(directory, plugin_dir):
    """Use only the open-source software VP9/Wayland test path, without GPU probes."""
    if not plugin_dir:
        candidates = [Path("/usr/lib64/gstreamer-1.0"),
                      Path(f"/usr/lib/{platform.machine()}-linux-gnu/gstreamer-1.0")]
        plugin_dir = next((candidate for candidate in candidates
                           if (candidate / "libgstwaylandsink.so").is_file()), None)
    assert plugin_dir, "Pass --gst-plugin-dir with the GStreamer plugin directory"
    destination = directory / "plugins"
    destination.mkdir()
    for name in ["coreelements", "playback", "typefindfunctions", "matroska", "vpx",
                 "videoconvertscale", "autodetect", "waylandsink", "videoparsersbad"]:
        source = plugin_dir / f"libgst{name}.so"
        assert source.is_file(), f"Missing software test plugin: {source}"
        (destination / source.name).symlink_to(source.resolve())
    return {"GST_PLUGIN_SYSTEM_PATH_1_0": str(destination), "GST_PLUGIN_PATH_1_0": ""}


def private_test(directory):
    # Fail closed before obtaining a bus or evaluating any shell code.
    assert os.environ["XDG_RUNTIME_DIR"] == str(directory / "runtime")
    assert str(directory / "runtime") in os.environ["DBUS_SESSION_BUS_ADDRESS"]
    options = json.loads((directory / "options.json").read_text())
    # Even OS-service probes target the empty private bus rather than real power,
    # authentication or networking services.
    os.environ["DBUS_SYSTEM_BUS_ADDRESS"] = os.environ["DBUS_SESSION_BUS_ADDRESS"]
    log = (directory / "shell.log").open("w+")
    command = ["bwrap", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
               "--die-with-parent", "--unshare-pid", "--bind", str(directory), str(directory),
               options["shell"], "--headless", "--wayland", "--no-x11", "--virtual-monitor",
               "640x360", "--wayland-display", "openwall-gnome-test", "--unsafe-mode"]
    shell = subprocess.Popen(command, stdout=log, stderr=log)
    outsiders = []
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    probe = None
    try:
        def ready():
            assert shell.poll() is None, "private GNOME Shell exited"
            try:
                return evaluate(bus, "!Main.layoutManager._startingUp && Main.layoutManager._bgManagers.length === 1")
            except GLib.Error:
                return False

        wait_for(ready, "private GNOME background", timeout=35)
        uri = (ROOT / "integrations/gnome/presentation.js").as_uri()
        evaluate(bus, "(async () => { const {PresentationProbe} = await import(" + json.dumps(uri)
                 + "); Main.overview.hide(); global._openwallProbe = new PresentationProbe(); })()")
        probe = Probe(bus, directory, Path(options["binary"]))
        fixture = directory / "quadrants.png"
        env = {"VK_DRIVER_FILES": options["icd"], "VK_ICD_FILENAMES": options["icd"]}
        probe.start(fixture, env)
        wait_for(probe.scene_visible, "GNOME scene quadrant pixels")
        probe.capture().save(directory / "scene.png")

        # An identically titled ordinary client is not a wallpaper. Classification
        # must use the dedicated Wayland connection, not a title or PID heuristic.
        outsider_env = dict(os.environ, **env, WAYLAND_DISPLAY="openwall-gnome-test")
        outsider_env.pop("WAYLAND_SOCKET", None)
        outsider = subprocess.Popen([options["binary"], "preview", str(fixture)],
                                    env=outsider_env, stdout=log, stderr=log)
        outsiders.append(outsider)

        def unrelated_window():
            assert outsider.poll() is None, "ordinary preview exited unexpectedly"
            return evaluate(bus, "global.get_window_actors().some(a => "
                            "a.meta_window.title?.includes('quadrants.png') && "
                            "!global._openwallProbe._run.client.owns_window(a.meta_window) && "
                            "!a.meta_window.minimized)")

        wait_for(unrelated_window, "ordinary same-title preview remains unmanaged")
        outsider.send_signal(signal.SIGINT)
        assert outsider.wait(timeout=10) == 0
        wait_for(probe.scene_visible, "wallpaper retained after ordinary preview closes")
        probe.stop()
        wait_for(lambda: not probe.scene_visible(), "native static background after stop")

        # Superseded startup requests and stop during startup cannot leak workers.
        start = "global._openwallProbe.start(" + ",".join(json.dumps(arg) for arg in
                [options["binary"], str(fixture), env]) + ")"
        evaluate(bus, f"Promise.all([{start}, global._openwallProbe.stop()])")
        assert probe.status()["pid"] is None, probe.status()
        evaluate(bus, f"Promise.all([{start}, {start}])")
        wait_for(lambda: probe.status()["hasClone"], "newest startup request")
        wait_for(probe.scene_visible, "scene after startup cancellation")
        evaluate(bus, "global._openwallProbe._run.process.force_exit()")
        wait_for(lambda: probe.status()["pid"] is None, "mapped worker crash cleanup")
        wait_for(lambda: not probe.scene_visible(), "fallback after mapped worker crash")
        probe.start(fixture, env)
        wait_for(probe.scene_visible, "scene after worker crash")
        evaluate(bus, "global._openwallProbe._run.clone.destroy()")
        wait_for(lambda: probe.status()["pid"] is None, "presentation actor loss stops worker")
        probe.start(fixture, env)
        wait_for(probe.scene_visible, "scene after presentation actor loss")
        probe.stop()

        if options["video"]:
            video_env = software_plugins(directory, Path(options["gst_plugins"]) if options["gst_plugins"] else None)
            probe.start(Path(options["video"]), video_env)
            wait_for(lambda: probe.status()["diagnostics"].count("Video loop restarted") >= 2,
                     "native GNOME background video loops")
            frames = []
            for index in range(2):
                image = probe.capture()
                image.save(directory / f"video-{index}.png")
                # Exclude the native panel and its clock from the motion comparison.
                frames.append(image.crop((0, 40, image.width, image.height)).tobytes())
                time.sleep(.3)
            assert frames[0] != frames[1], "background video is frozen"
            assert "OpenWall GPU:" not in probe.status()["diagnostics"]
            assert "First video buffer:" in probe.status()["diagnostics"]
            assert "Native video window configured" in probe.status()["diagnostics"]
            assert "parent 1×1" in probe.status()["diagnostics"]
            probe.stop()

            invalid = directory / "invalid.webm"
            invalid.write_bytes(b"OpenWall invalid video fixture\n")
            evaluate(bus, "global._invalidVideoMaps = 0; global._invalidVideoSignal = "
                     "global.window_manager.connect_after('map', (_wm, actor) => { "
                     "const run = global._openwallProbe._run; "
                     "if (run?.client.owns_window(actor.meta_window)) global._invalidVideoMaps++; });")
            evaluate(bus, "global._openwallProbe.start(" + ",".join(json.dumps(arg) for arg in
                     [options["binary"], str(invalid), video_env]) + ")")
            wait_for(lambda: probe.status()["pid"] is None, "invalid video fails before mapping")
            assert evaluate(bus, "global._invalidVideoMaps") == 0, "failed video mapped a black window"
            assert "video playback failed" in probe.status()["diagnostics"], probe.status()
            evaluate(bus, "global.window_manager.disconnect(global._invalidVideoSignal)")

        # A rejected worker exits without leaving presentation objects behind.
        sleeper = shutil.which("sleep")
        assert sleeper
        evaluate(bus, "global._openwallProbe.start(" + json.dumps(sleeper) + ","
                 + json.dumps(str(fixture)) + ",{})")
        wait_for(lambda: probe.status()["pid"] is None, "failed worker cleanup")
        # A worker that never maps and ignores SIGINT must be forcibly reaped.
        # Its startup deadline must not block the shell's GLib main loop.
        hung = directory / "hung-worker"
        hung.write_text("#!/usr/bin/python3\nimport os,signal,time\n"
                        "signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
                        "os.write(1, b'Q' * 262144 + b'OPENWALL-NOISE-END\\n')\n"
                        "while True: time.sleep(1)\n")
        hung.chmod(0o700)
        started = time.monotonic()
        evaluate(bus, "global._openwallProbe.start(" + json.dumps(str(hung)) + ","
                 + json.dumps(str(fixture)) + ",{})")
        wait_for(lambda: "OPENWALL-NOISE-END" in probe.status()["diagnostics"], "bounded noisy worker drain")
        assert len(probe.status()["diagnostics"]) <= 16384, "worker diagnostics exceeded their bound"
        wait_for(lambda: probe.status()["pid"] is None, "bounded startup timeout and forced worker exit")
        assert 9 < time.monotonic() - started < 15, "startup deadline was not exercised"

        # This ordinary worker lives outside bubblewrap so compositor death, rather
        # than PID namespace teardown, must make its Wayland event loop fail.
        outsider = subprocess.Popen([options["binary"], "preview", str(fixture)],
                                    env=outsider_env, stdout=log, stderr=log)
        outsiders.append(outsider)
        wait_for(lambda: evaluate(bus, "global.get_window_actors().some(a => "
                 "a.meta_window.title?.includes('quadrants.png'))"), "worker before compositor loss")
        shell.terminate()
        shell.wait(timeout=5)
        assert outsider.wait(timeout=10) != 0, "compositor loss reported success"
        assert "committed initial non-empty content without acknowledging configuration" not in (directory / "shell.log").read_text(), "video violated the initial XDG configure handshake"
        assert "Client provided invalid window geometry" not in (directory / "shell.log").read_text(), "video committed invalid XDG geometry"
        print(json.dumps({"gnome_background_probe": "passed", "video": bool(options["video"]),
                          "checks": ["scene pixels", "connection ownership", "ordinary client preserved",
                                     "fallback", "startup cancellation", "worker cleanup",
                                     "worker crash", "presentation actor loss", "bounded noisy diagnostics",
                                     "unresponsive worker deadline", "compositor loss"]}), flush=True)
    finally:
        if probe and shell.poll() is None:
            try:
                probe.stop()
            except (GLib.Error, AssertionError):
                pass
        for outsider in outsiders:
            if outsider.poll() is None:
                outsider.kill()
            outsider.wait(timeout=5)
        if shell.poll() is None:
            shell.terminate()
        try:
            shell.wait(timeout=5)
        except subprocess.TimeoutExpired:
            shell.kill()
            shell.wait(timeout=5)
        log.close()


def run(args):
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    with tempfile.TemporaryDirectory(prefix="owg-") as temporary:
        directory = Path(temporary)
        for name in ["runtime", "config", "data", "cache", "state", "desktop", "services", "schemas"]:
            (directory / name).mkdir(mode=0o700)
        (directory / "config/user-dirs.dirs").write_text("\n".join(
            f'XDG_{name}_DIR="{directory}/desktop"' for name in ["DESKTOP", "DOWNLOAD", "DOCUMENTS",
                "MUSIC", "PICTURES", "PUBLICSHARE", "TEMPLATES", "VIDEOS"]))
        for source_dir in [Path("/usr/share/glib-2.0/schemas"), args.schema_dir]:
            if source_dir:
                for schema in source_dir.glob("*.xml"):
                    shutil.copyfile(schema, directory / "schemas" / schema.name)
        (directory / "schemas/99_openwall-test.gschema.override").write_text(
            "[org.gnome.desktop.notifications]\nshow-banners=false\n"
            "[org.gnome.shell]\nwelcome-dialog-last-shown-version='99999'\n"
            "[org.gnome.desktop.interface]\nenable-animations=false\n")
        subprocess.run(["glib-compile-schemas", str(directory / "schemas")], check=True,
                       capture_output=True, timeout=10)
        runtime = escape(str(directory / "runtime"))
        services = escape(str(directory / "services"))
        (directory / "dbus.conf").write_text(f'''<busconfig><type>session</type>
<listen>unix:tmpdir={runtime}</listen><auth>EXTERNAL</auth><servicedir>{services}</servicedir>
<policy context="default"><allow own="*"/><allow send_destination="*"/>
<allow receive_sender="*"/></policy></busconfig>''')
        drivers = Path("/usr/share/vulkan/icd.d")
        icd = next((candidate for candidate in [drivers / f"lvp_icd.{platform.machine()}.json",
                    drivers / "lvp_icd.json"] if candidate.is_file()), None)
        assert icd, "Mesa lavapipe is required"
        (directory / "options.json").write_text(json.dumps({"binary": str(args.binary),
            "shell": args.shell, "video": str(args.video_fixture) if args.video_fixture else None,
            "icd": str(icd), "gst_plugins": str(args.gst_plugin_dir) if args.gst_plugin_dir else None}))
        image = Image.new("RGB", (640, 360))
        draw = ImageDraw.Draw(image)
        for rect, color in zip([(0, 0, 319, 179), (320, 0, 639, 179),
                                (0, 180, 319, 359), (320, 180, 639, 359)], COLORS):
            draw.rectangle(rect, fill=color)
        image.save(directory / "quadrants.png")
        env = os.environ.copy()
        for name in ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "DBUS_SESSION_BUS_ADDRESS",
                     "DBUS_STARTER_ADDRESS", "SESSION_MANAGER", "SWAYSOCK", "HYPRLAND_INSTANCE_SIGNATURE"]:
            env.pop(name, None)
        env.update(XDG_RUNTIME_DIR=str(directory / "runtime"), XDG_CONFIG_HOME=str(directory / "config"),
                   XDG_DATA_HOME=str(directory / "data"), XDG_CACHE_HOME=str(directory / "cache"),
                   XDG_STATE_HOME=str(directory / "state"), XDG_CURRENT_DESKTOP="GNOME",
                   XDG_SESSION_TYPE="wayland", GSETTINGS_BACKEND="memory",
                   GSETTINGS_SCHEMA_DIR=str(directory / "schemas"), LIBGL_ALWAYS_SOFTWARE="1")
        mesa = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
        if mesa.is_file():
            env["__EGL_VENDOR_LIBRARY_FILENAMES"] = str(mesa)
        log = (directory / "session.log").open("w+")
        child = subprocess.Popen(["dbus-run-session", f"--config-file={directory / 'dbus.conf'}", "--",
                                  sys.executable, str(Path(__file__).resolve()), "--private-root", str(directory)],
                                 env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            assert child.wait(timeout=100) == 0, "private GNOME test failed"
            print((directory / "session.log").read_text())
        finally:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
            if child.returncode:
                for logfile in directory.glob("*.log"):
                    print(logfile.name, logfile.read_text(errors="replace")[-12000:], file=sys.stderr)
            if args.artifacts:
                args.artifacts.mkdir(parents=True, exist_ok=True)
                for artifact in directory.iterdir():
                    if artifact.suffix in [".log", ".png"]:
                        shutil.copyfile(artifact, args.artifacts / artifact.name)
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", default=shutil.which("gnome-shell"))
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall")
    parser.add_argument("--schema-dir", type=Path,
                        default=Path(os.environ["GSETTINGS_SCHEMA_DIR"]) if os.environ.get("GSETTINGS_SCHEMA_DIR") else None)
    parser.add_argument("--gst-plugin-dir", type=Path)
    parser.add_argument("--video-fixture", type=Path)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--private-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.private_root:
        private_test(args.private_root)
        return
    for executable in [args.shell, "bwrap", "dbus-run-session", "glib-compile-schemas"]:
        assert executable and shutil.which(executable), f"Missing dependency: {executable}"
    args.shell = str(Path(shutil.which(args.shell)).resolve())
    args.binary = args.binary.resolve(strict=True)
    if args.video_fixture:
        args.video_fixture = args.video_fixture.resolve(strict=True)
    run(args)


if __name__ == "__main__":
    main()
