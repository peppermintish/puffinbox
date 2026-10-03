#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Configure vendored OpenSSL without its separately noticed SipHash source."""

import os
from pathlib import Path
import sys


def configure_arguments(arguments: list[str]) -> list[str]:
    if not arguments or Path(arguments[0]).name != "Configure":
        raise ValueError("The OpenSSL build wrapper accepts only Configure.")
    if "enable-siphash" in arguments or "enable-quic" in arguments:
        raise ValueError("Bundled OpenSSL must keep SipHash and QUIC disabled.")
    return ["perl", *arguments, "no-siphash", "no-quic"]


if __name__ == "__main__":
    try:
        command = configure_arguments(sys.argv[1:])
        os.execvp(command[0], command)
    except (ValueError, OSError) as error:
        raise SystemExit(f"OpenSSL configuration failed: {error}") from error
