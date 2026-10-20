#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Measure the shared-GL video process inside a private headless compositor."""
import argparse
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import statistics
import subprocess
import tempfile

from benchmark_video import fingerprint, LOG_LIMIT
from private_gpu import nvidia_renderer
from test_wayland_headless import Session, wait_for

ROOT = Path(__file__).resolve().parents[1]


def bounded_int(low, high):
    def parse(text):
        value = int(text)
        if not low <= value <= high:
            raise argparse.ArgumentTypeError(f"expected {low}..{high}")
        return value
    return parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("video", type=Path, help="original VP9/WebM fixture; hash included in report")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/examples/native_gl_video_probe")
    parser.add_argument("--gl-api", choices=["desktop", "gles"], default="desktop")
    parser.add_argument("--seconds", type=bounded_int(1, 120), default=30)
    parser.add_argument("--warmup", type=bounded_int(0, 30), default=5)
    parser.add_argument("--repeat", type=bounded_int(1, 5), default=3)
    parser.add_argument("--width", type=bounded_int(1, 8192), default=1920)
    parser.add_argument("--height", type=bounded_int(1, 8192), default=1080)
    parser.add_argument("--nvidia", action="store_true")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--check-interruption", action="store_true", help="also reject an intentionally incomplete trial (CI contract check)")
    args = parser.parse_args()
    if args.nvidia and args.gl_api != "desktop":
        parser.error("NVIDIA video transfer qualification requires desktop GL")
    if args.render_node and not args.nvidia:
        parser.error("--render-node requires --nvidia")
    if args.width * args.height * 4 > 256 * 1024 * 1024:
        parser.error("private output exceeds the buffer limit")
    binary, video = args.binary.resolve(strict=True), args.video.resolve(strict=True)
    assert platform.system() == "Linux" and all(shutil.which(tool) for tool in ["sway", "swaymsg", "grim"])
    vendor, node, renderer = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json"), None, None
    if args.nvidia:
        vendor, node, renderer = nvidia_renderer(args.render_node)
    assert vendor.is_file()
    if args.check_interruption:
        guard_env = os.environ.copy()
        guard_env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
        base = [str(binary), str(video), "--output", "never-connect"]
        for invalid in [["--measure-seconds", "0"], ["--measure-seconds", "121"],
                        ["--measure-seconds", "1", "--loops", "1"],
                        ["--measure-seconds", "1", "--staged-control", "/never-created.sock"],
                        ["--warmup-seconds", "0"]]:
            rejected = subprocess.run([*base, *invalid], env=guard_env, capture_output=True, text=True, timeout=5)
            assert rejected.returncode != 0 and "error:" in rejected.stderr and not rejected.stdout.strip(), rejected
        guarded = subprocess.run([*base, "--measure-seconds", "1"], env=guard_env, capture_output=True, text=True, timeout=5)
        assert guarded.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guarded.stderr and not guarded.stdout.strip(), guarded
    reports = []
    with tempfile.TemporaryDirectory(prefix="openwall-native-measure-") as temporary:
        directory = Path(temporary)
        session = Session(directory, shutil.which("sway"), software_vulkan=False, renderer_env=renderer)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor),
                           GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl", GST_GL_API="opengl3" if args.gl_api == "desktop" else "gles2",
                           GST_PLUGIN_FEATURE_RANK="nvvp9dec:MAX" if args.nvidia else "vp9dec:MAX", GST_REGISTRY_1_0=str(directory / "registry.bin"))
        if args.nvidia:
            session.env.update(GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        try:
            session.connect()
            output, other = sorted(item["name"] for item in session.outputs() if item["active"])
            session.ipc(f"output {output} mode {args.width}x{args.height}")
            wait_for(lambda: any(item["name"] == output and item["current_mode"]["width"] == args.width
                                and item["current_mode"]["height"] == args.height for item in session.outputs()), "private output dimensions")
            baseline = wait_for(lambda: session.capture(output), "measurement baseline").getpixel((10, 10))
            other_baseline = session.capture(other).tobytes()
            for index in range(args.repeat):
                log = (directory / f"measure-{index}.log").open("w+")
                result = (directory / f"measure-{index}.json").open("w+")
                session.logs.extend([log, result])
                child = subprocess.Popen([str(binary), str(video), "--output", output, "--gl-api", args.gl_api,
                                          "--measure-seconds", str(args.seconds), "--warmup-seconds", str(args.warmup)],
                                         env=session.env, stdout=result, stderr=log, text=True, start_new_session=True)
                session.workers.append(child)
                try:
                    child.communicate(timeout=args.seconds + args.warmup + 25)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.communicate(timeout=5)
                    raise AssertionError("native measurement exceeded its process deadline")
                assert child.returncode == 0, session.text(log)[-LOG_LIMIT:]
                report = json.loads(session.text(result))
                metric = report["measurement"]
                assert report["status"] == "passed" and metric["scope"] == "video_process_only", report
                assert metric["sample_seconds"] >= args.seconds and metric["rss_samples"] >= 2, metric
                assert math.isfinite(metric["cpu_percent_one_core"]) and metric["cpu_percent_one_core"] >= 0, metric
                assert 0 < metric["mean_sampled_rss_mib"] <= metric["peak_sampled_rss_mib"], metric
                assert 0 < metric["observed"]["submissions"] <= report["submitted_frames"], report
                assert 0 < metric["observed"]["presentation_feedbacks"] <= report["presented"], report
                assert report["submitted_frames"] >= report["retained_frame_records"] == len(report["frames"]), report
                assert len(report["frames"]) <= 32 and len(report["presented_frame_ids"]) <= 256, report
                fences = report["consumer_fences"]
                assert fences["submitted"] == fences["retired"] == report["submitted_frames"], report
                assert 0 < fences["peak_in_flight"] <= 3 and fences["cleanup_finish_calls"] == 0, report
                assert all(report[key] == 0 for key in ["consumer_cpu_buffer_maps", "consumer_readbacks", "consumer_offscreen_targets"]), report
                assert report["egl_lifecycle"] == {"context_creations":1,"context_destructions":1,"display_initializations":1,
                                                   "display_terminations":1,"surface_creations":1,"surface_destructions":1}, report
                assert report["witness_button_events"] >= 2 and len(report["decoders"]) == 1, report
                assert "memory:GLMemory" in report["input_memory"]["caps"], report
                memory = report["input_memory"]["memory"]
                assert memory["shared_context"] and memory["producer_sync_meta"], report
                if args.nvidia:
                    assert "NVIDIA" in report["gl"]["vendor"] and report["decoders"] == ["nvvp9dec"], report
                    decoder = next(iter(report["decoder_buffers"].values()))
                    assert decoder["all_memory_is_gl"] and "memory:GLMemory" in decoder["caps"], report
                    text = session.text(log)
                    assert "using GL memory" in text and "fallback to system memory" not in text, text[-LOG_LIMIT:]
                else:
                    assert "llvmpipe" in report["gl"]["renderer"].lower() and report["decoders"] == ["vp9dec"], report
                report["diagnostics_tail"] = session.text(log)[-LOG_LIMIT:]
                reports.append(report)
                def removed():
                    image = session.capture(output)
                    return image is not None and all(max(abs(a-b) for a,b in zip(image.getpixel((int(image.width*x), int(image.height*y))), baseline)) <= 2
                                                    for x,y in [(.1,.25),(.7,.25),(.3,.75),(.9,.75)])
                wait_for(removed, "measurement surface cleanup")
                assert session.capture(other).tobytes() == other_baseline, "measurement changed other output"
            if args.nvidia:
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-LOG_LIMIT:]
            if args.check_interruption:
                log = (directory / "interrupted.log").open("w+")
                result = (directory / "interrupted.json").open("w+")
                session.logs.extend([log, result])
                child = subprocess.Popen([str(binary), str(video), "--output", output, "--gl-api", args.gl_api,
                                          "--measure-seconds", "10", "--warmup-seconds", "0"],
                                         env=session.env, stdout=result, stderr=log, text=True)
                session.workers.append(child)
                session.expect_log(child, log, "Native GL measurement started after actual presentation and warmup")
                child.send_signal(signal.SIGINT)
                child.communicate(timeout=10)
                assert child.returncode != 0 and not session.text(result).strip(), session.text(log)[-LOG_LIMIT:]
                assert "native measurement interrupted before completion" in session.text(log), session.text(log)[-LOG_LIMIT:]
                wait_for(removed, "incomplete measurement surface cleanup")
            compositor_version = subprocess.run([shutil.which("sway"), "--version"], capture_output=True, text=True, check=True).stdout.strip()
        finally:
            session.close()
    metrics = [report["measurement"] for report in reports]
    print(json.dumps({"schema_version":1,"status":"passed","scope":"private_headless_video_process_only",
                      "binary":fingerprint(binary),"video":fingerprint(video),"private_output_size":[args.width,args.height],
                      "system":{"os":platform.system(),"release":platform.release(),"architecture":platform.machine(),"compositor":compositor_version},
                      "nvidia":args.nvidia,"render_node":str(node) if node else None,"runs":reports,
                      "interruption_contract_checked":args.check_interruption,
                      "median_cpu_percent_one_core":statistics.median(metric["cpu_percent_one_core"] for metric in metrics),
                      "median_presentation_feedback_hz":statistics.median(metric["presentation_feedback_hz"] for metric in metrics),
                      "max_sampled_rss_mib":max(metric["peak_sampled_rss_mib"] for metric in metrics)}))


if __name__ == "__main__":
    main()
