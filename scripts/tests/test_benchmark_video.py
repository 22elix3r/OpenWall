# SPDX-License-Identifier: AGPL-3.0-or-later
"""Exercise accounting, bounded pipes and deadlines with real disposable processes."""
from pathlib import Path
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from benchmark_video import LOG_LIMIT, benchmark


@unittest.skipUnless(sys.platform == "linux", "requires Linux /proc")
class BenchmarkProcesses(unittest.TestCase):
    def run_worker(self, body, **kwargs):
        with tempfile.TemporaryDirectory(prefix="openwall-benchmark-test-") as temporary:
            directory = Path(temporary)
            worker = directory / "worker"
            worker.write_text(f"#!{sys.executable}\nimport os, signal, sys, time\n" + body)
            worker.chmod(0o700)
            video = directory / "original.webm"
            video.write_bytes(b"test process accounting, not media decoding\n")
            return benchmark(worker, video, "HEADLESS-1", seconds=.5, warmup=.05,
                             startup_timeout=.5, stop_grace=.15, **kwargs)

    def test_busy_worker_with_noise_preserves_readiness_and_measures_cpu(self):
        report = self.run_worker(
            "import ctypes\n"
            "ctypes.CDLL(None).prctl(15, b'a)b benchmark', 0, 0, 0)\n"
            "signal.signal(signal.SIGINT, lambda *_: sys.exit(0))\n"
            "print('Native video attached on HEADLESS-1', flush=True)\n"
            "print('GStreamer decoder: original-test-decoder', flush=True)\n"
            "print('First video buffer: DMA-BUF=false; caps=video/x-raw', flush=True)\n"
            "os.write(1, b'x' * 1_000_000 + b'\\n')\n"
            "while True: sum(range(100_000))\n")
        self.assertEqual(report["status"], "passed", report)
        self.assertFalse(report["forced_stop"])
        self.assertGreater(report["cpu_percent_one_core"], 1)
        self.assertGreater(report["peak_sampled_rss_mib"], 0)
        self.assertGreaterEqual(report["sample_seconds"], .5)
        self.assertEqual(report["decoders"], ["original-test-decoder"])
        self.assertFalse(report["first_sink_buffer"]["dmabuf"])
        self.assertLessEqual(len(report["diagnostics_tail"]), LOG_LIMIT)
        self.assertNotIn("First video buffer:", report["diagnostics_tail"])

    def test_noisy_never_ready_worker_is_bounded_and_forcibly_reaped(self):
        started = time.monotonic()
        report = self.run_worker(
            "signal.signal(signal.SIGINT, signal.SIG_IGN)\n"
            "while True: os.write(1, b'n' * 8192)\n")
        self.assertEqual(report["status"], "failed", report)
        self.assertIn("startup deadline", report["error"])
        self.assertTrue(report["forced_stop"])
        self.assertEqual(report["exit_code"], -9)
        self.assertIsNone(report["first_sink_buffer"])
        self.assertLessEqual(len(report["diagnostics_tail"]), LOG_LIMIT)
        self.assertLess(time.monotonic() - started, 4)

    def test_worker_exit_cannot_be_reported_as_a_completed_measurement(self):
        report = self.run_worker("print('decoder failed', flush=True)\nsys.exit(7)\n")
        self.assertEqual(report["status"], "failed", report)
        self.assertEqual(report["exit_code"], 7)
        self.assertNotIn("cpu_percent_one_core", report)
        self.assertIn("decoder failed", report["diagnostics_tail"])

    def test_streaming_thread_can_report_data_before_the_parent_setup_log(self):
        report = self.run_worker(
            "signal.signal(signal.SIGINT, lambda *_: sys.exit(0))\n"
            "print('First video buffer: DMA-BUF=false; caps=video/x-raw', flush=True)\n"
            "time.sleep(.15)\n"
            "print('Native video attached on HEADLESS-1', flush=True)\n"
            "while True: time.sleep(.05)\n")
        self.assertEqual(report["status"], "passed", report)
        self.assertFalse(report["forced_stop"])

    def test_sink_data_alone_cannot_start_a_native_worker_measurement(self):
        report = self.run_worker(
            "signal.signal(signal.SIGINT, lambda *_: sys.exit(0))\n"
            "print('First video buffer: DMA-BUF=false; caps=video/x-raw', flush=True)\n"
            "while True: time.sleep(.05)\n")
        self.assertEqual(report["status"], "failed", report)
        self.assertIn("startup deadline", report["error"])
        self.assertFalse(report["forced_stop"])
        self.assertIsNotNone(report["first_sink_buffer"])
        self.assertNotIn("cpu_percent_one_core", report)


if __name__ == "__main__":
    unittest.main()
