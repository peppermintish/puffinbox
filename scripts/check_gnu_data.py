#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Inventory address-backed DWARF variables in loaded read-only ELF data."""

from __future__ import annotations

import argparse
from bisect import bisect_left
from collections import Counter
import hashlib
from itertools import product
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


def constant(attribute):
    """Integer-valued references and expressions are not literal dimensions."""
    if attribute is None or getattr(attribute, "form", None) not in (
            "DW_FORM_data1", "DW_FORM_data2", "DW_FORM_data4", "DW_FORM_data8", "DW_FORM_data16",
            "DW_FORM_udata", "DW_FORM_sdata", "DW_FORM_implicit_const"):
        return None
    return attribute.value if type(attribute.value) is int else None


def array_dimensions(die):
    dimensions = []
    for child in die.iter_children():
        if child.tag != "DW_TAG_subrange_type":
            continue
        attrs = child.attributes
        if "DW_AT_count" in attrs:
            count = constant(attrs["DW_AT_count"])
        elif "DW_AT_upper_bound" in attrs:
            upper = constant(attrs["DW_AT_upper_bound"])
            if upper is None:
                return None
            if "DW_AT_lower_bound" in attrs:
                lower = constant(attrs["DW_AT_lower_bound"])
            else:
                language = die.cu.get_top_DIE().attributes.get("DW_AT_language")
                # Only the zero-based C and Rust defaults used by these builds.
                if language is None or language.value not in (0x01, 0x02, 0x0C, 0x1C, 0x1D):
                    return None
                lower = 0
            count = upper - lower + 1 if upper is not None and lower is not None else None
        else:
            return None
        if count is None or count < 0:
            return None
        dimensions.append(count)
    return dimensions or None


def byte_size(die, seen=None):
    seen = set() if seen is None else seen
    if die.offset in seen:
        return None
    seen.add(die.offset)
    if "DW_AT_byte_size" in die.attributes:
        value = constant(die.attributes["DW_AT_byte_size"])
        return value if isinstance(value, int) and value >= 0 else None
    if die.tag in ("DW_TAG_pointer_type", "DW_TAG_reference_type", "DW_TAG_rvalue_reference_type"):
        return die.cu.header.address_size
    owner = inherited(die, "DW_AT_type")
    if owner is None:
        return None
    size = byte_size(owner.get_DIE_from_attribute("DW_AT_type"), seen)
    if die.tag == "DW_TAG_array_type":
        dimensions = array_dimensions(die)
        if dimensions is None or size is None:
            return None
        for shape in [die, *die.iter_children()]:
            if any(key in shape.attributes for key in ("DW_AT_byte_stride", "DW_AT_bit_stride")):
                return None
        for count in dimensions:
            size *= count
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


def optional_string_layout(die):
    """Verify the observed null-pointer niche; this is not a general enum reader."""
    children = list(die.iter_children())
    parts = [child for child in children if child.tag == "DW_TAG_variant_part"]
    # Rust also places variant type and method declarations here.
    if (die.tag != "DW_TAG_structure_type" or byte_size(die) != 16 or len(parts) != 1
            or any(child.tag not in ("DW_TAG_variant_part", "DW_TAG_structure_type", "DW_TAG_subprogram") for child in children)):
        return False
    part = parts[0]
    reference = part.attributes.get("DW_AT_discr")
    if reference is None or reference.form not in (
            "DW_FORM_ref1", "DW_FORM_ref2", "DW_FORM_ref4", "DW_FORM_ref8", "DW_FORM_ref_udata", "DW_FORM_ref_addr"):
        return False
    children = list(part.iter_children())
    members = [child for child in children if child.tag == "DW_TAG_member"]
    variants = [child for child in children if child.tag == "DW_TAG_variant"]
    if len(children) != 3 or len(members) != 1 or len(variants) != 2:
        return False
    discriminant = part.get_DIE_from_attribute("DW_AT_discr")
    if discriminant is not members[0]:
        return False

    def member_type(member, position):
        if (member.tag != "DW_TAG_member"
                or any(key in member.attributes for key in ("DW_AT_bit_size", "DW_AT_bit_offset", "DW_AT_data_bit_offset"))
                or constant(member.attributes.get("DW_AT_data_member_location")) != position):
            return None
        owner = inherited(member, "DW_AT_type")
        return unwrapped(owner.get_DIE_from_attribute("DW_AT_type")) if owner else None

    scalar = member_type(discriminant, 0)
    encoding = scalar.attributes.get("DW_AT_encoding") if scalar else None
    if (scalar is None or scalar.tag != "DW_TAG_base_type" or byte_size(scalar) != 8
            or encoding is None or encoding.value != 7):
        return False
    seen = set()
    for variant in variants:
        if "DW_AT_discr_list" in variant.attributes:
            return False
        value = variant.attributes.get("DW_AT_discr_value")
        if value is not None and constant(value) != 0:
            return False
        expected = "None" if value is not None else "Some"
        if expected in seen:
            return False
        seen.add(expected)
        children = list(variant.iter_children())
        if len(children) != 1:
            return False
        member = children[0]
        shape = member_type(member, 0)
        name = member.attributes.get("DW_AT_name")
        shape_name = shape.attributes.get("DW_AT_name") if shape else None
        if (name is None or text(name.value) != expected or shape is None
                or shape.tag != "DW_TAG_structure_type" or byte_size(shape) != 16
                or shape_name is None or text(shape_name.value) != expected):
            return False
        children = list(shape.iter_children())
        if any(child.tag not in ("DW_TAG_member", "DW_TAG_template_type_param") for child in children):
            return False
        children = [child for child in children if child.tag == "DW_TAG_member"]
        if expected == "None":
            if children:
                return False
        else:
            if len(children) != 1:
                return False
            element = member_type(children[0], 0)
            name = children[0].attributes.get("DW_AT_name")
            if name is None or text(name.value) != "__0" or element is None or list(string_fields(element)) != [(0, ())]:
                return False
    return seen == {"None", "Some"}


def string_fields(die, offset=0, path=(), seen=None, *, slices=False, optional=False):
    """Recognize bounded observed Rust string layouts; never follow arbitrary pointers."""
    seen = set() if seen is None else set(seen)
    die = unwrapped(die)
    if die is None or die.offset in seen or len(path) > 8:
        return
    seen.add(die.offset)
    if die.tag == "DW_TAG_array_type":
        owner = inherited(die, "DW_AT_type")
        ordering = die.attributes.get("DW_AT_ordering")
        if owner is None or (ordering is not None and constant(ordering) != 0):
            return
        for shape in [die, *die.iter_children()]:
            if any(key in shape.attributes for key in ("DW_AT_byte_stride", "DW_AT_bit_stride",
                                                       "DW_AT_data_location", "DW_AT_allocated",
                                                       "DW_AT_associated", "DW_AT_rank")):
                return
        dimensions = array_dimensions(die)
        if dimensions is None or any(count == 0 for count in dimensions):
            return
        slots = 1
        for count in dimensions:
            slots *= count
            if slots > 4096:
                return
        element = owner.get_DIE_from_attribute("DW_AT_type")
        width = byte_size(element)
        if width is None or width <= 0 or byte_size(die) != slots * width:
            return
        fields = list(string_fields(element, path=path + ("[]",), seen=seen, slices=slices, optional=optional))
        if len(fields) * slots > 4096:
            return
        for slot, indices in enumerate(product(*(range(count) for count in dimensions))):
            # Indices describe zero-based dense storage, not language lower bounds.
            label = "".join(f"[{index}]" for index in indices)
            for position, member_path in fields:
                yield offset + slot * width + position, path + (label,) + member_path[len(path) + 1:]
        return
    if die.tag != "DW_TAG_structure_type":
        return
    members = [child for child in die.iter_children() if child.tag == "DW_TAG_member"]
    name = die.attributes.get("DW_AT_name")
    if optional and name and text(name.value) == "Option<&str>":
        if len(path) <= 6 and optional_string_layout(die):
            yield offset, path
        return
    if not optional and name and text(name.value) == ("&[&str]" if slices else "&str"):
        if slices and len(path) >= 8:
            return
        names = {text(child.attributes["DW_AT_name"].value): child for child in members
                 if "DW_AT_name" in child.attributes}
        if byte_size(die) != 16 or len(members) != 2 or set(names) != {"data_ptr", "length"}:
            return
        types = []
        for field, position in ((names["data_ptr"], 0), (names["length"], 8)):
            if any(key in field.attributes for key in ("DW_AT_bit_size", "DW_AT_bit_offset", "DW_AT_data_bit_offset")):
                return
            location = field.attributes.get("DW_AT_data_member_location")
            owner = inherited(field, "DW_AT_type")
            if constant(location) != position or owner is None:
                return
            types.append(unwrapped(owner.get_DIE_from_attribute("DW_AT_type")))
        pointer, length = types
        if (pointer is None or pointer.tag != "DW_TAG_pointer_type" or byte_size(pointer) != 8
                or "DW_AT_type" not in pointer.attributes):
            return
        element = unwrapped(pointer.get_DIE_from_attribute("DW_AT_type"))
        if slices and (element is None or list(string_fields(element)) != [(0, ())]):
            return
        for scalar, width in (((length, 8),) if slices else ((element, 1), (length, 8))):
            encoding = scalar.attributes.get("DW_AT_encoding") if scalar else None
            if (scalar is None or scalar.tag != "DW_TAG_base_type" or byte_size(scalar) != width
                    or encoding is None or encoding.value != 7):
                return
        yield offset, path
        return
    total = byte_size(die)
    if total is None:
        return
    fields = []
    for child in members:
        location = child.attributes.get("DW_AT_data_member_location")
        owner = inherited(child, "DW_AT_type")
        position = constant(location)
        if position is None or owner is None:
            continue
        field = owner.get_DIE_from_attribute("DW_AT_type")
        size = byte_size(field)
        if size is None or position < 0 or position + size > total:
            continue
        name = child.attributes.get("DW_AT_name")
        fields.extend(string_fields(field, offset + position,
                                    path + (text(name.value) if name else None,), seen, slices=slices, optional=optional))
        if len(fields) > 4096:
            return
    yield from fields


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

    def reference(self, address, root_size, offset, limit=1024 * 1024,
                  bound="one-MiB inspection bound"):
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
        if length > limit:
            raise ValueError("String payload exceeds the " + bound + ".")
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
        return {"fieldAddress": hex(field), "binding": binding, "dataAddress": hex(pointer),
                "length": length}, pointer

    def inspect(self, address, root_size, offset):
        reference, pointer = self.reference(address, root_size, offset)
        if pointer is None:
            return reference, None
        length = reference["length"]
        data, section = self.read(pointer, length)
        if self.overlapping(pointer, length):
            raise ValueError("String payload overlaps a relocation.")
        try:
            data.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ValueError("String payload is not valid UTF-8.") from error
        return {**reference, "section": section, "storedBytesSha256": hashlib.sha256(data).hexdigest(),
                "utf8Validated": True}, (pointer, pointer + length)

    def inspect_slice(self, address, root_size, offset, limit=4096):
        reference, pointer = self.reference(address, root_size, offset, min(limit, 4096),
                                            "bounded string-slice element count")
        count = reference.pop("length")
        result = {**reference, "elementCount": count, "elementByteSize": 16}
        if pointer is None:
            return result, [], None
        if pointer % 8:
            raise ValueError("String-slice data is not aligned to its observed pointer layout.")
        size = count * 16
        raw, section = self.read(pointer, size)
        # Validate the entire slice before returning any coverage or payloads.
        strings = [self.inspect(pointer, size, index * 16) for index in range(count)]
        result.update(section=section, byteSize=size, storedBytesSha256=hashlib.sha256(raw).hexdigest())
        return result, strings, (pointer, pointer + size)

    def inspect_optional(self, address, root_size, offset):
        if not self.supported:
            raise ValueError("Optional-string inspection requires supported ELF64 x86-64 relocations.")
        if offset < 0 or offset + 16 > root_size:
            raise ValueError("Optional-string header is outside its bounded variable.")
        field = address + offset
        raw, _ = self.read(field, 8)
        pointer = int.from_bytes(raw, "little")
        writes = self.overlapping(field, 8)
        if writes:
            entries = self.relocations.get(field, [])
            if writes != [field] or len(entries) != 1 or entries[0][:2] != (8, 0) or entries[0][2] <= 0:
                raise ValueError("Optional-string discriminant has an unsupported or ambiguous relocation.")
            binding = "R_X86_64_RELATIVE"
        else:
            binding = "stored-pointer-word"
            if pointer == 0:
                # None has no active length or string payload to inspect.
                return {"fieldAddress": hex(field), "variant": "None", "discriminantBinding": binding}, None, None
        observation, span = self.inspect(address, root_size, offset)
        return {"fieldAddress": hex(field), "variant": "Some", "discriminantBinding": binding}, observation, span


def read_only_sections(elf):
    """File-backed loaded data, including complete read-only-after-relocation sections."""
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
    return sections


def inventory(elf, stream):
    from elftools.dwarf.dwarf_expr import DWARFExprParser

    if not elf.has_dwarf_info(strict=True):
        raise ValueError("The executable has no DWARF variable data.")
    dwarf = elf.get_dwarf_info(follow_links=False)
    sections = read_only_sections(elf)
    rows, sources, spans, counts = [], {}, {}, Counter()
    strings, rejected_strings, string_spans = [], [], {}
    slices, rejected_slices, slice_spans = [], [], {}
    optional_strings, rejected_optional_strings = [], []
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
                root_fields = 0
                for offset, path in string_fields(owner.get_DIE_from_attribute("DW_AT_type")):
                    counts["typedStringFields"] += 1
                    association = {"rootAddress": hex(address), "rootName": name,
                                   "rootSourceAssociation": source, "memberPath": path}
                    try:
                        observation, span = reader.inspect(address, size, offset)
                        strings.append({**association, **observation})
                        root_fields += 1
                        if span is None:
                            counts["emptyStringFields"] += 1
                        else:
                            counts["readOnlyStringReferences"] += 1
                            string_spans.setdefault(observation["section"], []).append(span)
                    except ValueError as error:
                        counts["unsupportedStringFields"] += 1
                        rejected_strings.append({**association, "reason": str(error)})
                for offset, path in string_fields(owner.get_DIE_from_attribute("DW_AT_type"), slices=True):
                    counts["typedStringSliceFields"] += 1
                    association = {"rootAddress": hex(address), "rootName": name,
                                   "rootSourceAssociation": source, "memberPath": path}
                    try:
                        observation, elements, storage = reader.inspect_slice(address, size, offset, 4096 - root_fields)
                        slices.append({**association, **observation})
                        root_fields += len(elements)
                        if storage is None:
                            counts["emptyStringSliceFields"] += 1
                        else:
                            counts["readOnlyStringSliceReferences"] += 1
                            slice_spans.setdefault(observation["section"], []).append(storage)
                        for index, (element, span) in enumerate(elements):
                            counts["typedStringFields"] += 1
                            strings.append({**association, **element, "memberPath": path + (f"[{index}]",),
                                            "sliceFieldAddress": observation["fieldAddress"]})
                            if span is None:
                                counts["emptyStringFields"] += 1
                            else:
                                counts["readOnlyStringReferences"] += 1
                                string_spans.setdefault(element["section"], []).append(span)
                    except ValueError as error:
                        counts["unsupportedStringSliceFields"] += 1
                        rejected_slices.append({**association, "reason": str(error)})
                for offset, path in string_fields(owner.get_DIE_from_attribute("DW_AT_type"), optional=True):
                    counts["typedOptionalStringFields"] += 1
                    association = {"rootAddress": hex(address), "rootName": name,
                                   "rootSourceAssociation": source, "memberPath": path}
                    try:
                        if root_fields >= 4096:
                            raise ValueError("Optional string exceeds the per-root string-field budget.")
                        observation, element, span = reader.inspect_optional(address, size, offset)
                        optional_strings.append({**association, **observation})
                        root_fields += 1
                        if element is None:
                            counts["absentOptionalStringFields"] += 1
                            continue
                        counts["presentOptionalStringFields"] += 1
                        counts["typedStringFields"] += 1
                        strings.append({**association, **element, "memberPath": path + ("Some", "__0"),
                                        "optionalFieldAddress": observation["fieldAddress"]})
                        if span is None:
                            counts["emptyStringFields"] += 1
                        else:
                            counts["readOnlyStringReferences"] += 1
                            string_spans.setdefault(element["section"], []).append(span)
                    except ValueError as error:
                        counts["unsupportedOptionalStringFields"] += 1
                        rejected_optional_strings.append({**association, "reason": str(error)})
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
        slice_storage = slice_spans.get(section.name, [])
        with_slices = covered_bytes(spans.get(section.name, []) + payloads + slice_storage)
        coverage.append({"section": section.name, "bytes": stop - start,
                         "variableBytes": covered, "remainingBytes": stop - start - covered,
                         "stringPayloadBytes": covered_bytes(payloads), "variableAndStringBytes": combined,
                         "remainingAfterStringInspection": stop - start - combined,
                         "stringSliceStorageBytes": covered_bytes(slice_storage),
                         "variableStringAndSliceBytes": with_slices,
                         "remainingAfterSliceInspection": stop - start - with_slices,
                         "readOnlyAfterRelocation": after_relocation})
    return {"counts": dict(counts), "rows": rows, "sourceFiles": sources, "sectionCoverage": coverage,
            "stringReferences": strings, "unsupportedStringFields": rejected_strings,
            "stringSliceReferences": slices, "unsupportedStringSliceFields": rejected_slices,
            "optionalStringReferences": optional_strings, "unsupportedOptionalStringFields": rejected_optional_strings,
            "knownNonAllowlistedSourceFiles": [name for name in sources if known_non_allowlisted_path(name)],
            "unreviewedSystemHeaderFiles": [name for name in sources if name.startswith("/usr/include/")],
            "licenseClearance": False,
            "scope": "Direct DW_OP_addr/addrx variables in loaded non-executable read-only and GNU RELRO sections. "
                     "Hashes describe ELF file bytes before relocation. Source-less vtable names are counted, not "
                     "attributed or cleared. String references cover bounded UTF-8 payloads reached through observed "
                     "Rust &str structures and fixed dense arrays within source-associated variables, using supported "
                     "x86-64 relocations. Array paths use zero-based storage indices. Each array expansion and "
                     "structure is bounded to 4096 string fields, and paths to eight levels. "
                     "Observed Rust &[&str] fields additionally cover bounded read-only element storage and "
                     "UTF-8 payloads through supported relocations. Each root has a 4096-string-field budget. "
                     "The observed Option<&str> null-pointer niche selects only its active variant; None's "
                     "inactive length is not read. "
                     "Root declarations are associations, not proof of literal origin. Dynamic, strided, oversized "
                     "or unbounded arrays, other variant parts, arbitrary "
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
