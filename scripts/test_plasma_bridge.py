#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Test the external-process Plasma presentation bridge in private desktop sessions.

Default: the real bridge in a Qt window, nested in headless Sway (suitable for CI).
--plasma: also start private KWin/Plasma 6, with isolated D-Bus and XDG directories.
Requires Pillow, Sway, grim, Qt's qml executable, Wayland QML modules and Mesa.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from xml.sax.saxutils import escape

from PIL import Image, ImageDraw

from test_wayland_headless import COLORS, ROOT, Session, wait_for

PLUGIN = ROOT / "integrations/plasma/org.openwall.wallpaper"
QML = ROOT / "scripts/qml"
SOCKET_PREFIX = "openwall-plasma-"


def qt_tool(name):
    for candidate in [f"{name}6", f"{name}-qt6", name,
                      f"/usr/lib64/qt6/bin/{name}", f"/usr/lib/qt6/bin/{name}"]:
        if executable := shutil.which(candidate):
            return executable
    return None


def sockets(directory):
    return sorted(entry for entry in (directory / "runtime").glob(f"{SOCKET_PREFIX}*")
                  if entry.is_socket())


def spawn(session, command, env, name, new_session=False):
    log = (session.directory / f"{name}.log").open("w+")
    session.logs.append(log)
    child = subprocess.Popen(command, env=env, stdout=log, stderr=log,
                             start_new_session=new_session)
    session.workers.append(child)
    return child, log


def graphics_env(session):
    env = dict(session.env, QT_QPA_PLATFORM="wayland", QSG_RHI_BACKEND="opengl",
               LIBGL_ALWAYS_SOFTWARE="1", QT_FORCE_STDERR_LOGGING="1",
               QT_LOGGING_TO_CONSOLE="1", KDE_DEBUG="1")
    env.pop("QT_QUICK_BACKEND", None)
    # NVIDIA GLVND may otherwise be selected even with a software Vulkan ICD.
    mesa = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    if mesa.exists():
        env["__EGL_VENDOR_LIBRARY_FILENAMES"] = str(mesa)
    return env


def fixture(directory):
    image = Image.new("RGB", (640, 360))
    draw = ImageDraw.Draw(image)
    for rect, color in zip([(0, 0, 319, 179), (320, 0, 639, 179),
                            (0, 180, 319, 359), (320, 180, 639, 359)], COLORS):
        draw.rectangle(rect, fill=color)
    path = directory / "quadrants.png"
    image.save(path)
    return path


def assert_alive(session, *children):
    assert all(child.poll() is None for child in children), session.diagnostics()


def exercise(session, args, env, host, desktop_check=None):
    directory = session.directory
    wait_for(lambda: sockets(directory), "external presentation socket")
    assert len(sockets(directory)) == 1, sockets(directory)
    worker_env = dict(env, WAYLAND_DISPLAY=sockets(directory)[0].name)
    source = fixture(directory)

    def start(name):
        return spawn(session, [str(args.binary), "preview", str(source)], worker_env, name)

    def displayed(worker):
        assert_alive(session, host, worker)
        return session.frame("HEADLESS-1")

    def fallback():
        assert_alive(session, host)
        image = session.capture("HEADLESS-1")
        # A dark, blue fallback, rather than stale worker content or a black hole.
        return image is not None and all(
            5 < image.getpixel((int(image.width * x), int(image.height * y)))[2] < 100
            for x, y in [(.25, .25), (.75, .25), (.25, .75), (.75, .75)])

    worker, _ = start("scene")
    wait_for(lambda: displayed(worker), "Rust scene composited inside trusted host")
    if args.artifacts:
        session.capture("HEADLESS-1").save(args.artifacts / f"{'plasma' if args.plasma else 'qt'}-scene.png")
    if desktop_check:
        desktop_check()
        wait_for(lambda: displayed(worker), "wallpaper retained after widget removal")

    # An unexpected second toplevel must not take over the active wallpaper.
    duplicate, _ = start("duplicate")
    assert duplicate.wait(timeout=15) != 0, "duplicate client was not rejected"
    assert displayed(worker), "duplicate client displaced the original worker"

    # Logical rectangle updates must configure the existing worker, not require
    # a new socket or decoder. Exercise the host's actual fullscreen window.
    session.ipc("output HEADLESS-1 mode 800x450 scale 1.25")
    wait_for(lambda: displayed(worker), "presentation after fractional-scale update")
    session.ipc("output HEADLESS-1 scale 1")
    wait_for(lambda: displayed(worker), "presentation after logical resize")
    session.ipc("output HEADLESS-1 mode 640x360 scale 1")
    wait_for(lambda: displayed(worker), "presentation after restoring size")
    session.stop_worker(worker)
    wait_for(fallback, "fallback after graceful worker exit")

    worker, _ = start("reconnect")
    wait_for(lambda: displayed(worker), "worker reconnection")
    worker.kill()
    worker.wait(timeout=10)
    wait_for(fallback, "fallback after worker SIGKILL")
    worker, _ = start("after-crash")
    wait_for(lambda: displayed(worker), "reconnection after worker crash")
    session.stop_worker(worker)

    if args.video_fixture:
        video, log = spawn(session, [str(args.binary), "preview", str(args.video_fixture)],
                           worker_env, "video")

        def video_frame():
            assert_alive(session, host, video)
            image = session.capture("HEADLESS-1")
            return image is not None and any(
                max(image.getpixel((int(image.width * x), int(image.height * y)))) > 130
                for x, y in [(.25, .25), (.75, .25), (.25, .75), (.75, .75)])

        wait_for(video_frame, "native video pixels inside presentation host")
        session.expect_log(video, log, "First video buffer:")
        session.expect_log(video, log, "Native video window configured")
        session.expect_log(video, log, "Video loop restarted")
        if desktop_check:
            desktop_check()
        decoders = session.text(log).count("GStreamer decoder:")
        offset = len(session.text(log))
        session.ipc("output HEADLESS-1 mode 800x450 scale 1")
        session.expect_log(video, log, "logical 800×450", offset)
        wait_for(video_frame, "native video after host resize")
        offset = len(session.text(log))
        session.expect_log(video, log, "Video loop restarted", offset)
        assert session.text(log).count("GStreamer decoder:") == decoders, "host resize replaced decoder"
        assert "OpenWall GPU:" not in session.text(log), session.diagnostics()
        session.stop_worker(video)
        session.ipc("output HEADLESS-1 mode 640x360 scale 1")
        wait_for(fallback, "fallback after video exit")
        video, _ = spawn(session, [str(args.binary), "preview", str(args.video_fixture)],
                         worker_env, "video-crash")
        wait_for(video_frame, "video before SIGKILL")
        video.kill()
        video.wait(timeout=10)
        wait_for(fallback, "fallback after video SIGKILL")

    print(json.dumps({"bridge": "Plasma desktop" if args.plasma else "Qt host",
                      "checks": ["scene pixels", "duplicate client rejection", "resize", "graceful exit",
                                 "worker crash", "reconnect", "fallback"],
                      "native_video": bool(args.video_fixture)}), flush=True)


def evaluate(qdbus, env, code):
    return subprocess.run([qdbus, "org.kde.plasmashell", "/PlasmaShell",
                           "org.kde.PlasmaShell.evaluateScript", code], env=env,
                          capture_output=True, text=True, timeout=5)


def private_plasma(directory, qdbus):
    """Executed only inside our isolated dbus-run-session, never the user's bus."""
    assert os.environ["XDG_RUNTIME_DIR"] == str(directory / "runtime")
    assert str(directory / "runtime") in os.environ["DBUS_SESSION_BUS_ADDRESS"]
    children = []

    def stop(*_):
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)

    def launch(command, env, name):
        log = (directory / f"{name}.log").open("w+")
        child = subprocess.Popen(command, env=env, stdout=log, stderr=log)
        children.append((child, log))
        return child

    try:
        env = os.environ.copy()
        kwin = launch(["kwin_wayland", "--wayland-display", env["WAYLAND_DISPLAY"],
                       "--socket", "openwall-kwin-test", "--width", "640", "--height", "360",
                       "--output-count", "1", "--no-lockscreen", "--no-global-shortcuts",
                       "--no-kactivities"], env, "kwin")

        def kwin_ready():
            assert kwin.poll() is None, "private KWin failed"
            return (directory / "runtime/openwall-kwin-test").is_socket()

        wait_for(kwin_ready, "private KWin socket", timeout=25)
        env["WAYLAND_DISPLAY"] = "openwall-kwin-test"
        version = subprocess.check_output(["plasmashell", "--version"], env=env, text=True, timeout=10)
        assert "plasmashell 6." in version, f"Plasma 6 required: {version}"
        shell = launch(["plasmashell", "--no-respawn"], env, "plasmashell")

        def configured():
            assert shell.poll() is None, "private Plasma failed"
            result = evaluate(qdbus, env, 'var ds = desktops(); for (var i = 0; i < ds.length; i++) '
                              '{ ds[i].wallpaperPlugin = "org.openwall.wallpaper"; }')
            return result.returncode == 0 and sockets(directory)

        wait_for(configured, "Plasma wallpaper socket", timeout=40)
        (directory / "ready.json").write_text(json.dumps({
            "bus": env["DBUS_SESSION_BUS_ADDRESS"], "kwin_pid": kwin.pid,
            "plasma_pid": shell.pid}))
        while not (directory / "stop").exists():
            assert shell.poll() is None and kwin.poll() is None, "private desktop exited"
            time.sleep(.1)
    finally:
        for child, log in reversed(children):
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            log.close()


def plasma_environment(session):
    directory = session.directory
    for name in ["data", "config", "cache", "state", "desktop", "services"]:
        (directory / name).mkdir(exist_ok=True)
    # No KDE service may resolve a user-directory default back into the real home.
    (directory / "config/user-dirs.dirs").write_text("\n".join(
        f'XDG_{name}_DIR="{directory}/desktop"' for name in ["DESKTOP", "DOWNLOAD", "DOCUMENTS",
            "MUSIC", "PICTURES", "PUBLICSHARE", "TEMPLATES", "VIDEOS"]))
    shutil.copytree(PLUGIN, directory / "data/plasma/wallpapers/org.openwall.wallpaper")
    shutil.copytree(ROOT / "scripts/plasma-widget", directory / "data/plasma/plasmoids/org.openwall.test-widget")
    # Permit only the required ActivityManager activation, not portals/kded/agents.
    service = Path("/usr/share/dbus-1/services/org.kde.ActivityManager.service")
    assert service.is_file(), "Plasma ActivityManager service is missing"
    shutil.copy(service, directory / "services")
    runtime = escape(str(directory / "runtime"))
    services = escape(str(directory / "services"))
    (directory / "dbus.conf").write_text(f'''<busconfig><type>session</type>
<listen>unix:tmpdir={runtime}</listen><auth>EXTERNAL</auth><servicedir>{services}</servicedir>
<policy context="default"><allow own="*"/><allow send_destination="*"/>
<allow receive_sender="*"/></policy></busconfig>''')
    return dict(graphics_env(session), XDG_DATA_HOME=str(directory / "data"),
                XDG_STATE_HOME=str(directory / "state"), XDG_CURRENT_DESKTOP="KDE")


def desktop_widget_check(session, args, env):
    result = evaluate(args.qdbus, env, 'var d = desktops()[0]; '
                      'var w = d.addWidget("org.openwall.test-widget"); '
                      'w.geometry = new QRectF(250, 90, 96, 96); print(w.id);')
    assert result.returncode == 0, result.stderr
    widget_id = int(result.stdout.strip())

    def colored_region(color):
        image = session.capture("HEADLESS-1")
        if image is None:
            return None
        mask = Image.new("L", image.size)
        mask.putdata([255 if max(abs(a - b) for a, b in zip(pixel, color)) < 5 else 0
                      for pixel in image.getdata()])
        bounds = mask.getbbox()
        return bounds if bounds and mask.histogram()[255] > 500 else None

    bounds = wait_for(lambda: colored_region((190, 41, 233)), "desktop widget above wallpaper")
    x, y = (bounds[0] + bounds[2]) // 2, (bounds[1] + bounds[3]) // 2
    # Keep a virtual pointer alive through capability negotiation in both nested shells.
    outputs = [output for output in session.outputs() if output["active"]]
    target = next(output["rect"] for output in outputs if output["name"] == "HEADLESS-1")
    x += target["x"]
    y += target["y"]
    width = max(output["rect"]["x"] + output["rect"]["width"] for output in outputs)
    height = max(output["rect"]["y"] + output["rect"]["height"] for output in outputs)
    subprocess.run([str(args.pointer_helper), str(x), str(y), str(width), str(height)],
                   env=dict(session.env, OPENWALL_TEST_POINTER_RUNTIME=session.env["XDG_RUNTIME_DIR"]),
                   timeout=10, check=True)
    wait_for(lambda: colored_region((37, 214, 184)), "desktop widget receives pointer input")
    if args.artifacts:
        session.capture("HEADLESS-1").save(args.artifacts / "plasma-widget-input.png")
    result = evaluate(args.qdbus, env, f"desktops()[0].widgetById({widget_id}).remove();")
    assert result.returncode == 0, result.stderr
    print("Plasma widget ordering and pointer input: passed", flush=True)


def stop_group(child):
    # dbus activation can leave descendants after dbus-run-session exits.
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=5)


def clean_private_helpers(directory):
    """KIO workers may detach; identify only our exact private runtime directory."""
    marker = f"XDG_RUNTIME_DIR={directory / 'runtime'}".encode()
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            if entry.stat().st_uid == os.getuid() and marker in (entry / "environ").read_bytes().split(b"\0"):
                os.kill(int(entry.name), signal.SIGTERM)
        except (OSError, ProcessLookupError):
            pass


def run(args):
    # Short prefix keeps the nested D-Bus/KIO Unix socket paths below sun_path.
    with tempfile.TemporaryDirectory(prefix="owp-") as temporary:
        directory = Path(temporary)
        session = Session(directory, args.sway, True)
        group = None
        try:
            session.connect()
            if args.plasma:
                env = plasma_environment(session)
                group, _ = spawn(session, ["dbus-run-session", f"--config-file={directory / 'dbus.conf'}",
                                 "--", sys.executable, str(Path(__file__).resolve()),
                                 "--private-plasma-root", str(directory), "--qdbus", args.qdbus],
                                 env, "session", new_session=True)

                def ready():
                    assert_alive(session, group)
                    return (directory / "ready.json").exists()

                wait_for(ready, "private Plasma session", timeout=65)
                info = json.loads((directory / "ready.json").read_text())
                host = group
                host_pid = info["kwin_pid"]
            else:
                env = graphics_env(session)
                host, _ = spawn(session, [args.qml, str(QML / "PresentationHost.qml")], env, "qml")
                host_pid = host.pid

            def window():
                assert_alive(session, host)
                return next((item for item in session.windows() if item["pid"] == host_pid), None)

            mapped = wait_for(window, "private presentation host window")
            session.ipc(f'[con_id={mapped["id"]}] move container to output HEADLESS-1')
            session.ipc(f'[con_id={mapped["id"]}] fullscreen enable')
            desktop_env = dict(env, DBUS_SESSION_BUS_ADDRESS=info["bus"],
                               WAYLAND_DISPLAY="openwall-kwin-test") if args.plasma else None
            check = (lambda: desktop_widget_check(session, args, desktop_env)) if args.plasma else None
            exercise(session, args, env, host, check)

            if args.plasma:
                preview_env = dict(env, WAYLAND_DISPLAY="openwall-kwin-test",
                                   DBUS_SESSION_BUS_ADDRESS=info["bus"])
                before = sockets(directory)
                preview, log = spawn(session, [args.qml, str(QML / "WallpaperPreview.qml")],
                                     preview_env, "preview")
                session.expect_log(preview, log, "OpenWall preview guard passed")
                time.sleep(.5)
                assert sockets(directory) == before, "preview created a desktop compositor"
                preview.terminate()
                preview.wait(timeout=10)
                print("Plasma plain-window preview guard: passed", flush=True)

            # Losing the trusted host must terminate the worker rather than leave
            # a background process polling or claiming successful presentation.
            worker_env = dict(env, WAYLAND_DISPLAY=sockets(directory)[0].name)
            worker, _ = spawn(session, [str(args.binary), "preview", str(directory / "quadrants.png")],
                              worker_env, "host-loss")
            session.expect_frame(worker, "HEADLESS-1")
            if args.plasma:
                os.kill(info["plasma_pid"], signal.SIGTERM)
            else:
                host.terminate()
            assert worker.wait(timeout=10) != 0, "host loss reported success"
            print("Presentation host loss terminates worker: passed", flush=True)
        except BaseException:
            print(session.diagnostics()[-24000:], file=sys.stderr)
            raise
        finally:
            (directory / "stop").touch()
            if group:
                try:
                    group.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    pass
                stop_group(group)
            if args.artifacts:
                for logfile in directory.glob("*.log"):
                    shutil.copyfile(logfile, args.artifacts / logfile.name)
            session.close()
            clean_private_helpers(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/openwall")
    parser.add_argument("--sway", default="sway")
    parser.add_argument("--qml", default=qt_tool("qml"))
    parser.add_argument("--qdbus", default=qt_tool("qdbus"))
    parser.add_argument("--pointer-helper", type=Path, default=ROOT / "target/debug/examples/headless_pointer")
    parser.add_argument("--plasma", action="store_true", help="opt in to nested Plasma 6 test")
    parser.add_argument("--video-fixture", type=Path)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--private-plasma-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.private_plasma_root:
        private_plasma(args.private_plasma_root, args.qdbus)
        return
    assert args.binary.is_file(), "Build the OpenWall worker first"
    assert args.qml, "Qt 6 qml executable is missing; pass --qml /path/to/qt6/bin/qml"
    for executable in [args.sway, "swaymsg", "grim", args.qml]:
        assert shutil.which(executable), f"Missing test dependency: {executable}"
    if args.plasma:
        assert args.pointer_helper.is_file(), "Build cargo build -p openwall --example headless_pointer first"
        args.pointer_helper = args.pointer_helper.resolve()
        for executable in ["kwin_wayland", "plasmashell", "dbus-run-session", args.qdbus]:
            assert executable and shutil.which(executable), f"Missing Plasma test dependency: {executable}"
    if args.video_fixture:
        args.video_fixture = args.video_fixture.resolve(strict=True)
    args.binary = args.binary.resolve()
    if args.artifacts:
        args.artifacts.mkdir(parents=True, exist_ok=True)
    run(args)


if __name__ == "__main__":
    main()
