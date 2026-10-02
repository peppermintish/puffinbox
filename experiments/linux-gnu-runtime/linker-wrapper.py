#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Exclude compiler-builtins from an experimental final executable link."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def main():
    output = Path(os.environ["PUFFINBOX_RUNTIME_PROBE_OUTPUT"])
    label = os.environ.get("PUFFINBOX_RUNTIME_LINK_LABEL", "server")
    if not re.fullmatch(r"[a-z-]+", label):
        raise SystemExit("Invalid link record label.")
    arguments = sys.argv[1:]
    if any(argument.startswith("@") for argument in arguments):
        raise SystemExit("Inspect the new response-file format before changing the link inputs.")
    removed = [argument for argument in arguments
               if re.fullmatch(r"libcompiler_builtins-[0-9a-f]+[.]rlib", Path(argument).name)]
    if len(removed) != 1:
        raise SystemExit(f"Expected one compiler-builtins archive, observed {len(removed)}.")
    expected_parent = output / "target/x86_64-unknown-linux-gnu/release/deps"
    if Path(removed[0]).parent.resolve() != expected_parent.resolve():
        raise SystemExit("The compiler-builtins archive is outside this probe's target directory.")
    modified = [argument for argument in arguments if argument not in removed]
    driver = shutil.which("cc")
    if not driver:
        raise SystemExit("No C linker driver is available.")
    with (output / f"{label}-linker-argv.json").open("x") as record:
        json.dump({"driver": driver, "original": arguments, "modified": modified, "removed": removed}, record, indent=2)
    return subprocess.call([driver, *modified])


if __name__ == "__main__":
    raise SystemExit(main())
