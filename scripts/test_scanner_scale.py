#!/usr/bin/env python3
"""Deterministic arithmetic checks for scanner scale benchmark item counts."""

import socket
import struct
import threading
import unittest

from scanner_scale import catalogue_shape, postgres_startup_message, probe_postgres_loopback


class CatalogueShapeTests(unittest.TestCase):
    def test_default_tree_includes_the_configured_library_root(self) -> None:
        self.assertEqual(catalogue_shape(2_048, 128), (16, 2_065))

    def test_partial_shard_adds_a_directory_and_the_library_root(self) -> None:
        self.assertEqual(catalogue_shape(129, 128), (2, 132))

    def test_single_file_tree_has_one_shard_and_one_library_root(self) -> None:
        self.assertEqual(catalogue_shape(1, 128), (1, 3))


class PostgresLoopbackProbeTests(unittest.TestCase):
    @staticmethod
    def _message(message_type: bytes, payload: bytes) -> bytes:
        return message_type + struct.pack("!I", len(payload) + 4) + payload

    def _probe_against_response(self, response: bytes | None) -> tuple[bool, bytes]:
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        port = listener.getsockname()[1]
        received = bytearray()

        def serve_one() -> None:
            with listener:
                connection, _ = listener.accept()
                with connection:
                    header = connection.recv(4)
                    if len(header) != 4:
                        return
                    size = struct.unpack("!I", header)[0]
                    received.extend(header)
                    while len(received) < size:
                        chunk = connection.recv(size - len(received))
                        if not chunk:
                            return
                        received.extend(chunk)
                    if response is not None:
                        connection.sendall(response)

        thread = threading.Thread(target=serve_one, daemon=True)
        thread.start()
        result = probe_postgres_loopback(port, "scale_user", "scale_db")
        thread.join(timeout=3)
        self.assertFalse(thread.is_alive(), "fake PostgreSQL endpoint did not finish")
        return result, bytes(received)

    def test_startup_message_contains_protocol_and_database_parameters(self) -> None:
        message = postgres_startup_message("scale_user", "scale_db")
        length = struct.unpack("!I", message[:4])[0]
        self.assertEqual(length, len(message))
        self.assertEqual(struct.unpack("!I", message[4:8])[0], 196_608)
        self.assertIn(b"user\0scale_user\0database\0scale_db\0\0", message[8:])

    def test_probe_accepts_a_postgres_authentication_challenge(self) -> None:
        response = self._message(b"R", struct.pack("!I", 10) + b"SCRAM-SHA-256\0\0")
        ready, startup = self._probe_against_response(response)
        self.assertTrue(ready)
        self.assertEqual(startup, postgres_startup_message("scale_user", "scale_db"))

    def test_probe_rejects_a_reset_before_postgres_response(self) -> None:
        ready, _ = self._probe_against_response(None)
        self.assertFalse(ready)

    def test_probe_rejects_a_postgres_error_response(self) -> None:
        ready, _ = self._probe_against_response(self._message(b"E", b"database starting"))
        self.assertFalse(ready)


if __name__ == "__main__":
    unittest.main()
