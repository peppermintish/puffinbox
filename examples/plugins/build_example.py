#!/usr/bin/env python3
"""Encode the original metadata hook without a third-party Wasm compiler.

This is a fixed example encoder, not a parser for arbitrary WAT source.
metadata-enricher.wat describes the same hook for review. MIT OR Apache-2.0.
"""

import hashlib
from pathlib import Path


def unsigned(value: int) -> bytes:
    encoded = bytearray()
    while value >= 128:
        encoded.append((value & 127) | 128)
        value >>= 7
    encoded.append(value)
    return bytes(encoded)


def section(identifier: int, payload: bytes) -> bytes:
    return bytes([identifier]) + unsigned(len(payload)) + payload


def build() -> bytes:
    output = b'{"overview":"Applied the original example metadata hook."}'
    # Four i32 parameters and one i32 result; one function of that type.
    types = section(1, b"\x01\x60\x04\x7f\x7f\x7f\x7f\x01\x7f")
    functions = section(3, b"\x01\x00")
    memory = section(5, b"\x01\x01\x01\x10")
    exports = section(7, b"\x02\x06memory\x02\x00\x06enrich\x00\x00")
    # If capacity < 58 return -1; otherwise copy the fixed output to output_ptr.
    instructions = (b"\x20\x03\x41\x3a\x49\x04\x7f\x41\x7f\x05"
                    b"\x20\x02\x41\x00\x41\x3a\xfc\x0a\x00\x00\x41\x3a\x0b\x0b")
    body = b"\x00" + instructions
    code = section(10, b"\x01" + unsigned(len(body)) + body)
    data = section(11, b"\x01\x00\x41\x00\x0b" + unsigned(len(output)) + output)
    return b"\0asm\x01\0\0\0" + types + functions + memory + exports + code + data


if __name__ == "__main__":
    binary = build()
    Path(__file__).with_name("metadata-enricher.wasm").write_bytes(binary)
    print(hashlib.sha256(binary).hexdigest())
