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
import stat
import subprocess
import sys
import tempfile
import uuid


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def preserve_source(path: Path, store: Path) -> str:
    """Publish a complete hash-addressed copy without exposing partial files."""
    temporary = None
    try:
        with path.open("rb") as source, tempfile.NamedTemporaryFile(dir=store, delete=False) as copy:
            temporary = Path(copy.name)
            before = os.fstat(source.fileno())
            if not stat.S_ISREG(before.st_mode):
                raise ValueError("Native source capture requires a regular file.")
            value = hashlib.sha256()
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                value.update(chunk)
                copy.write(chunk)
            after = os.fstat(source.fileno())
            current = path.stat()
            identity = lambda info: (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
            if identity(before) != identity(after) or identity(before) != identity(current):
                raise ValueError("Native source changed while its bytes were being captured.")
        hexadecimal = value.hexdigest()
        blob = store / hexadecimal
        # The enclosing output is private; a host-side reviewer must also be
        # able to read files created by the disposable builder's account.
        temporary.chmod(0o444)
        try:
            os.link(temporary, blob)
        except FileExistsError:
            if not blob.is_file() or blob.resolve() != blob or digest(blob) != hexadecimal:
                raise ValueError("An existing native source copy is missing, linked or altered.")
        return hexadecimal
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def dependency_paths(content: str, directory: Path, *, allow_empty: bool = False) -> list[Path]:
    logical = content.replace("\\\n", "").splitlines()
    if not logical or ":" not in logical[0]:
        raise ValueError("Unsupported native compiler dependency rule.")
    _, dependencies = logical[0].split(":", 1)
    names = shlex.split(dependencies.replace("$$", "$"), comments=False)
    if not names and not allow_empty:
        raise ValueError("Native compiler emitted an empty dependency rule.")
    return [Path(os.path.abspath(directory / name)) for name in names]


def resolve_assembler_paths(paths: list[Path], known: list[Path], directory: Path) -> tuple[list[Path], dict[str, str]]:
    resolved = []
    aliases = {}
    for path in paths:
        if path.is_file():
            resolved.append(path)
            continue
        # GNU as also reports DWARF .file labels. GCC can supply a basename
        # with a separate directory; that basename is not relative to cwd.
        candidates = []
        if path.is_relative_to(directory):
            suffix = path.relative_to(directory).parts
            candidates = sorted({source for source in known if source.parts[-len(suffix):] == suffix})
        if len(candidates) != 1:
            raise ValueError("Assembler dependency path is missing or ambiguous: " + str(path))
        resolved.append(candidates[0])
        aliases[str(path)] = str(candidates[0])
    return resolved, aliases


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
    preprocess_source = "-E" in arguments and bool(sources)
    compile_source = "-c" in arguments and not preprocess_source and bool(sources)
    if (compile_source or preprocess_source) and len(sources) != 1:
        raise ValueError("Native compiler tracing supports one source per invocation.")
    traces = output / "native-compiler-traces"
    traces.mkdir(exist_ok=True)
    store = output / "native-source-bytes"
    store.mkdir(exist_ok=True)
    if traces.resolve() != traces or store.resolve() != store:
        raise ValueError("Native compiler records and byte copies cannot use symlinked directories.")
    identifier = uuid.uuid4().hex
    dependencies = traces / (identifier + ".d")
    assembler_dependencies = traces / (identifier + ".assembler.d")
    compiler = Path("/usr/bin/cc")
    preprocessed = (compile_source or preprocess_source) and sources[0].suffix != ".s"
    compiler_arguments = []
    for argument in arguments:
        # GCC retains the user-only dependency mode when both -MMD and -MD
        # are passed. Select all headers explicitly before adding our rule.
        if preprocessed and argument in {"-MMD", "--write-user-dependencies"}:
            compiler_arguments.append("-MD")
        elif preprocessed and argument.startswith("-Wp,"):
            parts = argument.split(",")[1:]
            retained = []
            index = 0
            while index < len(parts):
                if parts[index] in {"-MMD", "-MD"}:
                    # A direct preprocessor output path overrides the
                    # driver's -MF. Our complete rule owns its output path.
                    if index + 1 >= len(parts) or not parts[index + 1]:
                        raise ValueError("Preprocessor dependency option requires an output path.")
                    index += 2
                else:
                    retained.append(parts[index])
                    index += 1
            if retained:
                compiler_arguments.append("-Wp," + ",".join(retained))
        else:
            compiler_arguments.append(argument)
    command = [str(compiler), *compiler_arguments]
    if preprocessed:
        # Feeding the assembler through stdin avoids recording a compiler
        # temporary that is deleted before the driver returns.
        command += ["-pipe", "-MD", "-MF", str(dependencies)]
    if compile_source:
        command += ["-Xassembler", "--MD", "-Xassembler", str(assembler_dependencies)]
    result = subprocess.run(command, pass_fds=jobserver_fds())
    paths = list(sources)
    if result.returncode == 0 and preprocessed:
        paths += dependency_paths(dependencies.read_text(), directory)
    aliases = {}
    if result.returncode == 0 and compile_source:
        reported = dependency_paths(assembler_dependencies.read_text(), directory, allow_empty=preprocessed)
        resolved, aliases = resolve_assembler_paths(reported, paths, directory)
        paths += resolved
    hashes = {str(path): preserve_source(path, store) for path in sorted(set(paths))}
    record = {"schemaVersion": 2, "sourceByteDirectory": store.name,
              "compiler": str(compiler), "compilerSha256": digest(compiler), "directory": str(directory),
              "arguments": arguments, "command": command, "exitCode": result.returncode, "compileSource": compile_source,
              "dependencyRuleCaptured": result.returncode == 0 and (compile_source or preprocessed),
              "preprocessorDependencyRuleCaptured": result.returncode == 0 and preprocessed,
              "assemblerDependencyRuleCaptured": result.returncode == 0 and compile_source,
              "assemblerPathAliases": aliases, "sourceFiles": hashes,
              "licenseClearance": False,
              "scope": "Hash-addressed copies of native source and compiler-reported include files, read after the compiler "
                       "returns and preserved before this invocation returns. Copies do not establish which bytes were "
                       "compiled if an input changed during compilation, or which header content was retained. "
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
