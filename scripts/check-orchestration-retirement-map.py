#!/usr/bin/env python3
"""Check the planned MCP retirement inventory against named source declarations."""

import argparse
import collections
import json
from pathlib import Path
import re
import sys


def rust_tokens(source):
    """Read attribute tokens while skipping nested comments and literal bodies."""
    i = 0
    while i < len(source):
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end + 1
            continue
        if source.startswith("/*", i):
            i += 2
            depth = 1
            while depth and i < len(source):
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("unterminated Rust comment")
            continue
        raw = re.match(r'(?:br|cr|r)(\#*)"', source[i:])
        if raw:
            start = i + raw.end()
            end_marker = '"' + raw.group(1)
            end = source.find(end_marker, start)
            if end < 0:
                raise ValueError("unterminated raw Rust literal")
            yield ("string", source[start:end])
            i = end + len(end_marker)
            continue
        if source[i] == '"':
            start = i
            i += 1
            while i < len(source) and source[i] != '"':
                i += 2 if source[i] == "\\" else 1
            if i >= len(source):
                raise ValueError("unterminated Rust string")
            i += 1
            # Tool names use plain identifiers; escapes are not accepted as names.
            yield ("string", source[start + 1:i - 1])
            continue
        char = re.match(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'", source[i:])
        if char:
            i += char.end()
            continue
        ident = re.match(r"[A-Za-z_][A-Za-z_0-9]*", source[i:])
        if ident:
            yield ("token", ident.group())
            i += ident.end()
            continue
        yield ("token", source[i])
        i += 1


def tool_names(source):
    tokens = list(rust_tokens(source))
    prefix = [("token", value) for value in ("#", "[", "tool", "(")]
    for i in range(len(tokens) - 3):
        if tokens[i:i + 4] != prefix:
            continue
        j = i + 4
        depth = 1
        names = []
        while j < len(tokens) and depth:
            token = tokens[j]
            if depth == 1 and token == ("token", "name"):
                if tokens[j + 1:j + 2] != [("token", "=")] or j + 2 >= len(tokens):
                    raise ValueError("tool name must be an explicit string")
                kind, name = tokens[j + 2]
                if kind != "string" or not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", name):
                    raise ValueError("tool name must be a plain identifier string")
                names.append(name)
            if token == ("token", "("):
                depth += 1
            elif token == ("token", ")"):
                depth -= 1
            j += 1
        if depth or len(names) != 1 or tokens[j:j + 1] != [("token", "]")]:
            raise ValueError("tool attribute must have exactly one explicit name")
        yield names[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("baseline", "progress", "target"), default="baseline")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    mapping = json.loads(
        (root / "design/orchestration/bro-execution-retirement-map.json").read_text()
    )
    baseline_rows = mapping["tools"]
    additions = mapping.get("additions", [])
    rows = baseline_rows + additions
    problems = []
    by_name = {row["tool"]: row for row in rows}
    if len(rows) != len(by_name):
        problems.append("duplicate names in retirement map")
    if len(baseline_rows) != mapping["source_declaration_count"]:
        problems.append("map count does not match declared baseline")
    for row in rows:
        group = mapping["groups"].get(row["group"])
        if group is None or any(row[key] != group[key] for key in ("disposition", "wave")):
            problems.append(f"invalid group/disposition/wave for {row['tool']}")
        if row["disposition"] not in {"keep", "slim", "retire"}:
            problems.append(f"invalid disposition for {row['tool']}")

    owners = collections.defaultdict(list)
    for path in (root / "src/tools").rglob("*.rs"):
        for name in tool_names(path.read_text()):
            owners[name].append(str(path.relative_to(root)))
    for name, paths in owners.items():
        if len(paths) != 1:
            problems.append(f"duplicate source declaration: {name}")
        if args.mode == "baseline" and name in by_name and paths != [by_name[name]["source"]]:
            problems.append(f"source owner differs from baseline: {name}")

    current = set(owners)
    baseline = set(by_name)
    retired = {name for name, row in by_name.items() if row["disposition"] == "retire"}
    survivors = baseline - retired
    for name in sorted(current - baseline):
        problems.append(f"unmapped tool: {name}")
    required = {row["tool"] for row in baseline_rows} if args.mode == "baseline" else survivors
    for name in sorted(required - current):
        problems.append(f"missing required tool: {name}")
    if args.mode == "target":
        for name in sorted(current & retired):
            problems.append(f"retired tool still declared: {name}")

    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    print(
        f"PASS ({args.mode}): {len(current)} declarations; "
        f"{len(survivors)} planned survivors; "
        f"{len(retired - current)}/{len(retired)} retirements absent"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
