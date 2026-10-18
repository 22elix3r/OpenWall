#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Opt-in Linux video worker measurements; does not measure compositor/GPU energy."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
LOG_LIMIT = 16_384


class Diagnostics:
    def __init__(self):
        self.tail = b""
        self.partial = b""
        self.discard_line = False
        self.first_buffer = None
        self.ready_at = None
        self.decoders = []
        self.loops = 0
        self.scene_gpu = False
        self.native_parent = False

    def feed(self, chunk):
        self.tail = (self.tail + chunk)[-LOG_LIMIT:]
        lines = (self.partial + chunk).split(b"\n")
        for raw in lines[:-1]:
            if not self.discard_line and len(raw) <= LOG_LIMIT:
                self.line(raw.decode(errors="replace"))
            self.discard_line = False
        self.partial = lines[-1]
        if len(self.partial) > LOG_LIMIT:
            self.partial = b""
            self.discard_line = True

    def line(self, line):
        prefix = "First video buffer: DMA-BUF="
        if line.startswith(prefix) and self.first_buffer is None:
            value, separator, caps = line[len(prefix):].partition("; caps=")
            if separator and value in ("true", "false"):
                self.first_buffer = {"dmabuf": value == "true", "caps": caps}
                self.ready_at = time.monotonic()
        if line.startswith("GStreamer decoder: ") and len(self.decoders) < 32:
            decoder = line.removeprefix("GStreamer decoder: ")
            if decoder not in self.decoders:
                self.decoders.append(decoder)
        self.loops += line == "Video loop restarted"
        self.scene_gpu |= line.startswith("OpenWall GPU:")
        self.native_parent |= line.startswith("Native video attached on ")


def process_sample(pid):
    # comm may contain spaces and parentheses. Fields after its final ')' start at state.
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    ticks = int(fields[11]) + int(fields[12])
    rss = int(fields[21]) * os.sysconf("SC_PAGE_SIZE")
    return time.monotonic(), ticks, rss


def fingerprint(path):
    with path.open("rb") as stream:
        return {"name": path.name, "bytes": path.stat().st_size,
                "sha256": hashlib.file_digest(stream, "sha256").hexdigest()}


def drain(selector, diagnostics, timeout=0):
    for key, _ in selector.select(timeout):
        # Bounded work per iteration: continuous noise cannot starve the deadline.
        for _ in range(16):
            try:
                chunk = os.read(key.fd, 8192)
            except BlockingIOError:
                break
            if not chunk:
                selector.unregister(key.fileobj)
                break
            diagnostics.feed(chunk)


def stop(child, selector, diagnostics, grace):
    forced = False
    try:
        os.killpg(child.pid, signal.SIGINT)
        deadline = time.monotonic() + grace
        while child.poll() is None and time.monotonic() < deadline:
            drain(selector, diagnostics, .05)
        forced = child.poll() is None
    except ProcessLookupError:
        pass
    finally:
        # The worker has its own process group; never signal the caller's group.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=5)
        drain(selector, diagnostics)
    return forced


def benchmark(binary, video, output, seconds=30, warmup=5, startup_timeout=15, stop_grace=3,
              env=None):
    env = os.environ if env is None else env
    report = {"schema_version": 1, "status": "failed", "scope": "worker_process_only",
              "binary": fingerprint(binary), "video": fingerprint(video), "output": output,
              "system": {"os": platform.system(), "release": platform.release(),
                         "architecture": platform.machine()},
              "requested_sample_seconds": seconds, "warmup_seconds": warmup,
              "environment": {key: env[key][:1024] for key in
                              ["GST_PLUGIN_FEATURE_RANK", "GST_PLUGIN_SYSTEM_PATH_1_0",
                               "GST_PLUGIN_PATH_1_0", "GST_REGISTRY_1_0", "LIBVA_DRIVER_NAME",
                               "LIBVA_DRIVERS_PATH", "DRI_PRIME"] if key in env}}
    diagnostics = Diagnostics()
    started = time.monotonic()
    child = subprocess.Popen([str(binary), "apply", str(video), "--output", output],
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             start_new_session=True, env=env)
    with selectors.DefaultSelector() as selector, child.stdout:
        os.set_blocking(child.stdout.fileno(), False)
        selector.register(child.stdout, selectors.EVENT_READ)
        baseline = last = None
        rss_sum = rss_peak = count = 0
        try:
            while True:
                drain(selector, diagnostics, .1)
                now = time.monotonic()
                if child.poll() is not None:
                    raise RuntimeError(f"worker exited before the sample completed ({child.returncode})")
                if diagnostics.scene_gpu:
                    raise RuntimeError("expected a native video desktop worker without a scene GPU")
                # The streaming thread can report its buffer before the main thread
                # prints the parent setup message. Both orders are valid.
                if diagnostics.ready_at is None or not diagnostics.native_parent:
                    if now - started >= startup_timeout:
                        raise RuntimeError("native parent/sink buffer not ready before startup deadline")
                    continue
                if now < diagnostics.ready_at + warmup:
                    continue
                sample = process_sample(child.pid)
                if baseline is None:
                    baseline = sample
                if last is None or sample[0] - last[0] >= .25 or sample[0] - baseline[0] >= seconds:
                    last = sample
                    rss_sum += sample[2]
                    rss_peak = max(rss_peak, sample[2])
                    count += 1
                if sample[0] - baseline[0] >= seconds:
                    elapsed = sample[0] - baseline[0]
                    report.update(status="passed", sample_seconds=round(elapsed, 3),
                                  cpu_percent_one_core=round((sample[1] - baseline[1]) /
                                      os.sysconf("SC_CLK_TCK") / elapsed * 100, 3),
                                  mean_sampled_rss_mib=round(rss_sum / count / 1024**2, 2),
                                  peak_sampled_rss_mib=round(rss_peak / 1024**2, 2),
                                  rss_samples=count)
                    break
        except (RuntimeError, OSError) as error:
            report["error"] = str(error)
        finally:
            report["forced_stop"] = stop(child, selector, diagnostics, stop_grace)
    report.update(exit_code=child.returncode, decoders=diagnostics.decoders,
                  first_sink_buffer=diagnostics.first_buffer, loops=diagnostics.loops,
                  startup_to_sink_buffer_seconds=round(diagnostics.ready_at - started, 3)
                      if diagnostics.ready_at is not None else None,
                  diagnostics_tail=diagnostics.tail.decode(errors="replace"))
    if report["forced_stop"] or child.returncode != 0:
        report.update(status="failed", error=report.get("error", "worker did not stop cleanly"))
    return report


def bounded_float(minimum, maximum):
    def parse(value):
        result = float(value)
        if not minimum <= result <= maximum:
            raise argparse.ArgumentTypeError(f"expected {minimum}..{maximum}")
        return result
    return parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("video", type=Path)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/openwall")
    parser.add_argument("--output", required=True, help="one layer-shell output connector")
    parser.add_argument("--seconds", type=bounded_float(1, 3600), default=30)
    parser.add_argument("--warmup", type=bounded_float(0, 300), default=5)
    parser.add_argument("--startup-timeout", type=bounded_float(1, 120), default=15)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("Linux /proc process accounting is required")
    try:
        report = benchmark(args.binary.resolve(strict=True), args.video.resolve(strict=True),
                           args.output, args.seconds, args.warmup, args.startup_timeout)
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        report = {"schema_version": 1, "status": "failed", "error": str(error)}
    print(json.dumps(report, indent=2))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
