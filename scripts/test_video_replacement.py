#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Replace and restore distinct video sources in a private headless compositor."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time

from private_gpu import nvidia_renderer
from test_native_gl_video import fixture, identify_pixels
from test_wayland_headless import Session, wait_for
from test_owned_egl_wayland import cpu_seconds

ROOT = Path(__file__).resolve().parents[1]
LIFECYCLE = {"context_creations": 1, "context_destructions": 1, "display_initializations": 1,
             "display_terminations": 1, "surface_creations": 2, "surface_destructions": 2}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/examples/video_replacement_probe")
    parser.add_argument("--gl-api", choices=["desktop", "gles"], default="desktop")
    parser.add_argument("--nvidia", action="store_true")
    parser.add_argument("--render-node", type=Path)
    parser.add_argument("--artifact-directory", type=Path)
    args = parser.parse_args()
    if args.nvidia and args.gl_api != "desktop":
        parser.error("NVIDIA video transfer qualification requires desktop GL")
    if args.render_node and not args.nvidia:
        parser.error("--render-node requires --nvidia")
    binary = args.binary.resolve(strict=True)
    assert all(shutil.which(tool) for tool in ["sway", "swaymsg", "grim", "ffmpeg"])
    vendor = Path("/usr/share/glvnd/egl_vendor.d/50_mesa.json")
    node, render_env = None, None
    if args.nvidia:
        vendor, node, render_env = nvidia_renderer(args.render_node)
    assert vendor.is_file()
    env = os.environ.copy()
    env.pop("OPENWALL_PRIVATE_COMPOSITOR", None)
    guard = subprocess.run([str(binary), "missing.webm", "candidate.webm", "--output", "never-connect",
                            "--control-socket", "/never-created.sock"], env=env, capture_output=True, text=True, timeout=5)
    assert guard.returncode != 0 and "OPENWALL_PRIVATE_COMPOSITOR=1" in guard.stderr and not guard.stdout.strip(), guard
    with tempfile.TemporaryDirectory(prefix="openwall-video-replacement-") as temporary:
        directory = Path(temporary)
        # Reject inherited descriptors before attempting any display connection.
        private_env = {key: value for key, value in env.items()
                       if key not in {"DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "SWAYSOCK", "DBUS_SESSION_BUS_ADDRESS"}}
        private_env.update(OPENWALL_PRIVATE_COMPOSITOR="1", XDG_RUNTIME_DIR=str(directory))
        bad_owner, bad_endpoint = socket.socketpair(type=socket.SOCK_DGRAM)
        try:
            for descriptor in [subprocess.DEVNULL, bad_endpoint]:
                invalid = subprocess.run([str(binary), "missing.webm", "candidate.webm", "--output", "never-connect",
                                          "--control-stdin"], env=private_env, stdin=descriptor,
                                         capture_output=True, text=True, timeout=5)
                assert invalid.returncode != 0 and "control stdin" in invalid.stderr and not invalid.stdout.strip(), invalid
        finally:
            bad_owner.close()
            bad_endpoint.close()
        videos, colors = [], []
        for index in range(2):
            source = directory / f"source-{index}"
            source.mkdir()
            video, palette, _ = fixture(source, args.nvidia, variant=index)
            videos.append(video)
            colors.append(palette)
        runtime = directory / "session"
        runtime.mkdir()
        session = Session(runtime, shutil.which("sway"), software_vulkan=False, renderer_env=render_env)
        session.env.update(OPENWALL_PRIVATE_COMPOSITOR="1", EGL_PLATFORM="wayland",
                           LIBGL_ALWAYS_SOFTWARE="0" if args.nvidia else "1", __EGL_VENDOR_LIBRARY_FILENAMES=str(vendor),
                           GST_GL_WINDOW="surfaceless", GST_GL_PLATFORM="egl", GST_GL_API="opengl3" if args.gl_api == "desktop" else "gles2",
                           GST_PLUGIN_FEATURE_RANK="nvvp9dec:MAX" if args.nvidia else "vp9dec:MAX", GST_REGISTRY_1_0=str(directory / "registry.bin"))
        if args.nvidia:
            session.env.update(GST_DEBUG="nvdecoder:5", GST_DEBUG_NO_COLOR="1")
        maximum_error = 0
        captures = 0
        results, controls = {}, {}
        try:
            session.connect()
            output, other = sorted(item["name"] for item in session.outputs() if item["active"])
            baseline = wait_for(lambda: session.capture(output), "baseline").getpixel((10, 10))
            other_baseline = session.capture(other).tobytes()

            def spawn(candidate=None, extra=(), stream=False):
                index = len(session.logs)
                log = (runtime / f"replace-{index}.log").open("w+")
                result = (runtime / f"replace-{index}.json").open("w+")
                session.logs.extend([log, result])
                path = runtime / f"control-{index}.sock"
                owner, endpoint = socket.socketpair() if stream else (None, None)
                control_args = ["--control-stdin"] if stream else ["--control-socket", str(path)]
                try:
                    child = subprocess.Popen([str(binary), str(videos[0]), str(candidate or videos[1]), "--output", output,
                                              *control_args, "--gl-api", args.gl_api, *extra],
                                             env=session.env, stdin=endpoint, stdout=result, stderr=log, text=True)
                except Exception:
                    if owner: owner.close()
                    raise
                finally:
                    if endpoint: endpoint.close()
                session.workers.append(child)
                results[child.pid], controls[child.pid] = result, owner if stream else path
                return child, log

            def finish(child):
                child.communicate(timeout=10)
                return session.text(results[child.pid])

            def command(child, message):
                if isinstance(controls[child.pid], socket.socket):
                    controls[child.pid].sendall(message)
                    return
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                    control.sendto(message, str(controls[child.pid]))

            def pixels(child, allowed):
                nonlocal maximum_error, captures
                assert child.poll() is None, session.diagnostics()
                image = session.capture(output)
                assert image is not None, session.diagnostics()
                captures += 1
                matches = [(identify_pixels(image, colors[index]), index) for index in allowed]
                # Sort by channel error before marker/source identity.
                (identity, error), source = min(matches, key=lambda item: item[0][1])
                maximum_error = max(maximum_error, error)
                assert error <= 3, ("pixels outside permitted source frames", error, allowed, session.diagnostics())
                return source, identity

            def moving(child, index, seconds=1.2):
                seen = set()
                until = time.monotonic() + seconds
                while time.monotonic() < until:
                    source, identity = pixels(child, [index])
                    assert source == index
                    seen.add(identity)
                    time.sleep(.025)
                assert len(seen) >= 2, ("visible video stopped", index, seen, session.diagnostics())
                return seen

            def empty():
                image = session.capture(output)
                return image is not None and all(max(abs(a-b) for a,b in zip(image.getpixel((int(image.width*x), int(image.height*y))), baseline)) <= 2
                                                for x,y in [(.1,.25),(.7,.25),(.3,.75),(.9,.75)])

            def validate(report, surfaces=2):
                assert report["egl_lifecycle"] == dict(LIFECYCLE, surface_creations=surfaces, surface_destructions=surfaces), report
                fences = report["consumer_fences"]
                assert fences["submitted"] == fences["retired"] == len(report["frames"]), report
                assert 0 < fences["peak_in_flight"] <= fences["limit"] == 3 and fences["cleanup_finish_calls"] == 0, report
                assert all(report[key] == 0 for key in ["consumer_cpu_buffer_maps", "consumer_readbacks", "consumer_offscreen_targets"]), report
                sources = [report[label] for label in ["previous", "candidate"]]
                sources.append(report.get("reprepared", {}).get("candidate"))
                for source in sources:
                    if not source or not source["prepared"]:
                        continue
                    assert len(source["decoders"]) == 1 and "memory:GLMemory" in source["input_memory"]["caps"], report
                    assert source["input_memory"]["memory"]["shared_context"] and source["input_memory"]["memory"]["producer_sync_meta"], report
                    if args.nvidia:
                        assert source["decoders"] == ["nvvp9dec"], report
                        trace = next(iter(source["decoder_buffers"].values()))
                        assert trace["factory"] == "nvvp9dec" and trace["all_memory_is_gl"] and "memory:GLMemory" in trace["caps"], report
                presented = set(report["presented_frame_ids"])
                assert all(frame["id"] not in presented for frame in report["frames"] if not frame["visible"]), report
                if report["feedback_history_saturated"]:
                    assert len(presented) == 256 and all(start["presented_frame_id"] not in presented for start in report["clock_starts"]), report
                else:
                    assert all(start["presented_frame_id"] in presented for start in report["clock_starts"]), report
                assert report["previous"]["playback_started"], report

            child, log = spawn()
            session.expect_log(child, log, "Replacement candidate cached; previous video continues")
            old_seen = moving(child, 0, 7)
            command(child, b"publish\n")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                source, _ = pixels(child, [0, 1])
                if source == 1 and "Replacement candidate video presented; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("candidate publication did not complete")
            candidate_seen = moving(child, 1)
            command(child, b"rollback\n")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                source, _ = pixels(child, [0, 1])
                if source == 0 and "Replacement previous video restored; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("previous video restoration did not complete")
            restored_seen = moving(child, 0)
            child.send_signal(signal.SIGINT)
            report = json.loads(finish(child))
            assert child.returncode == 0 and report["status"] == "passed", session.diagnostics()
            validate(report)
            assert report["previous"]["completed_loops"] >= 1, report
            assert report["candidate_presented"] and report["previous_restored"] and report["witness_button_events"] == 6, report
            assert report["candidate"]["playback_started"] and len(report["candidate"]["decoders"]) == 1, report
            assert [start["source"] for start in report["clock_starts"]] == [0, 1, 0], report
            assert next(frame for frame in report["frames"] if frame["kind"] == "publish")["pts_ns"] == 0, report
            assert next(frame for frame in report["frames"] if frame["kind"] == "rollback")["pts_ns"] == report["rollback_pts_ns"], report
            assert all(frame["pts_ns"] == 0 for frame in report["frames"] if frame["source"] == 1 and not frame["visible"]), report
            assert report["candidate_first_frame_confirmed"] and not report["finalized"] and not report["automatic_rollback"], report
            assert report["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), report
            assert session.capture(other).tobytes() == other_baseline, "other output changed"
            wait_for(empty, "rollback cleanup")

            saturated, log = spawn(extra=["--saturate-feedback-history"])
            session.expect_log(saturated, log, "Replacement candidate cached; previous video continues")
            moving(saturated, 0)
            command(saturated, b"publish\n")
            session.expect_log(saturated, log, "Replacement candidate video presented; pointer reached witness")
            moving(saturated, 1)
            command(saturated, b"rollback\n")
            session.expect_log(saturated, log, "Replacement previous video restored; pointer reached witness")
            moving(saturated, 0)
            saturated.send_signal(signal.SIGINT)
            saturated_report = json.loads(finish(saturated))
            assert saturated.returncode == 0 and saturated_report["status"] == "passed", session.diagnostics()
            validate(saturated_report)
            assert saturated_report["previous_restored"] and saturated_report["witness_button_events"] == 6, saturated_report
            assert [start["source"] for start in saturated_report["clock_starts"]] == [0, 1, 0], saturated_report
            assert saturated_report["candidate_first_frame_confirmed"] and saturated_report["candidate_presentation_receipt"]["completion"] == "presented", saturated_report
            assert session.capture(other).tobytes() == other_baseline, "saturated history changed other output"
            wait_for(empty, "saturated-history cleanup")

            saturated_discard, log = spawn(extra=["--saturate-feedback-history", "--discard-candidate-submission"])
            session.expect_log(saturated_discard, log, "Replacement candidate cached; previous video continues")
            command(saturated_discard, b"publish\n")
            session.expect_log(saturated_discard, log, "Replacement previous video restored; pointer reached witness")
            moving(saturated_discard, 0)
            saturated_discard.send_signal(signal.SIGINT)
            saturated_discard_report = json.loads(finish(saturated_discard))
            assert saturated_discard.returncode == 0 and saturated_discard_report["status"] == "passed", session.diagnostics()
            validate(saturated_discard_report)
            assert saturated_discard_report["handled_candidate_discards"] == 1 and len(saturated_discard_report["discarded_frame_ids"]) == 256, saturated_discard_report
            assert saturated_discard_report["candidate_presentation_receipt"]["completion"] == "discarded" and not saturated_discard_report["candidate_first_frame_confirmed"], saturated_discard_report
            assert [start["source"] for start in saturated_discard_report["clock_starts"]] == [0, 0] and saturated_discard_report["witness_button_events"] == 4, saturated_discard_report
            assert session.capture(other).tobytes() == other_baseline, "saturated discard history changed other output"
            wait_for(empty, "saturated-discard cleanup")

            aborted, log = spawn()
            session.expect_log(aborted, log, "Replacement candidate cached; previous video continues")
            command(aborted, b"abort\n")
            session.expect_log(aborted, log, "Replacement candidate removed; previous video retained")
            moving(aborted, 0)
            aborted.send_signal(signal.SIGINT)
            cancellation = json.loads(finish(aborted))
            assert aborted.returncode == 0 and cancellation["candidate_aborted"], session.diagnostics()
            validate(cancellation)
            assert not cancellation["candidate"]["playback_started"] and not cancellation["candidate_presented"], cancellation
            assert cancellation["candidate"]["delivery"]["received"] == 1, cancellation
            assert cancellation["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), cancellation
            wait_for(empty, "aborted video cleanup")

            invalid_media = directory / "invalid.webm"
            invalid_media.write_bytes(b"not a media container\n")
            failed, log = spawn(invalid_media)
            session.expect_log(failed, log, "Replacement candidate removed; previous video retained")
            moving(failed, 0)
            failed.send_signal(signal.SIGINT)
            failure = json.loads(finish(failed))
            assert failed.returncode == 0 and failure["candidate_failure"] and not failure["candidate_presented"], session.diagnostics()
            validate(failure)
            assert failure["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), failure
            wait_for(empty, "failed candidate leaves previous alive until shutdown")

            faulty, log = spawn(extra=["--fail-candidate-after-ms", "1000"])
            session.expect_log(faulty, log, "Replacement candidate cached; previous video continues")
            command(faulty, b"publish\n")
            session.expect_log(faulty, log, "Replacement candidate video presented; pointer reached witness")
            fault_markers = set()
            until = time.monotonic() + 8
            while time.monotonic() < until:
                source, identity = pixels(faulty, [0, 1])
                if source == 1:
                    fault_markers.add(identity)
                if source == 0 and "Replacement previous video restored; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("injected candidate media error did not restore previous video")
            assert len(fault_markers) >= 2, ("candidate did not move before injected failure", fault_markers)
            moving(faulty, 0)
            faulty.send_signal(signal.SIGINT)
            recovery = json.loads(finish(faulty))
            assert faulty.returncode == 0 and recovery["status"] == "passed", session.diagnostics()
            validate(recovery)
            assert recovery["fault_injected"] and recovery["automatic_rollback"] and recovery["previous_restored"], recovery
            assert recovery["candidate_presented"] and recovery["candidate_first_frame_confirmed"] and not recovery["finalized"], recovery
            assert "injected private candidate stream error" in recovery["candidate_failure"] and recovery["candidate_report_error"] is None, recovery
            assert [start["source"] for start in recovery["clock_starts"]] == [0, 1, 0], recovery
            assert recovery["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), recovery
            assert next(frame for frame in recovery["frames"] if frame["kind"] == "rollback")["pts_ns"] == recovery["rollback_pts_ns"], recovery
            wait_for(empty, "automatic media-error rollback cleanup")

            early, log = spawn(extra=["--fail-before-candidate-confirmation"])
            session.expect_log(early, log, "Replacement candidate cached; previous video continues")
            command(early, b"publish\n")
            session.expect_log(early, log, "Replacement candidate readiness cancelled; restoring retained previous video")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                source, _ = pixels(early, [0, 1])
                if source == 0 and "Replacement previous video restored; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("candidate failure before readiness did not restore previous video")
            moving(early, 0)
            early.send_signal(signal.SIGINT)
            early_report = json.loads(finish(early))
            assert early.returncode == 0 and early_report["status"] == "passed", session.diagnostics()
            validate(early_report)
            assert early_report["early_cancel_requested"] and early_report["automatic_rollback"] and early_report["fault_injected"], early_report
            assert early_report["previous_restored"] and not early_report["candidate_readiness_accepted"] and not early_report["candidate_presented"], early_report
            assert not early_report["candidate"]["playback_started"] and early_report["candidate"]["delivery"]["received"] == 1, early_report
            assert [start["source"] for start in early_report["clock_starts"]] == [0, 0] and early_report["witness_button_events"] == 4, early_report
            feedback = early_report["presentation_feedback"]
            assert feedback["cancelled"] == int(early_report["early_cancel_applied"]), early_report
            assert feedback["cancelled_presented"] + feedback["cancelled_discarded"] == feedback["cancelled"], early_report
            assert feedback["pending"] == feedback["active"] <= feedback["limit"] == 16, early_report
            assert early_report["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), early_report
            assert next(frame for frame in early_report["frames"] if frame["kind"] == "rollback")["pts_ns"] == early_report["rollback_pts_ns"], early_report
            wait_for(empty, "cancelled publication feedback cleanup")

            discarded, log = spawn(extra=["--discard-candidate-submission"])
            session.expect_log(discarded, log, "Replacement candidate cached; previous video continues")
            command(discarded, b"publish\n")
            session.expect_log(discarded, log, "Replacement candidate readiness cancelled; restoring retained previous video")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                pixels(discarded, [0])
                if "Replacement previous video restored; pointer reached witness" in session.text(log):
                    break
            else:
                raise AssertionError("discarded candidate did not restore previous video")
            moving(discarded, 0)
            discarded.send_signal(signal.SIGINT)
            discarded_report = json.loads(finish(discarded))
            assert discarded.returncode == 0 and discarded_report["status"] == "passed", session.diagnostics()
            validate(discarded_report)
            assert discarded_report["discard_injected"] and discarded_report["automatic_rollback"] and discarded_report["previous_restored"], discarded_report
            assert discarded_report["candidate_failure"] == "candidate presentation discarded", discarded_report
            assert discarded_report["handled_candidate_discards"] == 1, discarded_report
            assert discarded_report["discarded_frame_ids"] == [discarded_report["candidate_publication_id"]], discarded_report
            assert not discarded_report["candidate_first_frame_confirmed"] and not discarded_report["candidate_readiness_accepted"], discarded_report
            assert not discarded_report["candidate"]["playback_started"] and [start["source"] for start in discarded_report["clock_starts"]] == [0, 0], discarded_report
            assert discarded_report["witness_button_events"] == 4 and not discarded_report["fault_injected"], discarded_report
            assert next(frame for frame in discarded_report["frames"] if frame["kind"] == "rollback")["pts_ns"] == discarded_report["rollback_pts_ns"], discarded_report
            assert discarded_report["removed_candidate_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), discarded_report
            assert session.capture(other).tobytes() == other_baseline, "discard recovery changed other output"
            wait_for(empty, "discarded candidate restoration cleanup")

            broken_restore, log = spawn(extra=["--discard-restoration-submission"])
            session.expect_log(broken_restore, log, "Replacement candidate cached; previous video continues")
            command(broken_restore, b"publish\n")
            session.expect_log(broken_restore, log, "Replacement candidate video presented; pointer reached witness")
            command(broken_restore, b"rollback\n")
            assert not finish(broken_restore).strip() and broken_restore.returncode != 0, session.diagnostics()
            assert "previous/restoration/finalized presentation discarded" in session.text(log), session.diagnostics()
            wait_for(empty, "discarded restoration negative control cleanup")

            finalized, log = spawn()
            session.expect_log(finalized, log, "Replacement candidate cached; previous video continues")
            command(finalized, b"publish\n")
            session.expect_log(finalized, log, "Replacement candidate video presented; pointer reached witness")
            command(finalized, b"finalize\n")
            session.expect_log(finalized, log, "Replacement finalized; previous source retired; candidate video continues")
            finalized_markers = moving(finalized, 1, 7)
            finalized.send_signal(signal.SIGINT)
            committed = json.loads(finish(finalized))
            assert finalized.returncode == 0 and committed["status"] == "passed", session.diagnostics()
            validate(committed)
            assert committed["finalized"] and not committed["previous_restored"] and not committed["candidate_failure"], committed
            assert committed["candidate"]["completed_loops"] >= 1 and len(finalized_markers) == 12, committed
            assert committed["retired_previous_lifecycle"] == dict(LIFECYCLE, surface_destructions=1, context_destructions=0, display_terminations=0), committed
            assert [start["source"] for start in committed["clock_starts"]] == [0, 1], committed
            later = [frame for frame in committed["frames"] if frame["id"] > committed["finalization_frame_id"]]
            assert later and all(frame["source"] == 1 and frame["visible"] for frame in later), committed
            assert session.capture(other).tobytes() == other_baseline, "finalization changed other output"
            wait_for(empty, "finalized candidate cleanup")

            renewed, log = spawn(extra=["--reprepare-after-finalize", str(videos[0])])
            session.expect_log(renewed, log, "Replacement candidate cached; previous video continues")
            command(renewed, b"publish\n")
            session.expect_log(renewed, log, "Replacement candidate video presented; pointer reached witness")
            command(renewed, b"finalize\n")
            session.expect_log(renewed, log, "Replacement finalized; previous source retired; candidate video continues")
            until = time.monotonic() + 8
            while time.monotonic() < until:
                pixels(renewed, [1])
                if "Replacement next candidate cached; committed video continues" in session.text(log):
                    break
            else:
                raise AssertionError("retired native branch could not prepare the next candidate")
            renewed_markers = moving(renewed, 1, 7)
            command(renewed, b"abort\n")
            session.expect_log(renewed, log, "Replacement re-prepared candidate removed; committed video continues")
            moving(renewed, 1)
            renewed.send_signal(signal.SIGINT)
            renewal = json.loads(finish(renewed))
            assert renewed.returncode == 0 and renewal["status"] == "passed", session.diagnostics()
            validate(renewal, surfaces=3)
            next_candidate = renewal["reprepared"]
            assert renewal["finalized"] and not renewal["previous_restored"] and renewal["witness_button_events"] == 4, renewal
            assert next_candidate["attempted"] and next_candidate["aborted"] and not next_candidate["failure"], renewal
            assert next_candidate["candidate"]["prepared"] and not next_candidate["candidate"]["playback_started"], renewal
            assert next_candidate["candidate"]["delivery"]["received"] == 1 and next_candidate["report_error"] is None, renewal
            assert next_candidate["removed_lifecycle"] == dict(LIFECYCLE, surface_creations=3, surface_destructions=2, context_destructions=0, display_terminations=0), renewal
            new_frames = [frame for frame in renewal["frames"] if frame["generation"] == 2]
            assert new_frames and all(not frame["visible"] and frame["pts_ns"] == 0 for frame in new_frames), renewal
            assert [start["source"] for start in renewal["clock_starts"]] == [0, 1], renewal
            assert len(renewed_markers) == 12 and renewal["candidate"]["completed_loops"] >= 1, renewal
            assert session.capture(other).tobytes() == other_baseline, "branch recreation changed other output"
            wait_for(empty, "re-prepared candidate cleanup")

            for bad_path in [invalid_media, directory / "missing-next.webm"]:
                bad_next, log = spawn(extra=["--reprepare-after-finalize", str(bad_path)])
                session.expect_log(bad_next, log, "Replacement candidate cached; previous video continues")
                command(bad_next, b"publish\n")
                session.expect_log(bad_next, log, "Replacement candidate video presented; pointer reached witness")
                command(bad_next, b"finalize\n")
                session.expect_log(bad_next, log, "Replacement re-prepared candidate removed; committed video continues")
                moving(bad_next, 1)
                bad_next.send_signal(signal.SIGINT)
                next_failure = json.loads(finish(bad_next))
                assert bad_next.returncode == 0 and next_failure["status"] == "passed", session.diagnostics()
                validate(next_failure, surfaces=3)
                assert next_failure["finalized"] and next_failure["reprepared"]["failure"] and not next_failure["reprepared"]["aborted"], next_failure
                assert next_failure["reprepared"]["removed_lifecycle"] == dict(LIFECYCLE, surface_creations=3, surface_destructions=2, context_destructions=0, display_terminations=0), next_failure
                assert [start["source"] for start in next_failure["clock_starts"]] == [0, 1] and next_failure["witness_button_events"] == 4, next_failure
                wait_for(empty, "failed next candidate keeps committed video")

            controller_cpu = []
            for stage in ["prepared", "out_of_order", "published", "finalized", "next_prepared", "malformed", "truncated", "overflow"]:
                extra = ["--reprepare-after-finalize", str(videos[0])] if stage == "next_prepared" else []
                orphan, log = spawn(extra=extra, stream=True)
                session.expect_log(orphan, log, "Replacement candidate cached; previous video continues")
                if stage not in ["prepared", "out_of_order"]:
                    if stage == "published":
                        command(orphan, b"pub")
                        moving(orphan, 0)
                        assert "Replacement candidate video presented" not in session.text(log), session.diagnostics()
                        command(orphan, b"lish\n")
                    else:
                        command(orphan, b"publish\n")
                    session.expect_log(orphan, log, "Replacement candidate video presented; pointer reached witness")
                if stage in ["finalized", "next_prepared"]:
                    command(orphan, b"finalize\n")
                    session.expect_log(orphan, log, "Replacement finalized; previous source retired; candidate video continues")
                    if stage == "next_prepared":
                        session.expect_log(orphan, log, "Replacement next candidate cached; committed video continues")
                if stage == "out_of_order":
                    command(orphan, b"rollback\n")
                elif stage == "overflow":
                    command(orphan, b"x" * 80)
                elif stage == "malformed":
                    command(orphan, b"unknown\n")
                else:
                    if stage == "truncated": command(orphan, b"roll")
                    controls[orphan.pid].close()
                session.expect_log(orphan, log, "Replacement controller lost; preserving retained assignment")
                if stage in ["prepared", "out_of_order"]:
                    session.expect_log(orphan, log, "Replacement candidate removed; previous video retained")
                elif stage == "next_prepared":
                    session.expect_log(orphan, log, "Replacement re-prepared candidate removed; committed video continues")
                elif stage not in ["finalized"]:
                    session.expect_log(orphan, log, "Replacement previous video restored; pointer reached witness")
                before = cpu_seconds(orphan.pid)
                moving(orphan, 1 if stage in ["finalized", "next_prepared"] else 0)
                controller_cpu.append(cpu_seconds(orphan.pid) - before)
                assert controller_cpu[-1] < .5, ("closed controller caused a busy loop", stage, controller_cpu[-1])
                orphan.send_signal(signal.SIGINT)
                orphan_report = json.loads(finish(orphan))
                assert orphan.returncode == 0 and orphan_report["status"] == "passed", session.diagnostics()
                validate(orphan_report, surfaces=3 if stage == "next_prepared" else 2)
                assert orphan_report["controller_failure"] and orphan_report["control_transport"] == "inherited_unix_stream", orphan_report
                if stage in ["finalized", "next_prepared"]:
                    assert orphan_report["finalized"] and not orphan_report["previous_restored"] and not orphan_report["automatic_rollback"], orphan_report
                    assert [start["source"] for start in orphan_report["clock_starts"]] == [0, 1] and orphan_report["witness_button_events"] == 4, orphan_report
                    if stage == "next_prepared": assert orphan_report["reprepared"]["failure"] and not orphan_report["reprepared"]["aborted"], orphan_report
                elif stage in ["prepared", "out_of_order"]:
                    assert not orphan_report["candidate"]["playback_started"] and not orphan_report["candidate_presented"], orphan_report
                    assert [start["source"] for start in orphan_report["clock_starts"]] == [0] and orphan_report["witness_button_events"] == 2, orphan_report
                else:
                    assert orphan_report["automatic_rollback"] and orphan_report["previous_restored"], orphan_report
                    assert [start["source"] for start in orphan_report["clock_starts"]] == [0, 1, 0] and orphan_report["witness_button_events"] == 6, orphan_report
                    assert next(frame for frame in orphan_report["frames"] if frame["kind"] == "rollback")["pts_ns"] == orphan_report["rollback_pts_ns"], orphan_report
                assert session.capture(other).tobytes() == other_baseline, "controller loss changed other output"
                wait_for(empty, "controller-loss cleanup")

            for message, error in [(b"rollback\n", "invalid replacement command order"),
                                   (b"finalize\n", "invalid replacement command order"),
                                   (b"unknown\n", "invalid nested diagnostic command"),
                                   (b"publish\n" + b"x"*64, "invalid nested diagnostic command")]:
                invalid, log = spawn()
                session.expect_log(invalid, log, "Replacement candidate cached; previous video continues")
                command(invalid, message)
                assert not finish(invalid).strip() and invalid.returncode != 0, session.diagnostics()
                assert error in session.text(log), session.diagnostics()
                wait_for(empty, "invalid replacement command cleanup")

            interrupted, log = spawn()
            session.expect_log(interrupted, log, "Replacement candidate cached; previous video continues")
            interrupted.send_signal(signal.SIGINT)
            interrupt_report = json.loads(finish(interrupted))
            assert interrupted.returncode == 0 and interrupt_report["status"] == "interrupted", session.diagnostics()
            validate(interrupt_report)
            assert not interrupt_report["candidate"]["playback_started"], interrupt_report
            wait_for(empty, "interrupted replacement cleanup")

            negative, log = spawn(extra=["--intercept-input"])
            assert not finish(negative).strip() and negative.returncode != 0, session.diagnostics()
            assert "replacement surface intercepted private input" in session.text(log), session.diagnostics()
            wait_for(empty, "input negative control cleanup")
            for visible in [False, True]:
                crashed, log = spawn()
                session.expect_log(crashed, log, "Replacement candidate cached; previous video continues")
                if visible:
                    command(crashed, b"publish\n")
                    session.expect_log(crashed, log, "Replacement candidate video presented; pointer reached witness")
                crashed.kill()
                assert not finish(crashed).strip() and crashed.returncode == -signal.SIGKILL
                wait_for(empty, "replacement SIGKILL cleanup")
            if args.nvidia:
                compositor_log = session.text(session.log)
                assert "NVIDIA" in compositor_log and str(node) in compositor_log, compositor_log[-16_384:]
                native_log = session.text(log)
                assert "using GL memory" in native_log and "fallback to system memory" not in native_log, "missing decoder GL-transfer diagnostics"
            lost, log = spawn()
            session.expect_log(lost, log, "Replacement candidate cached; previous video continues")
            session.process.terminate()
            session.process.wait(timeout=10)
            assert not finish(lost).strip() and lost.returncode != 0, session.diagnostics()
        except Exception:
            if args.artifact_directory:
                args.artifact_directory.mkdir(parents=True, exist_ok=False)
                for path in runtime.iterdir():
                    if path.is_file() and path.suffix in [".log", ".json", ".png"]:
                        shutil.copy2(path, args.artifact_directory / path.name)
            print(session.diagnostics())
            raise
        finally:
            for endpoint in controls.values():
                if isinstance(endpoint, socket.socket): endpoint.close()
            session.close()
    print(json.dumps({"video_replacement":"passed", "gl_api":args.gl_api, "nvidia":args.nvidia,
                      "gl":report["gl"], "visible_markers":[sorted(old_seen),sorted(candidate_seen),sorted(restored_seen)],
                      "sampled_compositor_frames":captures,"max_compositor_channel_error":maximum_error,
                      "egl_lifecycle":report["egl_lifecycle"],"consumer_fences":report["consumer_fences"],
                      "early_cancellation_feedback":feedback,
                      "early_candidate_was_presented":early_report["candidate_first_frame_confirmed"],
                      "handled_candidate_discards":discarded_report["handled_candidate_discards"],
                      "reprepared_lifecycle":renewal["egl_lifecycle"],
                      "controller_loss_max_cpu_seconds":max(controller_cpu),
                      "checks":["distinct old/new sources", "old video moves while candidate paused", "presentation-gated clocks",
                                "sampled publication/rollback pixels", "rollback retained PTS", "video moves after restoration",
                                "abort and malformed candidate retain moving previous video", "real input", "unchanged second output",
                                "injected post-publication media error restores previous video", "failed/rolled-back candidate retired",
                                "failed candidate readiness never starts its clock", "late cancelled feedback cannot complete restoration",
                                "real candidate discard restores previous video", "discarded restoration fails without claiming recovery",
                                "finalize retires previous source while candidate keeps moving and looping",
                                "recreate retired branch and abort next hidden candidate without replacing root/context",
                                "malformed next candidate retains moving committed video",
                                "controller EOF/malformed input preserves or restores the retained assignment without a busy loop",
                                "presentation receipts gate clocks when diagnostic history is full",
                                "input negative control", "SIGKILL", "compositor loss"]}))


if __name__ == "__main__":
    main()
