#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Classify mapped standard-library paths against the exact compiler's notices."""

from __future__ import annotations

import argparse
import hashlib
from html.parser import HTMLParser
import json
from pathlib import Path
import re


NOTICE_NAME = "COPYRIGHT-library.html"
ALLOWLIST = {"mit", "apache-2.0"}
OPERATORS = {"AND", "and", "OR", "or", "WITH", "with"}
TOKEN = re.compile(r"\s+|[()]|(?:DocumentRef-[A-Za-z0-9.-]+:)?[A-Za-z0-9][A-Za-z0-9.-]*\+?")


def expression_allowed(expression: str) -> bool:
    """Require an MIT/Apache choice, respecting WITH, AND, OR and parentheses.

    Unknown identifiers, later-version suffixes and every WITH addition are
    outside this allowlist. This is a policy evaluator, not an SPDX registry.
    Parse every operand, including alternatives after an allowed OR branch.
    """
    tokens = []
    offset = 0
    for match in TOKEN.finditer(expression):
        if match.start() != offset:
            raise ValueError("Unsupported license-expression syntax.")
        offset = match.end()
        if not match.group().isspace():
            tokens.append((match.group(), match.start(), match.end()))
    if offset != len(expression) or not tokens:
        raise ValueError("Empty or unsupported license expression.")
    index = 0

    def peek() -> str | None:
        return tokens[index][0] if index < len(tokens) else None

    def simple() -> bool:
        nonlocal index
        name = peek()
        if name is None or name in OPERATORS or name in {"(", ")"}:
            raise ValueError("Expected a license identifier.")
        index += 1
        allowed = name.lower() in ALLOWLIST
        if peek() in {"WITH", "with"}:
            operator = tokens[index]
            if tokens[index - 1][2] == operator[1]:
                raise ValueError("WITH requires surrounding whitespace.")
            index += 1
            addition = peek()
            if addition is None or addition in OPERATORS or addition in {"(", ")"} or addition.endswith("+"):
                raise ValueError("Expected a license addition identifier.")
            if operator[2] == tokens[index][1]:
                raise ValueError("WITH requires surrounding whitespace.")
            index += 1
            allowed = False
        return allowed

    def primary() -> bool:
        nonlocal index
        if peek() != "(":
            return simple()
        index += 1
        allowed = disjunction()
        if peek() != ")":
            raise ValueError("Unbalanced license-expression parentheses.")
        index += 1
        return allowed

    def conjunction() -> bool:
        nonlocal index
        allowed = primary()
        while peek() in {"AND", "and"}:
            index += 1
            operand = primary()
            allowed = allowed and operand
        return allowed

    def disjunction() -> bool:
        nonlocal index
        allowed = conjunction()
        while peek() in {"OR", "or"}:
            index += 1
            operand = conjunction()
            allowed = allowed or operand
        return allowed

    allowed = disjunction()
    if index != len(tokens):
        raise ValueError("Unexpected trailing license-expression input.")
    return allowed


class InTreeNotices(HTMLParser):
    """Read hierarchical path/license fields without executing notice HTML."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.active = False
        self.started = self.finished = False
        self.stack = []
        self.roots = []
        self.paragraph = None

    def handle_starttag(self, tag, attrs):
        if tag == "h2":
            heading = dict(attrs).get("id")
            if heading == "in-tree-files":
                if self.started:
                    raise ValueError("Duplicate in-tree notice section.")
                self.active = self.started = True
            elif self.active:
                if heading != "out-of-tree-dependencies" or self.stack or self.paragraph is not None:
                    raise ValueError("Unclosed or unsupported in-tree notice section.")
                self.active = False
                self.finished = True
        elif self.active and tag == "div":
            if self.paragraph is not None:
                raise ValueError("Notice path nested inside a paragraph.")
            self.stack.append({"fields": {}, "children": []})
        elif self.active and self.stack and tag == "p":
            if self.paragraph is not None:
                raise ValueError("Nested notice paragraphs.")
            self.paragraph = []

    def handle_data(self, data):
        if self.active and self.paragraph is not None:
            self.paragraph.append(data)
        elif self.active and self.stack and data.strip():
            raise ValueError("Unsupported text outside notice fields.")

    def handle_endtag(self, tag):
        if not self.active:
            return
        if tag == "p" and self.stack:
            if self.paragraph is None:
                raise ValueError("Unmatched notice paragraph.")
            value = " ".join("".join(self.paragraph).split())
            self.paragraph = None
            match = re.fullmatch(r"(File/Directory|License|Copyright|Exceptions):\s*(.*)", value)
            if not match:
                raise ValueError("Unsupported notice field.")
            field, value = match.groups()
            fields = self.stack[-1]["fields"]
            if field in {"File/Directory", "License"} and field in fields:
                raise ValueError("Duplicate notice path or license field.")
            fields[field] = value
        elif tag == "div":
            if not self.stack or self.paragraph is not None:
                raise ValueError("Unmatched or unclosed notice node.")
            node = self.stack.pop()
            if self.stack:
                self.stack[-1]["children"].append(node)
            else:
                self.roots.append(node)

    def rules(self, data: str) -> dict[str, str]:
        self.feed(data)
        self.close()
        if not self.started or not self.finished or self.stack or len(self.roots) != 1:
            raise ValueError("Missing, unclosed or unsupported in-tree notice tree.")
        rules = {}

        def visit(node, parent=None):
            fields = node["fields"]
            if not fields.get("File/Directory") or not fields.get("License"):
                raise ValueError("Notice node requires a path and license.")
            name = fields["File/Directory"]
            if parent is None:
                if name != ".":
                    raise ValueError("The notice tree requires its default root rule.")
                path = "."
            else:
                if "\\" in name or ":" in name or any(
                    not part or part in {".", ".."} or re.search(r"\s|\x00", part) for part in name.split("/")
                ):
                    raise ValueError("Unsafe or unsupported notice path.")
                path = name if parent == "." else parent + "/" + name
            if path in rules:
                raise ValueError("Duplicate hierarchical notice path.")
            # Validate syntax even for currently unmapped exceptions.
            expression_allowed(fields["License"])
            rules[path] = fields["License"]
            for child in node["children"]:
                visit(child, path)

        visit(self.roots[0])
        return rules


def classify(source: dict, hashes: dict, notice: bytes, inventory_sha256: str, hashes_sha256: str) -> dict:
    notice_sha256 = hashlib.sha256(notice).hexdigest()
    if hashes.get("compilerNotices", {}).get(NOTICE_NAME) != notice_sha256:
        raise ValueError("The notice does not match the exact compiler's recorded notice hash.")
    if source.get("sourceHashesSha256") != hashes_sha256:
        raise ValueError("The inventory does not match the exact source-hash record.")
    rules = InTreeNotices().rules(notice.decode("utf-8", errors="strict"))
    sysroot = hashes.get("sysroot", "")
    if not sysroot.startswith("/") or any(part in {".", ".."} for part in sysroot.split("/")):
        raise ValueError("Missing or invalid compiler sysroot.")
    prefix = sysroot.rstrip("/") + "/lib/rustlib/src/rust/library/"
    standard = source.get("standardLibrarySourceFiles")
    if not isinstance(standard, dict) or not standard:
        raise ValueError("No mapped standard-library files to classify.")
    classified = {}
    for name, record in sorted(standard.items()):
        if not name.startswith(prefix):
            raise ValueError("Mapped source is outside the recorded compiler sysroot.")
        suffix = name[len(prefix):]
        if "\\" in suffix or any(not part or part in {".", ".."} for part in suffix.split("/")):
            raise ValueError("Unsafe mapped source path.")
        digest = record.get("sha256", "")
        if not re.fullmatch(r"[0-9a-f]{64}", digest) or hashes.get("sourceFiles", {}).get(name) != digest:
            raise ValueError("Mapped source does not match its exact build hash.")
        relative = "library/" + suffix
        if suffix.startswith("vendor/"):
            # Vendored crates are covered by package/version-specific notices,
            # not the in-tree default. Keep them open until that join is reviewed.
            rule = expression = None
            allowed = False
            status = "unreviewed-vendored-dependency"
        else:
            matches = [path for path in rules if path == "." or relative == path or relative.startswith(path + "/")]
            rule = max(matches, key=len)
            expression = rules[rule]
            allowed = expression_allowed(expression)
            status = "declared-license-expression"
        classified[name] = {"sourceSha256": digest, "applicableNoticePath": rule,
                            "licenseExpression": expression, "noticeWithinAllowlist": allowed, "reviewStatus": status}
    return {
        "noticeSha256": notice_sha256, "inventorySha256": inventory_sha256, "sourceHashesSha256": hashes_sha256,
        "binarySha256": source["binarySha256"], "inTreeNoticeRules": rules,
        "mappedStandardLibraryFiles": classified,
        "outsideNoticeAllowlist": {name: value for name, value in classified.items() if not value["noticeWithinAllowlist"]},
        "licenseClearance": False,
        "scope": "Declared hierarchical notices for exact mapped standard-library source paths. Individual-file "
                 "exceptions, vendored package notices, anonymous constants, unmapped instructions, assembler, "
                 "generated code, included headers, other dependencies and external-runtime distribution remain open. "
                 "Passing this check does not establish complete source provenance or release license clearance.",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", required=True, type=Path)
    parser.add_argument("--source-hashes", required=True, type=Path)
    parser.add_argument("--notice", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path, help="new classification record")
    args = parser.parse_args()
    try:
        inventory = args.inventory.read_bytes()
        hashes = args.source_hashes.read_bytes()
        result = classify(json.loads(inventory), json.loads(hashes), args.notice.read_bytes(),
                          hashlib.sha256(inventory).hexdigest(), hashlib.sha256(hashes).hexdigest())
        with args.output.open("x") as output:
            json.dump(result, output, indent=2)
            output.write("\n")
    except (OSError, ValueError, KeyError, TypeError, AttributeError, RecursionError) as error:
        parser.error(str(error))
    print(f"Compiler notice classification: {len(result['mappedStandardLibraryFiles'])} exact mapped files; "
          f"{len(result['outsideNoticeAllowlist'])} outside the notice allowlist or unreviewed. License clearance remains open.")
    return 1 if result["outsideNoticeAllowlist"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
