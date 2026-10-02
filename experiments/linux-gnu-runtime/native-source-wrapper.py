#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Record native compiler inputs before build scripts remove temporary sources."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import uuid


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def dependency_paths(content: str, directory: Path) -> list[Path]:
    logical = content.replace("\\\n", "").splitlines()
    if not logical or ":" not in logical[0]:
        raise ValueError("Unsupported native compiler dependency rule.")
    _, dependencies = logical[0].split(":", 1)
    names = shlex.split(dependencies.replace("$$", "$"), comments=False)
    if not names:
        raise ValueError("Native compiler emitted an empty dependency rule.")
    return [Path(os.path.abspath(directory / name)) for name in names]


def jobserver_fds() -> tuple[int, ...]:
    descriptors = set()
    for name in ("CARGO_MAKEFLAGS", "MAKEFLAGS"):
        for match in re.finditer(r"--jobserver-(?:auth|fds)=(\d+),(\d+)", os.environ.get(name, "")):
            for value in match.groups():
                descriptor = int(value)
                try:
                    os.fstat(descriptor)
                except OSError:
                    continue
                descriptors.add(descriptor)
    return tuple(sorted(descriptors))


def main() -> int:
    output = Path(os.environ["PUFFINBOX_RUNTIME_PROBE_OUTPUT"])
    if not output.is_absolute() or not output.is_dir() or output.resolve() != output:
        raise ValueError("Native compiler tracing requires an existing absolute probe directory.")
    directory = Path.cwd()
    arguments = sys.argv[1:]
    sources = [Path(os.path.abspath(directory / argument)) for argument in arguments
               if not argument.startswith("-") and Path(argument).suffix in {".c", ".cc", ".cpp", ".S", ".s"}
               and (directory / argument).is_file()]
    compile_source = "-c" in arguments and bool(sources)
    if compile_source and len(sources) != 1:
        raise ValueError("Native compiler tracing supports one compile source per invocation.")
    traces = output / "native-compiler-traces"
    traces.mkdir(exist_ok=True)
    identifier = uuid.uuid4().hex
    dependencies = traces / (identifier + ".d")
    compiler = Path("/usr/bin/cc")
    command = [str(compiler), *arguments]
    preprocessed = compile_source and sources[0].suffix != ".s"
    if preprocessed:
        command += ["-MD", "-MF", str(dependencies)]
    result = subprocess.run(command, pass_fds=jobserver_fds())
    paths = list(sources)
    if result.returncode == 0 and preprocessed:
        paths += dependency_paths(dependencies.read_text(), directory)
    hashes = {str(path): digest(path) for path in sorted(set(paths))}
    record = {"compiler": str(compiler), "compilerSha256": digest(compiler), "directory": str(directory),
              "arguments": arguments, "exitCode": result.returncode, "compileSource": compile_source,
              "dependencyRuleCaptured": result.returncode == 0 and preprocessed, "sourceFiles": hashes,
              "licenseClearance": False,
              "scope": "Native source and compiler-reported include bytes available before this invocation returns. "
                       "Compiler probes and failed invocations do not establish retained executable inputs."}
    with (traces / (identifier + ".json")).open("x") as ledger:
        json.dump(record, ledger, indent=2)
        ledger.write("\n")
    return result.returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError) as error:
        print(f"Native compiler input capture failed: {error}", file=sys.stderr)
        raise SystemExit(1)
