#!/usr/bin/env python3
"""Fail unless an FFmpeg executable exposes the fd protocol used by safe input handling."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys


def check(program: str) -> str:
    result = subprocess.run(
        [program, "-hide_banner", "-protocols"],
        check=False,
        capture_output=True,
        text=True,
        timeout=15,
    )
    if result.returncode:
        raise RuntimeError(f"{program} -protocols failed ({result.returncode}): {(result.stderr or result.stdout)[-1200:]}")
    protocols = {line.strip() for line in result.stdout.splitlines() if re.fullmatch(r"\s*[a-zA-Z0-9_+-]+\s*", line)}
    if "fd" not in protocols:
        raise RuntimeError(f"{program} does not expose FFmpeg's fd protocol required by the server's confined media reader")
    return program


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("program", nargs="?", default="ffmpeg")
    args = parser.parse_args()
    try:
        version = check(args.program)
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    print(f"{version}: FFmpeg fd protocol is available.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
