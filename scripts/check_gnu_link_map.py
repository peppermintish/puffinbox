#!/usr/bin/env python3
"""Inventory known runtime inputs in an experimental LLD server link map."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re


RUNTIME_CRATES = {"std", "core", "alloc", "compiler_builtins", "panic_abort", "panic_unwind",
                  "unwind", "std_detect", "proc_macro", "test"}
SYSTEM_ARCHIVES = {"libc.a", "libc_nonshared.a", "libunwind.a", "libgcc.a", "libgcc_eh.a"}
STARTUP_OBJECTS = {"Scrt1.o", "crt1.o", "rcrt1.o", "crti.o", "crtbegin.o", "crtbeginS.o",
                   "crtbeginT.o", "crtend.o", "crtendS.o", "crtn.o"}
INPUT_ROW = re.compile(r"^\s*([0-9a-f]+)\s+[0-9a-f]+\s+([0-9a-f]+)\s+\d+\s+(.+?):\((.*)\)\s*$")


def inventory(map_text: str, symbols_text: str) -> dict[str, object]:
    lines = map_text.splitlines()
    if not lines or lines[0].split() != ["VMA", "LMA", "Size", "Align", "Out", "In", "Symbol"]:
        raise ValueError("Expected an LLD map with the supported column header.")
    inputs = []
    runtime = []
    for line in lines[1:]:
        match = INPUT_ROW.match(line)
        if not match or not int(match[2], 16):
            continue
        path, section = match[3], match[4]
        if path == "<internal>":
            continue
        inputs.append(path)
        crate = re.search(r"/lib([a-z_]+)-[^/]*[.]rlib\(", path)
        archive = Path(path.split("(", 1)[0]).name
        if (path.startswith("/usr/") or archive in SYSTEM_ARCHIVES or archive in STARTUP_OBJECTS
                or (crate and crate[1] in RUNTIME_CRATES)):
            runtime.append({"address": match[1], "bytes": int(match[2], 16), "input": path, "section": section})
    if not inputs:
        raise ValueError("The map contains no parsed nonempty input sections.")
    unicode = [line for line in symbols_text.splitlines() if "core::unicode::unicode_data::" in line]
    defined = [line for line in unicode if not re.match(r"^\s+U ", line)]
    return {"scope": "Parsed nonempty LLD input sections; system paths, known runtime archives and startup objects, and generated Unicode symbols.",
            "inputObjects": len(set(inputs)), "retainedRuntimeSections": runtime,
            "unicodeImports": len(unicode) - len(defined), "unicodeDefinitions": defined,
            "knownRuntimeInputsAbsent": not runtime and not defined,
            "licenseClearance": False}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--map", type=Path, required=True)
    parser.add_argument("--symbols", type=Path, required=True, help="nm -C output for the exact executable")
    parser.add_argument("--output", type=Path, required=True, help="new inventory file")
    args = parser.parse_args()
    record = inventory(args.map.read_text(), args.symbols.read_text())
    with args.output.open("x") as output:
        json.dump(record, output, indent=2)
        output.write("\n")
    print(f"Runtime inventory: {len(record['retainedRuntimeSections'])} retained sections, "
          f"{len(record['unicodeDefinitions'])} generated Unicode definitions. License clearance remains separate.")
    return 0 if record["knownRuntimeInputsAbsent"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
