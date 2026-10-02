#!/usr/bin/env python3
"""Startup transport failures should retry without concealing server errors."""

from __future__ import annotations

import unittest
import urllib.error
from unittest.mock import Mock, patch

from acceptance import wait_for_ready


class ReadinessTests(unittest.TestCase):
    def test_reset_and_unavailable_response_retry_until_ready(self) -> None:
        client = Mock()
        client.request.side_effect = [ConnectionResetError(104, "starting"), (503, {}, b""), (200, {}, b"{}")]
        with patch("acceptance.time.monotonic", return_value=0), patch("acceptance.time.sleep"):
            wait_for_ready(client)
        self.assertEqual(client.request.call_count, 3)
        client.request.assert_called_with("GET", "/health/ready")

    def test_connection_failure_and_timeout_retry_until_ready(self) -> None:
        client = Mock()
        client.request.side_effect = [urllib.error.URLError("not listening"), TimeoutError("starting"), (200, {}, b"{}")]
        with patch("acceptance.time.monotonic", return_value=0), patch("acceptance.time.sleep"):
            wait_for_ready(client)
        self.assertEqual(client.request.call_count, 3)

    def test_bad_http_status_is_not_retried(self) -> None:
        client = Mock()
        client.request.return_value = (401, {}, b"")
        with patch("acceptance.time.monotonic", return_value=0), patch("acceptance.time.sleep") as sleep:
            with self.assertRaisesRegex(AssertionError, "HTTP 401"):
                wait_for_ready(client)
        self.assertEqual(client.request.call_count, 1)
        sleep.assert_not_called()

    def test_persistent_reset_still_expires(self) -> None:
        client = Mock()
        client.request.side_effect = ConnectionResetError(104, "starting")
        with patch("acceptance.time.monotonic", side_effect=[0, 0, 0, 2]), patch("acceptance.time.sleep"):
            with self.assertRaisesRegex(AssertionError, "startup deadline"):
                wait_for_ready(client, timeout_seconds=1)
        self.assertEqual(client.request.call_count, 1)


if __name__ == "__main__":
    unittest.main()
