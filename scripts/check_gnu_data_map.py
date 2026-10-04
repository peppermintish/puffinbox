#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Associate loaded read-only data ranges with LLD input-section rows, without license clearance."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re

from check_gnu_data import covered_bytes, read_only_sections


MAP_ROW = re.compile(r"^\s*([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+(\d+) (.*)$")
INPUT = re.compile(r"^ {8}(\S.*?):\((.*)\)$")


def inventory(map_text, sections):
    lines = map_text.splitlines()
    if not lines or lines[0].split() != ["VMA", "LMA", "Size", "Align", "Out", "In", "Symbol"]:
        raise ValueError("Expected the supported LLD map column header.")
    wanted = {section.name: (start, stop, protected) for start, stop, section, protected in sections}
    if not wanted or len(wanted) != len(sections):
        raise ValueError("Expected unique loaded read-only sections.")
    current, seen, rows = None, set(), []
    for line in lines[1:]:
        parsed = MAP_ROW.fullmatch(line)
        if parsed is None:
            continue
        address, size, tail = int(parsed[1], 16), int(parsed[3], 16), parsed[5]
        # Preserve column indentation: a .local symbol is not an output section.
        if tail and not tail[0].isspace():
            current = tail if tail in wanted else None
            if current is not None:
                if current in seen:
                    raise ValueError("Duplicate loaded output-section header.")
                seen.add(current)
                start, stop, _ = wanted[current]
                if (address, address + size) != (start, stop):
                    raise ValueError("Map output-section bounds differ from the ELF.")
            continue
        if current is None:
            continue
        match = INPUT.fullmatch(tail)
        if match is None or size == 0:
            continue
        start, stop, _ = wanted[current]
        if not start <= address < address + size <= stop:
            raise ValueError("Input-section range escapes its loaded output section.")
        rows.append({"outputSection": current, "address": hex(address), "bytes": size,
                     "input": match[1], "inputSection": match[2], "linkerInternal": match[1] == "<internal>"})
    if seen != wanted.keys():
        raise ValueError("Map is missing loaded read-only output-section headers.")
    if not rows:
        raise ValueError("Map has no supported nonempty loaded input-section rows.")
    coverage = []
    for name, (start, stop, protected) in wanted.items():
        selected = [row for row in rows if row["outputSection"] == name]
        spans = [(int(row["address"], 16), int(row["address"], 16) + row["bytes"]) for row in selected]
        named = [span for row, span in zip(selected, spans) if not row["linkerInternal"]]
        internal = [span for row, span in zip(selected, spans) if row["linkerInternal"]]
        total = covered_bytes(spans)
        coverage.append({"section": name, "bytes": stop - start, "inputRows": len(selected),
                         "mappedInputRangeUnionBytes": total, "namedInputRangeUnionBytes": covered_bytes(named),
                         "internalInputRangeUnionBytes": covered_bytes(internal),
                         "outsideParsedInputRows": stop - start - total, "readOnlyAfterRelocation": protected})
    return {"rows": rows, "sectionCoverage": coverage, "licenseClearance": False,
            "scope": "Parsed input rows within ELF-verified loaded read-only output sections. Debug offsets and "
                     "symbol rows are excluded. Range unions handle aliases and overlap. Named input objects are "
                     "associations, not proof of literal origin or retained/inlined source licensing. Linker-internal "
                     "merged contributions and gaps need separate review; no exhaustive origin or license clearance is claimed."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--map", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="new inventory file")
    args = parser.parse_args()
    try:
        import elftools
        from elftools.elf.elffile import ELFFile
    except ImportError:
        parser.error("Install the external pyelftools audit tool; it is not a bundled project dependency.")
    if elftools.__version__ != "0.33":
        parser.error("This inspection is pinned to external pyelftools 0.33.")
    with args.binary.open("rb") as stream:
        binary_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    map_bytes = args.map.read_bytes()
    with args.binary.open("rb") as stream:
        elf = ELFFile(stream)
        if elf.elfclass != 64 or not elf.little_endian or elf["e_machine"] != "EM_X86_64":
            raise ValueError("This inspection requires ELF64 little-endian x86-64.")
        record = inventory(map_bytes.decode("utf-8"), read_only_sections(elf))
    with args.binary.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != binary_hash:
            raise ValueError("Binary changed during inspection.")
    record.update(binarySha256=binary_hash, mapSha256=hashlib.sha256(map_bytes).hexdigest(), inspectorVersion=elftools.__version__)
    with args.output.open("x") as output:
        json.dump(record, output, indent=2)
        output.write("\n")
    print(f"Read-only map associations: {len(record['rows'])} input rows; license clearance remains separate.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
