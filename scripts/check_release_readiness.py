#!/usr/bin/env python3
"""Refuse release packaging while documented release gates remain open."""

import json
from pathlib import Path


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    record = json.loads((root / "docs/release-gates.json").read_text(encoding="utf-8"))
    gates = record.get("gates", {})
    if not gates:
        raise SystemExit("Release gate record is empty.")
    pending = {name: gate for name, gate in gates.items() if gate.get("passed") is not True}
    if record.get("ready") is not True or pending:
        print("Release packaging is blocked:")
        for name, gate in pending.items():
            print(f"- {name}: {gate.get('detail', 'No evidence recorded.')}")
        return 1
    print("Documented release gates passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
