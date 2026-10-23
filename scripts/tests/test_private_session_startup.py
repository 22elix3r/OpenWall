#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Private compositor readiness must survive IPC socket/startup ordering."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_wayland_headless as headless


class PrivateSessionStartup(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        runtime = Path(self.directory.name)
        for name in ["sway-ipc.private.sock", "wayland-private"]:
            endpoint = socket.socket(socket.AF_UNIX)
            endpoint.bind(str(runtime / name))
            self.addCleanup(endpoint.close)
        self.session = headless.Session.__new__(headless.Session)
        self.session.env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime))
        self.session.process = Mock()
        self.session.process.poll.return_value = None
        self.session.diagnostics = Mock(return_value="private compositor diagnostic tail")
        self.outputs = subprocess.CompletedProcess(["swaymsg"], 0, json.dumps([{}, {}]), "")

    def test_bound_socket_can_precede_ipc_readiness(self):
        for error in [subprocess.CalledProcessError(1, ["swaymsg"], stderr="Connection refused"),
                      subprocess.TimeoutExpired(["swaymsg"], .01, stderr="IPC not yet servicing")]:
            with self.subTest(error=type(error).__name__), patch.object(
                    headless.subprocess, "run", side_effect=[error, self.outputs]) as query:
                self.session.connect()
                self.assertEqual(query.call_count, 2)
                self.assertEqual(self.session.env["WAYLAND_DISPLAY"], "wayland-private")
                self.assertEqual(Path(query.call_args.args[0][2]).parent, Path(self.directory.name))
                # Readiness retries do not apply to subsequent runtime commands.
                query.side_effect = error
                with self.assertRaises(type(error)):
                    self.session.ipc("output HEADLESS-1 disable")

    def test_permanent_ipc_failure_is_bounded_and_diagnosed(self):
        original_wait = headless.wait_for
        def brief(check, description):
            return original_wait(check, description, timeout=.15)
        error = subprocess.CalledProcessError(1, ["swaymsg"], stderr="permanent IPC failure")
        with patch.object(headless, "wait_for", side_effect=brief), patch.object(
                headless.subprocess, "run", side_effect=error):
            with self.assertRaises(AssertionError) as failure:
                self.session.connect()
        message = str(failure.exception)
        self.assertIn("Timed out: two headless outputs", message)
        self.assertIn("permanent IPC failure", message)
        self.assertIn("private compositor diagnostic tail", message)

    def test_compositor_exit_during_query_fails_without_retry(self):
        self.session.process.poll.side_effect = [None, None, 7]
        with patch.object(headless.subprocess, "run", side_effect=subprocess.CalledProcessError(
                1, ["swaymsg"], stderr="IPC closed")) as query:
            with self.assertRaisesRegex(AssertionError, "private compositor exited during startup"):
                self.session.connect()
            self.assertEqual(query.call_count, 1)


if __name__ == "__main__":
    unittest.main()
