#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Inventory mapped source locations in an experimental GNU executable."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import posixpath


LIBRARY_MARKER = "/lib/rustlib/src/rust/library/"


def text(value: bytes | str) -> str:
    return value.decode("utf-8", errors="strict") if isinstance(value, bytes) else value


def file_path(program, index: int, compilation_directory: str) -> str:
    version = program.header.version
    if version not in (4, 5):
        raise ValueError(f"Unsupported DWARF line-table version: {version}")
    delta = 1 if version < 5 else 0
    files = program["file_entry"]
    if not 0 <= index - delta < len(files):
        raise ValueError("Invalid DWARF source-file index.")
    entry = files[index - delta]
    name = text(entry.name)
    if name.startswith("/"):
        return posixpath.normpath(name)
    directories = program["include_directory"]
    if version < 5 and entry.dir_index == 0:
        directory = compilation_directory
    else:
        if not 0 <= entry.dir_index - delta < len(directories):
            raise ValueError("Invalid DWARF source-directory index.")
        directory = text(directories[entry.dir_index - delta])
        if not directory.startswith("/"):
            directory = posixpath.join(compilation_directory, directory)
    path = posixpath.normpath(posixpath.join(directory, name))
    if not path.startswith("/"):
        raise ValueError("Cannot resolve a source path without an absolute compilation directory.")
    return path


def known_non_allowlisted_path(path: str) -> bool:
    if LIBRARY_MARKER not in path:
        return False
    relative = path.split(LIBRARY_MARKER, 1)[1]
    return (
        (relative.startswith("core/src/unicode/") and relative != "core/src/unicode/mod.rs")
        or relative == "std/src/sys/sync/mutex/fuchsia.rs"
        or "compiler_builtins" in relative.split("/")
        or "compiler-builtins" in relative.split("/")
        or "libunwind" in relative.split("/")
    )


def inventory(elf, source_hashes: dict[str, str]) -> dict[str, object]:
    if not elf.has_dwarf_info():
        raise ValueError("The executable has no DWARF source-location data.")
    dwarf = elf.get_dwarf_info()
    executable_ranges = [
        (segment["p_vaddr"], segment["p_vaddr"] + segment["p_memsz"])
        for segment in elf.iter_segments()
        if segment["p_type"] == "PT_LOAD" and segment["p_flags"] & 1
    ]
    sources = {}
    units = mapped = unmapped = discarded = 0
    for unit in dwarf.iter_CUs():
        units += 1
        attributes = unit.get_top_DIE().attributes
        compilation_directory = text(attributes["DW_AT_comp_dir"].value) if "DW_AT_comp_dir" in attributes else ""
        program = dwarf.line_program_for_CU(unit)
        if program is None:
            continue
        previous = None
        for entry in program.get_entries():
            state = entry.state
            if state is None:
                continue
            if previous is not None and previous.address < state.address:
                if previous.address == 0 or not any(
                    start <= previous.address < state.address <= end for start, end in executable_ranges
                ):
                    discarded += 1
                elif not previous.line:
                    unmapped += 1
                else:
                    name = file_path(program, previous.file, compilation_directory)
                    source = sources.setdefault(name, {
                        "mappedIntervals": 0, "intervalBytes": 0,
                        "minLine": previous.line, "maxLine": previous.line,
                    })
                    source["mappedIntervals"] += 1
                    source["intervalBytes"] += state.address - previous.address
                    source["minLine"] = min(source["minLine"], previous.line)
                    source["maxLine"] = max(source["maxLine"], previous.line)
                    mapped += 1
            previous = None if state.end_sequence else state
        # Avoid retaining decoded line programs for thousands of compilation
        # units. Older/newer inspector versions may not expose this cache.
        cache = getattr(dwarf, "_linetable_cache", None)
        if cache is not None:
            cache.clear()
    if not mapped or not units:
        raise ValueError("No mapped instruction intervals were found in loaded executable ranges.")
    standard = {name: dict(value, sha256=source_hashes.get(name))
                for name, value in sources.items() if LIBRARY_MARKER in name}
    if not standard:
        raise ValueError("No standard-library source locations were resolved.")
    missing = [name for name, value in standard.items() if not value["sha256"]]
    if missing:
        raise ValueError("Mapped standard-library sources are missing exact build hashes: " + ", ".join(missing))
    blocked = {name: value for name, value in standard.items() if known_non_allowlisted_path(name)}
    headers = {name: value for name, value in sources.items() if name.startswith("/usr/include/")}
    return {
        "compilationUnits": units, "mappedSourceFiles": len(sources), "mappedIntervals": mapped,
        "lineZeroIntervals": unmapped, "discardedOrInvalidAddressIntervals": discarded,
        "standardLibrarySourceFiles": standard, "knownNonAllowlistedMappedFiles": blocked,
        "unreviewedSystemHeaderFiles": headers,
        "allMappedSources": sources, "licenseClearance": False,
        "scope": "DWARF v4/v5 line intervals within loaded executable ranges. Line mappings do not establish provenance "
                 "for anonymous constants, unmapped instructions, assembler, generated code, or every included header. "
                 "Interval bytes may overlap and do not measure total coverage. Known path checks are an inventory, "
                 "not an exhaustive source-license classifier. Mapped system-header instructions require "
                 "exact review or exclusion before this inventory can pass.",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--source-hashes", required=True, type=Path, help="source hash record from the instrumented builder")
    parser.add_argument("--output", required=True, type=Path, help="new inventory file")
    args = parser.parse_args()
    try:
        import elftools
        from elftools.elf.elffile import ELFFile
    except ImportError:
        parser.error("Install the external pyelftools audit tool; it is not a bundled project dependency.")
    try:
        with args.binary.open("rb") as stream:
            hashes = json.loads(args.source_hashes.read_text())["sourceFiles"]
            result = inventory(ELFFile(stream), hashes)
        result["binarySha256"] = hashlib.sha256(args.binary.read_bytes()).hexdigest()
        result["sourceHashesSha256"] = hashlib.sha256(args.source_hashes.read_bytes()).hexdigest()
        result["inspectorVersion"] = elftools.__version__
        with args.output.open("x") as output:
            json.dump(result, output, indent=2)
            output.write("\n")
    except (OSError, ValueError, KeyError) as error:
        parser.error(str(error))
    print(f"Source inventory: {len(result['standardLibrarySourceFiles'])} exact standard-library source files; "
          f"{len(result['knownNonAllowlistedMappedFiles'])} known non-allowlisted paths; "
          f"{len(result['unreviewedSystemHeaderFiles'])} unreviewed system headers. License clearance remains open.")
    return 1 if result["knownNonAllowlistedMappedFiles"] or result["unreviewedSystemHeaderFiles"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
