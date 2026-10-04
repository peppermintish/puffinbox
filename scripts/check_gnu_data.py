#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Inventory address-backed DWARF variables in loaded read-only ELF data."""

from __future__ import annotations

import argparse
from bisect import bisect_left
from collections import Counter
import hashlib
import json
from pathlib import Path
import re

from check_gnu_source_map import LIBRARY_MARKER, file_path, known_non_allowlisted_path, text


def inherited(die, attribute):
    """Follow declaration references without accepting a cyclic attribution."""
    seen = set()
    while die.offset not in seen:
        seen.add(die.offset)
        if attribute in die.attributes:
            return die
        reference = next((key for key in ("DW_AT_abstract_origin", "DW_AT_specification")
                          if key in die.attributes), None)
        if reference is None:
            return None
        die = die.get_DIE_from_attribute(reference)
    return None


def byte_size(die, seen=None):
    seen = set() if seen is None else seen
    if die.offset in seen:
        return None
    seen.add(die.offset)
    if "DW_AT_byte_size" in die.attributes:
        value = die.attributes["DW_AT_byte_size"].value
        return value if isinstance(value, int) and value >= 0 else None
    if die.tag in ("DW_TAG_pointer_type", "DW_TAG_reference_type", "DW_TAG_rvalue_reference_type"):
        return die.cu.header.address_size
    owner = inherited(die, "DW_AT_type")
    if owner is None:
        return None
    size = byte_size(owner.get_DIE_from_attribute("DW_AT_type"), seen)
    if die.tag == "DW_TAG_array_type":
        dimensions = 0
        for child in die.iter_children():
            if child.tag != "DW_TAG_subrange_type":
                continue
            dimensions += 1
            attrs = child.attributes
            if "DW_AT_count" in attrs:
                count = attrs["DW_AT_count"].value
            elif "DW_AT_upper_bound" in attrs:
                upper = attrs["DW_AT_upper_bound"].value
                if not isinstance(upper, int):
                    return None
                if "DW_AT_lower_bound" in attrs:
                    lower = attrs["DW_AT_lower_bound"].value
                else:
                    language = die.cu.get_top_DIE().attributes.get("DW_AT_language")
                    # Only the zero-based C and Rust defaults used by these builds.
                    if language is None or language.value not in (0x01, 0x02, 0x0C, 0x1C, 0x1D):
                        return None
                    lower = 0
                count = upper - lower + 1 if isinstance(upper, int) and isinstance(lower, int) else None
            else:
                return None
            if not isinstance(count, int) or count < 0 or size is None:
                return None
            size *= count
        if not dimensions:
            return None
    return size


def covered_bytes(intervals):
    total = 0
    end = None
    for start, stop in sorted(intervals):
        if stop < start:
            raise ValueError("Invalid data interval.")
        total += max(0, stop - max(start, end if end is not None else start))
        end = max(stop, end if end is not None else stop)
    return total


def unwrapped(die):
    seen = set()
    while die.tag in ("DW_TAG_typedef", "DW_TAG_const_type", "DW_TAG_volatile_type"):
        if die.offset in seen or "DW_AT_type" not in die.attributes:
            return None
        seen.add(die.offset)
        die = die.get_DIE_from_attribute("DW_AT_type")
    return die


def string_fields(die, offset=0, path=(), seen=None):
    """Recognize the observed Rust &str layout; never follow arbitrary pointers."""
    seen = set() if seen is None else set(seen)
    die = unwrapped(die)
    if die is None or die.offset in seen or len(path) > 8 or die.tag != "DW_TAG_structure_type":
        return
    seen.add(die.offset)
    members = [child for child in die.iter_children() if child.tag == "DW_TAG_member"]
    name = die.attributes.get("DW_AT_name")
    if name and text(name.value) == "&str":
        names = {text(child.attributes["DW_AT_name"].value): child for child in members
                 if "DW_AT_name" in child.attributes}
        if byte_size(die) != 16 or len(members) != 2 or set(names) != {"data_ptr", "length"}:
            return
        types = []
        for field, position in ((names["data_ptr"], 0), (names["length"], 8)):
            location = field.attributes.get("DW_AT_data_member_location")
            owner = inherited(field, "DW_AT_type")
            if location is None or location.value != position or owner is None:
                return
            types.append(unwrapped(owner.get_DIE_from_attribute("DW_AT_type")))
        pointer, length = types
        if (pointer is None or pointer.tag != "DW_TAG_pointer_type" or byte_size(pointer) != 8
                or "DW_AT_type" not in pointer.attributes):
            return
        element = unwrapped(pointer.get_DIE_from_attribute("DW_AT_type"))
        for scalar, width in ((element, 1), (length, 8)):
            encoding = scalar.attributes.get("DW_AT_encoding") if scalar else None
            if (scalar is None or scalar.tag != "DW_TAG_base_type" or byte_size(scalar) != width
                    or encoding is None or encoding.value != 7):
                return
        yield offset, path
        return
    total = byte_size(die)
    if total is None:
        return
    for child in members:
        location = child.attributes.get("DW_AT_data_member_location")
        owner = inherited(child, "DW_AT_type")
        if location is None or not isinstance(location.value, int) or owner is None:
            continue
        field = owner.get_DIE_from_attribute("DW_AT_type")
        size = byte_size(field)
        if size is None or location.value < 0 or location.value + size > total:
            continue
        name = child.attributes.get("DW_AT_name")
        yield from string_fields(field, offset + location.value, path + (text(name.value) if name else None,), seen)


class StringReader:
    """Bounded ELF64 x86-64 file inspection, without loading or executing the ELF."""
    def __init__(self, elf, stream, sections):
        self.stream, self.sections = stream, sections
        self.executable = elf["e_type"] == "ET_EXEC"
        self.supported = elf.elfclass == 64 and elf.little_endian and elf["e_machine"] == "EM_X86_64"
        self.relocations = {}
        for section in elf.iter_sections():
            if not section["sh_flags"] & 2:
                continue
            if section["sh_type"] in ("SHT_REL", "SHT_RELR"):
                # Packed and implicit-addend relocations need their own decoder.
                self.supported = False
            if section["sh_type"] == "SHT_RELA":
                for entry in section.iter_relocations():
                    address = entry["r_offset"]
                    if entry["r_info_type"] not in (1, 6, 7, 8, 16, 17, 18, 37):
                        # COPY, TLSDESC and other write widths are not modeled.
                        self.supported = False
                    self.relocations.setdefault(address, []).append(
                        (entry["r_info_type"], entry["r_info_sym"], entry["r_addend"]))
        self.addresses = sorted(self.relocations)

    def overlapping(self, address, size):
        # These supported x86-64 relocation forms each write one eight-byte word.
        begin = bisect_left(self.addresses, address - 7)
        end = bisect_left(self.addresses, address + size)
        return self.addresses[begin:end]

    def read(self, address, size):
        matches = [(start, section) for start, stop, section, _ in self.sections
                   if start <= address < stop and 0 <= size <= stop - address]
        if len(matches) != 1:
            raise ValueError("String bytes are not wholly inside one loaded read-only section.")
        start, section = matches[0]
        self.stream.seek(section["sh_offset"] + address - start)
        data = self.stream.read(size)
        if len(data) != size:
            raise ValueError("Truncated string bytes.")
        return data, section.name

    def inspect(self, address, root_size, offset):
        if not self.supported:
            raise ValueError("String inspection requires ELF64 little-endian x86-64 and explicit-addend relocations.")
        if offset < 0 or offset + 16 > root_size:
            raise ValueError("String field exceeds its bounded root variable.")
        field = address + offset
        raw, _ = self.read(field, 16)
        pointer, length = int.from_bytes(raw[:8], "little"), int.from_bytes(raw[8:], "little")
        if self.overlapping(field + 8, 8):
            raise ValueError("String length overlaps a relocation.")
        if length == 0:
            return {"fieldAddress": hex(field), "length": 0, "binding": "empty-no-dereference"}, None
        if length > 1024 * 1024:
            raise ValueError("String payload exceeds the one-MiB inspection bound.")
        writes = self.overlapping(field, 8)
        if writes:
            if writes != [field] or self.relocations[field] != [(8, 0, self.relocations[field][0][2])]:
                raise ValueError("String pointer has an unsupported or overlapping relocation.")
            pointer = self.relocations[field][0][2]
            binding = "R_X86_64_RELATIVE"
        elif self.executable:
            binding = "absolute"
        else:
            raise ValueError("Nonempty PIE string pointer lacks a supported relocation.")
        data, section = self.read(pointer, length)
        if self.overlapping(pointer, length):
            raise ValueError("String payload overlaps a relocation.")
        try:
            data.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ValueError("String payload is not valid UTF-8.") from error
        return {"fieldAddress": hex(field), "binding": binding, "dataAddress": hex(pointer),
                "length": length, "section": section, "storedBytesSha256": hashlib.sha256(data).hexdigest(),
                "utf8Validated": True}, (pointer, pointer + length)


def inventory(elf, stream):
    from elftools.dwarf.dwarf_expr import DWARFExprParser

    if not elf.has_dwarf_info(strict=True):
        raise ValueError("The executable has no DWARF variable data.")
    dwarf = elf.get_dwarf_info(follow_links=False)
    segments = list(elf.iter_segments())
    loaded = [(s["p_vaddr"], s["p_vaddr"] + s["p_memsz"]) for s in segments if s["p_type"] == "PT_LOAD"]
    relro = [(s["p_vaddr"], s["p_vaddr"] + s["p_memsz"]) for s in segments if s["p_type"] == "PT_GNU_RELRO"]
    sections = []
    for section in elf.iter_sections():
        start, stop = section["sh_addr"], section["sh_addr"] + section["sh_size"]
        if not section["sh_flags"] & 2 or section["sh_flags"] & 4 or section["sh_type"] == "SHT_NOBITS":
            continue
        if not any(a <= start < stop <= b for a, b in loaded):
            continue
        after_relocation = any(a <= start < stop <= b for a, b in relro)
        if section["sh_flags"] & 1 and not after_relocation:
            continue
        if any(existing[2].name == section.name for existing in sections):
            raise ValueError("Duplicate loaded read-only section name.")
        sections.append((start, stop, section, after_relocation))
    rows, sources, spans, counts = [], {}, {}, Counter()
    strings, rejected_strings, string_spans = [], [], {}
    reader = StringReader(elf, stream, sections)
    for cu in dwarf.iter_CUs():
        counts["compilationUnits"] += 1
        parser = DWARFExprParser(cu.structs)
        for die in cu.iter_DIEs():
            if die.tag != "DW_TAG_variable":
                continue
            counts["variableDeclarations"] += 1
            location = die.attributes.get("DW_AT_location")
            if location is None:
                counts["withoutLocation"] += 1
                continue
            if location.form != "DW_FORM_exprloc":
                counts["unsupportedLocationForm"] += 1
                continue
            operations = parser.parse_expr(location.value)
            if len(operations) != 1 or operations[0].op_name not in ("DW_OP_addr", "DW_OP_addrx"):
                counts["nonDirectAddressExpression"] += 1
                continue
            op = operations[0]
            address = op.args[0] if op.op_name == "DW_OP_addr" else dwarf.get_addr(cu, op.args[0])
            counts["directAddressVariables"] += 1
            matching = [s for s in sections if s[0] <= address < s[1]]
            if not matching:
                counts["outsideReadOnlyDataSections"] += 1
                continue
            if len(matching) != 1:
                raise ValueError("Overlapping read-only ELF data sections.")
            start, stop, section, after_relocation = matching[0]
            declaration = inherited(die, "DW_AT_decl_file")
            source = None
            if declaration is not None:
                top = declaration.cu.get_top_DIE()
                directory = text(top.attributes["DW_AT_comp_dir"].value) if "DW_AT_comp_dir" in top.attributes else ""
                source = file_path(dwarf.line_program_for_CU(declaration.cu),
                                   declaration.attributes["DW_AT_decl_file"].value, directory)
                sources.setdefault(source, {"variables": 0})["variables"] += 1
            else:
                counts["readOnlyVariablesWithoutSource"] += 1
            owner = inherited(die, "DW_AT_name")
            name = text(owner.attributes["DW_AT_name"].value) if owner else None
            vtable_name = source is None and bool(name and name.endswith("::{vtable}"))
            if vtable_name:
                counts["sourceLessVtableNames"] += 1
            size = byte_size(die)
            digest = None
            if isinstance(size, int) and 0 <= size <= stop - address:
                stream.seek(section["sh_offset"] + address - start)
                content = stream.read(size)
                if len(content) != size:
                    raise ValueError("Truncated ELF variable data.")
                digest = hashlib.sha256(content).hexdigest()
                spans.setdefault(section.name, []).append((address, address + size))
                if size == 0:
                    counts["zeroSizedVariables"] += 1
            else:
                counts["readOnlyVariablesWithoutBoundedSize"] += 1
            counts["readOnlyAddressVariables"] += 1
            rows.append({"address": hex(address), "section": section.name,
                         "readOnlyAfterRelocation": after_relocation, "name": name, "source": source,
                         "declLine": declaration.attributes["DW_AT_decl_line"].value
                         if declaration and "DW_AT_decl_line" in declaration.attributes else None,
                         "byteSize": size, "storedBytesSha256": digest, "sourceLessVtableName": vtable_name})
            owner = inherited(die, "DW_AT_type")
            language = cu.get_top_DIE().attributes.get("DW_AT_language")
            if source and digest and owner and language and language.value == 0x1C:
                for offset, path in string_fields(owner.get_DIE_from_attribute("DW_AT_type")):
                    counts["typedStringFields"] += 1
                    association = {"rootAddress": hex(address), "rootName": name,
                                   "rootSourceAssociation": source, "memberPath": path}
                    try:
                        observation, span = reader.inspect(address, size, offset)
                        strings.append({**association, **observation})
                        if span is None:
                            counts["emptyStringFields"] += 1
                        else:
                            counts["readOnlyStringReferences"] += 1
                            string_spans.setdefault(observation["section"], []).append(span)
                    except ValueError as error:
                        counts["unsupportedStringFields"] += 1
                        rejected_strings.append({**association, "reason": str(error)})
        # pyelftools 0.33 retains all parsed DIEs. Bound memory to one completed CU.
        # Later references can be parsed again from their original byte offsets.
        cu._dielist.clear()
        cu._diemap.clear()
    if not counts["compilationUnits"]:
        raise ValueError("The executable has no DWARF compilation units.")
    coverage = []
    for start, stop, section, after_relocation in sections:
        covered = covered_bytes(spans.get(section.name, []))
        payloads = string_spans.get(section.name, [])
        combined = covered_bytes(spans.get(section.name, []) + payloads)
        coverage.append({"section": section.name, "bytes": stop - start,
                         "variableBytes": covered, "remainingBytes": stop - start - covered,
                         "stringPayloadBytes": covered_bytes(payloads), "variableAndStringBytes": combined,
                         "remainingAfterStringInspection": stop - start - combined,
                         "readOnlyAfterRelocation": after_relocation})
    return {"counts": dict(counts), "rows": rows, "sourceFiles": sources, "sectionCoverage": coverage,
            "stringReferences": strings, "unsupportedStringFields": rejected_strings,
            "knownNonAllowlistedSourceFiles": [name for name in sources if known_non_allowlisted_path(name)],
            "unreviewedSystemHeaderFiles": [name for name in sources if name.startswith("/usr/include/")],
            "licenseClearance": False,
            "scope": "Direct DW_OP_addr/addrx variables in loaded non-executable read-only and GNU RELRO sections. "
                     "Hashes describe ELF file bytes before relocation. Source-less vtable names are counted, not "
                     "attributed or cleared. String references cover bounded UTF-8 payloads reached through observed "
                     "Rust &str structures within source-associated variables, using supported x86-64 relocations. "
                     "Root declarations are associations, not proof of literal origin. Arrays, variant parts, arbitrary "
                     "pointer graphs, location lists, indirect expressions, anonymous roots, code and linker-generated "
                     "material remain outside string coverage. Remaining bytes "
                     "include padding and other structures; no exhaustive data or license clearance is claimed."}


def join_sources(result, hashes, dependencies):
    from capture_gnu_sources import mapped_dependencies

    standard = {}
    for name, row in result["sourceFiles"].items():
        if LIBRARY_MARKER in name:
            if name not in hashes or not re.fullmatch(r"[0-9a-f]{64}", hashes[name]):
                raise ValueError("Variable source lacks an exact standard-library build hash: " + name)
            standard[name] = {**row, "sha256": hashes[name]}
    result["standardLibrarySourceFiles"] = standard
    result["dependencySourceFiles"] = mapped_dependencies({"allMappedSources": result["sourceFiles"]}, dependencies)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--source-hashes", type=Path, required=True)
    parser.add_argument("--dependency-sources", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        import elftools
        from elftools.elf.elffile import ELFFile
    except ImportError:
        parser.error("Install the external pyelftools audit tool; it is not a bundled project dependency.")
    if elftools.__version__ != "0.33":
        parser.error("This inspection is pinned to external pyelftools 0.33.")
    try:
        with args.binary.open("rb") as stream:
            result = inventory(ELFFile(stream), stream)
        join_sources(result, json.loads(args.source_hashes.read_text())["sourceFiles"],
                     json.loads(args.dependency_sources.read_text()))
        result.update({"binarySha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                       "sourceHashesSha256": hashlib.sha256(args.source_hashes.read_bytes()).hexdigest(),
                       "dependencySourceHashesSha256": hashlib.sha256(args.dependency_sources.read_bytes()).hexdigest(),
                       "inspectorVersion": elftools.__version__})
        with args.output.open("x") as output:
            json.dump(result, output, indent=2)
            output.write("\n")
    except (OSError, ValueError, KeyError) as error:
        parser.error(str(error))
    print(f"Read-only data inventory: {result['counts'].get('readOnlyAddressVariables', 0)} variables, "
          f"{len(result['sourceFiles'])} exact source files. Complete data and license coverage remain open.")
    return 1 if result["knownNonAllowlistedSourceFiles"] or result["unreviewedSystemHeaderFiles"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
