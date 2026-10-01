"""A bounded RFC 6455 client for the isolated acceptance server."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import socket
import struct
import time
import urllib.parse


class SocketClient:
    def __init__(self, base_url: str, token: str):
        parsed = urllib.parse.urlsplit(base_url)
        if parsed.scheme != "http" or parsed.hostname not in {"localhost", "127.0.0.1", "::1"}:
            raise AssertionError("acceptance sockets require an isolated loopback HTTP server")
        self.socket = socket.create_connection((parsed.hostname, parsed.port or 80), timeout=5)
        self.socket.settimeout(5)
        self.pending = bytearray()
        try:
            key = base64.b64encode(os.urandom(16)).decode("ascii")
            path = parsed.path.rstrip("/") + "/socket"
            headers = (
                f"GET {path} HTTP/1.1\r\nHost: {parsed.netloc}\r\n"
                f"Origin: {base_url.rstrip('/')}\r\nX-Emby-Token: {token}\r\n"
                f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            )
            self.socket.sendall(headers.encode("ascii"))
            while b"\r\n\r\n" not in self.pending:
                block = self.socket.recv(4096)
                if not block or len(self.pending) + len(block) > 16 * 1024:
                    raise AssertionError("socket upgrade ended or exceeded the header bound")
                self.pending.extend(block)
            boundary = self.pending.index(b"\r\n\r\n") + 4
            lines = bytes(self.pending[:boundary]).decode("ascii").split("\r\n")
            del self.pending[:boundary]
            if lines[0].split(" ")[1] != "101":
                raise AssertionError("authenticated socket upgrade did not return HTTP 101")
            fields = {}
            for line in lines[1:]:
                if not line:
                    continue
                name, value = line.split(":", 1)
                if name.lower() in fields:
                    raise AssertionError("socket upgrade contained duplicate response headers")
                fields[name.lower()] = value.strip()
            expected = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()).decode("ascii")
            if fields.get("sec-websocket-accept") != expected or fields.get("upgrade", "").lower() != "websocket":
                raise AssertionError("socket upgrade did not validate its RFC 6455 challenge")
            initial = self.event("ForceKeepAlive")
            if initial.get("Data") != 90:
                raise AssertionError("socket did not advertise its bounded keepalive interval")
            self.send(1, b'{"MessageType":"KeepAlive"}')
            self.event("KeepAlive")
        except BaseException:
            self.socket.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()

    def close(self):
        try:
            self.send(8, struct.pack("!H", 1000))
        except OSError:
            pass
        self.socket.close()

    def send(self, opcode: int, payload: bytes):
        if len(payload) > 16 * 1024:
            raise AssertionError("acceptance socket message exceeds its bound")
        mask = os.urandom(4)
        length = bytes([len(payload) | 128]) if len(payload) < 126 else bytes([126 | 128]) + struct.pack("!H", len(payload))
        self.socket.sendall(bytes([128 | opcode]) + length + mask + bytes(
            value ^ mask[index % 4] for index, value in enumerate(payload)
        ))

    def read(self, count: int) -> bytes:
        while len(self.pending) < count:
            block = self.socket.recv(min(4096, count - len(self.pending)))
            if not block:
                raise AssertionError("socket ended before its expected frame")
            self.pending.extend(block)
        value = bytes(self.pending[:count])
        del self.pending[:count]
        return value

    def frame(self):
        first, second = self.read(2)
        if first & 112 or not first & 128 or second & 128:
            raise AssertionError("server sent an unexpected compressed, fragmented or masked frame")
        length = second & 127
        if length == 126:
            length = struct.unpack("!H", self.read(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self.read(8))[0]
        if length > 16 * 1024 or (first & 15) >= 8 and length > 125:
            raise AssertionError("server socket frame exceeded its bound")
        return first & 15, self.read(length)

    def event(self, expected: str):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            self.socket.settimeout(max(0.01, deadline - time.monotonic()))
            opcode, payload = self.frame()
            if opcode == 9:
                self.send(10, payload)
                continue
            if opcode != 1:
                raise AssertionError("socket did not send the expected text event")
            event = json.loads(payload)
            if event.get("MessageType") == expected:
                return event
            if event.get("MessageType") == "ForceKeepAlive":
                self.send(1, b'{"MessageType":"KeepAlive"}')
                continue
            raise AssertionError("socket sent an unexpected notification type")
        raise AssertionError("socket notification timed out")

    def expect_quiet(self):
        self.socket.settimeout(0.25)
        try:
            self.frame()
        except TimeoutError:
            return
        raise AssertionError("socket received another account's notification")

    def expect_shutdown(self):
        self.socket.settimeout(5)
        opcode, payload = self.frame()
        if opcode != 8 or len(payload) < 2 or struct.unpack("!H", payload[:2])[0] != 1001:
            raise AssertionError("server shutdown did not close its socket with code 1001")
