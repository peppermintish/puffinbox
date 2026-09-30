#!/usr/bin/env python3
"""Compare declared Axum method/path pairs to the public Jellyfin 12.0 schema."""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SPEC_URL = "https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json"
EXPECTED_VERSION = "12.0.0"
EXPECTED_PATHS = 294
EXPECTED_OPERATIONS = 364
OPERATION_KEYS = {"get", "post", "put", "delete", "patch", "head", "options", "trace"}
ROUTE_START = re.compile(r"\.route\s*\(\s*([\"'])(.*?)\1\s*,", re.DOTALL)
METHOD_CALL = re.compile(r"\b(get|post|put|delete|patch|head|options|trace)\s*\(")
TEST_CONFIGURATION = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
RAW_RUST_STRING_START = re.compile(r"(?:b)?r(#{0,255})\"")


def balanced_call_end(source: str, opening_paren: int) -> int:
    depth = 0
    quote: str | None = None
    escaped = False
    for position in range(opening_paren, len(source)):
        char = source[position]
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {"\"", "'"}:
            quote = char
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return position + 1
    raise ValueError("unbalanced route declaration parentheses")


def strip_test_only_items(source: str) -> str:
    """Mask items guarded by #[cfg(test)] so unit-test routes are not counted."""
    spans: list[tuple[int, int]] = []
    for match in TEST_CONFIGURATION.finditer(source):
        opening_brace = source.find("{", match.end())
        semicolon = source.find(";", match.end())
        if opening_brace < 0 or (semicolon >= 0 and semicolon < opening_brace):
            continue
        depth = 0
        quote: str | None = None
        escaped = False
        line_comment = False
        block_comment = 0
        end = opening_brace
        while end < len(source):
            char = source[end]
            following = source[end + 1] if end + 1 < len(source) else ""
            if line_comment:
                if char == "\n":
                    line_comment = False
            elif block_comment:
                if char == "/" and following == "*":
                    block_comment += 1
                    end += 1
                elif char == "*" and following == "/":
                    block_comment -= 1
                    end += 1
            elif quote:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == quote:
                    quote = None
            elif char == "/" and following == "/":
                line_comment = True
                end += 1
            elif char == "/" and following == "*":
                block_comment = 1
                end += 1
            elif (raw_string := RAW_RUST_STRING_START.match(source, end)) is not None:
                terminator = '"' + ("#" * len(raw_string.group(1)))
                close = source.find(terminator, raw_string.end())
                if close < 0:
                    raise ValueError("unterminated raw string in #[cfg(test)] item")
                end = close + len(terminator)
                continue
            elif char in {"\"", "'"}:
                # Rust lifetimes are not quoted strings; only treat a single
                # quote as a character literal when a closing quote follows.
                if char == "\"" or "'" in source[end + 1 : end + 5]:
                    quote = char
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    spans.append((match.start(), end + 1))
                    break
            end += 1
        else:
            raise ValueError("unbalanced #[cfg(test)] item")

    if not spans:
        return source
    masked = list(source)
    for start, end in spans:
        masked[start:end] = " " * (end - start)
    return "".join(masked)


def normalize_path(path: str) -> str:
    return re.sub(r"\{[^{}]+\}", "{}", path)


def source_routes() -> list[dict[str, object]]:
    paths = sorted((ROOT / "src").rglob("*.rs"))
    rows: list[dict[str, object]] = []
    for source_path in paths:
        if not source_path.is_file():
            continue
        content = strip_test_only_items(source_path.read_text(encoding="utf-8"))
        for match in ROUTE_START.finditer(content):
            route_path = match.group(2)
            opening_paren = content.find("(", match.start())
            end = balanced_call_end(content, opening_paren)
            declaration = content[opening_paren:end]
            methods = sorted(set(METHOD_CALL.findall(declaration)))
            if not methods:
                continue
            rows.append({
                "source": str(source_path.relative_to(ROOT)).replace("\\", "/"),
                "path": route_path,
                "normalized": normalize_path(route_path),
                "methods": methods,
            })
    return rows


def fetch_spec() -> dict:
    request = urllib.request.Request(SPEC_URL, headers={"Accept": "application/json", "User-Agent": "Puffinbox API route comparison"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def operation_index(spec: dict) -> tuple[set[tuple[str, str]], int]:
    paths = spec.get("paths")
    if not isinstance(paths, dict):
        raise ValueError("OpenAPI document has no paths object")
    index: set[tuple[str, str]] = set()
    count = 0
    for path, operations in paths.items():
        if not isinstance(operations, dict):
            continue
        for method in operations:
            if method.lower() in OPERATION_KEYS:
                index.add((method.upper(), normalize_path(path)))
                count += 1
    return index, count


def make_report(spec: dict, routes: list[dict[str, object]], output_path: Path) -> tuple[str, int, int]:
    info = spec.get("info", {})
    version = str(info.get("version", "unknown"))
    expected, operation_count = operation_index(spec)
    declarations: dict[tuple[str, str], list[tuple[str, str]]] = defaultdict(list)
    for route in routes:
        for method in route["methods"]:
            declarations[(str(method).upper(), str(route["normalized"]))].append((str(route["source"]), str(route["path"])))
    matched = sorted(expected.intersection(declarations))
    declaration_only = sorted(set(declarations) - expected)
    path_count = len(spec.get("paths", {}))
    unique_declared = len(declarations)
    now = datetime.now(timezone.utc).isoformat()
    lines = [
        "# Generated route declaration comparison",
        "",
        f"Generated at {now} from [{SPEC_URL}]({SPEC_URL}).",
        "",
        f"- OpenAPI version: `{version}`",
        f"- Schema paths: `{path_count}`",
        f"- Schema operations: `{operation_count}`",
        f"- Unique declared server method/path pairs: `{unique_declared}`",
        f"- Declared pairs matching a schema method/path: `{len(matched)}`",
        f"- Declared pairs outside the schema: `{len(declaration_only)}`",
        "",
        "This is a source declaration comparison only. It does not establish that a matching route starts, authenticates correctly, returns the required shape, enforces policy, or behaves like Jellyfin.",
        "",
        "## Declared route pairs",
        "",
        "| Declaration | Source | Schema path/method match |",
        "| --- | --- | --- |",
    ]
    for route in routes:
        for method in route["methods"]:
            key = (str(method).upper(), str(route["normalized"]))
            if key in expected:
                mapping = "match"
            elif str(route["normalized"]) == "/Users/Me/MediaAccessToken":
                mapping = "Puffinbox extension / outside target schema"
            else:
                mapping = "custom / not in target schema"
            lines.append(f"| `{str(method).upper()} {route['path']}` | `{route['source']}` | {mapping} |")
    lines.extend([
        "",
        "The matched-count denominator is the full target schema's operation count, not this project's declarations. Unsupported operations are not implied to be implemented.",
        "",
    ])
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text("\n".join(lines), encoding="utf-8")
    return version, path_count, operation_count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="require the pinned version and operation counts")
    parser.add_argument("--output", type=Path, default=ROOT / "docs" / "generated-api-route-coverage.md")
    args = parser.parse_args()
    try:
        spec = fetch_spec()
        routes = source_routes()
        if not routes:
            raise ValueError("no Axum .route(...) declarations were found")
        version, path_count, operation_count = make_report(spec, routes, args.output)
    except Exception as error:
        print(f"API route comparison failed: {error}", file=sys.stderr)
        return 1
    if args.check and (version != EXPECTED_VERSION or path_count != EXPECTED_PATHS or operation_count != EXPECTED_OPERATIONS):
        print(f"Expected Jellyfin API {EXPECTED_VERSION} with {EXPECTED_PATHS} paths and {EXPECTED_OPERATIONS} operations; received {version}, {path_count}, {operation_count}.", file=sys.stderr)
        return 1
    print(f"Compared {len(routes)} route declarations with Jellyfin API {version} ({path_count} paths, {operation_count} operations).")
    print("The generated report is route-declaration evidence only; it does not test semantics.")
    print(args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
