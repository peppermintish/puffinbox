#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Build an experimental shared std and select it for the server executable."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


def jobserver_fds():
    match = re.search(r"--jobserver-(?:fds|auth)=(\d+),(\d+)", os.environ.get("CARGO_MAKEFLAGS", ""))
    if not match:
        return ()
    descriptors = tuple(int(part) for part in match.groups())
    for descriptor in descriptors:
        os.fstat(descriptor)
    return descriptors


def main():
    compiler, *arguments = sys.argv[1:]
    output = Path(os.environ["PUFFINBOX_RUNTIME_PROBE_OUTPUT"])
    name = arguments[arguments.index("--crate-name") + 1] if "--crate-name" in arguments else None
    original = list(arguments)
    if name == "std":
        index = arguments.index("--crate-type") + 1
        if arguments[index] != "rlib":
            raise SystemExit("The std producer no longer has the expected rlib crate type.")
        arguments[index] = "rlib,dylib"
        # Cargo can start std from dependency metadata while its archives are
        # still being written. The additional dylib needs those actual inputs.
        deadline = time.monotonic() + 120
        for index in range(len(arguments) - 1):
            if arguments[index] == "--extern" and arguments[index + 1].endswith(".rmeta"):
                prefix, path = arguments[index + 1].split("=", 1)
                archive = Path(path).with_suffix(".rlib")
                while not archive.is_file():
                    if time.monotonic() >= deadline:
                        raise SystemExit(f"Dependency archive did not become available: {archive}")
                    time.sleep(0.05)
                arguments[index + 1] = prefix + "=" + str(archive)
    elif name == "puffinbox_server":
        libraries = list((output / "target/x86_64-unknown-linux-gnu/release/deps").glob("libstd-*.so"))
        if len(libraries) != 1:
            raise SystemExit("Expected one rebuilt shared standard library.")
        arguments += ["--extern", "std=" + str(libraries[0]), "-C", "linker=" + str(Path(__file__).with_name("linker-wrapper.py"))]
    if name in {"std", "puffinbox_server"}:
        with (output / f"{name}-rustc-argv.json").open("x") as record:
            json.dump({"compiler": compiler, "original": original, "modified": arguments}, record, indent=2)
    return subprocess.call([compiler, *arguments], pass_fds=jobserver_fds())


if __name__ == "__main__":
    raise SystemExit(main())
