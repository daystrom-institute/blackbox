#!/usr/bin/env python3
"""Extract coarse discovery question-shapes from a phase document.

The extractor is intentionally mechanical and conservative. If the document
declares a frontmatter `question_shapes:` list, that wins. Otherwise it emits a
small standard discovery set from the document title and headings.
"""

import argparse
import json
import pathlib
import re
import sys


VALID_SHAPES = {
    "where",
    "what",
    "why",
    "who_when",
    "trace",
    "impact",
    "currentness",
    "generic",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--path", default="")
    parser.add_argument("--text", default="")
    return parser.parse_args()


def read_doc(path: str, text: str) -> str:
    if text and text.strip():
        return text
    if not path or not path.strip():
        raise SystemExit("extract-question-shapes requires --path or --text")
    return pathlib.Path(path).read_text(encoding="utf-8")


def strip_quotes(value: str) -> str:
    value = value.strip()
    if (value.startswith('"') and value.endswith('"')) or (
        value.startswith("'") and value.endswith("'")
    ):
        return value[1:-1]
    return value


def frontmatter_block(text: str) -> list[str]:
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return []
    for idx in range(1, len(lines)):
        if lines[idx].strip() == "---":
            return lines[1:idx]
    return []


def parse_frontmatter_shapes(text: str) -> list[dict]:
    lines = frontmatter_block(text)
    if not lines:
        return []
    in_shapes = False
    current: dict[str, object] | None = None
    shapes: list[dict] = []
    for raw in lines:
        line = raw.rstrip()
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped == "question_shapes:":
            in_shapes = True
            continue
        if (
            in_shapes
            and raw == stripped
            and re.match(r"^[A-Za-z0-9_-]+:", stripped)
            and not stripped.startswith("-")
        ):
            break
        if not in_shapes:
            continue
        if stripped.startswith("- "):
            if current:
                shapes.append(current)
            current = {"known_evidence": []}
            rest = stripped[2:].strip()
            if rest:
                if ":" in rest:
                    key, value = rest.split(":", 1)
                    current[key.strip()] = strip_quotes(value)
                else:
                    current["query"] = strip_quotes(rest)
            continue
        if current is not None and ":" in stripped:
            key, value = stripped.split(":", 1)
            current[key.strip()] = strip_quotes(value)
    if current:
        shapes.append(current)
    return normalize_shapes(shapes)


def doc_title(text: str) -> str:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            return stripped.lstrip("#").strip()
    return "phase document"


def headings(text: str) -> list[str]:
    found = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            title = stripped.lstrip("#").strip()
            if title:
                found.append(title)
    return found[:8]


def fallback_shapes(text: str) -> list[dict]:
    title = doc_title(text)
    heading_text = "; ".join(headings(text))
    scope = heading_text or title
    return normalize_shapes(
        [
            {
                "question_shape": "where",
                "query": f"Find the files, symbols, docs, and existing artifacts needed for this phase: {title}",
                "scope_hint": scope,
                "known_evidence": [],
            },
            {
                "question_shape": "what",
                "query": f"Identify the acceptance criteria and implementation surface implied by this phase: {title}",
                "scope_hint": scope,
                "known_evidence": [],
            },
            {
                "question_shape": "impact",
                "query": f"Find likely integration points, tests, workflow artifacts, and downstream blast radius for this phase: {title}",
                "scope_hint": scope,
                "known_evidence": [],
            },
        ]
    )


def normalize_shapes(raw_shapes: list[dict]) -> list[dict]:
    out = []
    for idx, raw in enumerate(raw_shapes):
        shape = str(raw.get("question_shape") or raw.get("shape") or "generic").strip().lower()
        if shape not in VALID_SHAPES:
            shape = "generic"
        query = str(raw.get("query") or raw.get("question") or "").strip()
        if not query:
            continue
        known = raw.get("known_evidence") or []
        if isinstance(known, str):
            known = [known] if known.strip() else []
        out.append(
            {
                "question_shape": shape,
                "query": query,
                "scope_hint": str(raw.get("scope_hint") or raw.get("scope") or "").strip(),
                "known_evidence": known,
            }
        )
    if not out:
        raise SystemExit("no question shapes extracted")
    return out


def main() -> int:
    args = parse_args()
    text = read_doc(args.path, args.text)
    shapes = parse_frontmatter_shapes(text) or fallback_shapes(text)
    json.dump(shapes, sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
