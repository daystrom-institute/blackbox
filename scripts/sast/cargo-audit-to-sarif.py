#!/usr/bin/env python3
"""Convert `cargo audit --json` output to SARIF 2.1.0.

Reads cargo-audit JSON from stdin, writes SARIF to stdout.
Maps every advisory (vulnerabilities + warnings) to a SARIF result anchored
on Cargo.lock. Severity:
  - vulnerabilities -> error
  - unsound         -> error
  - unmaintained    -> warning
  - notice          -> note
"""

import json
import sys

LEVEL_BY_KIND = {
    "vulnerability": "error",
    "unsound": "error",
    "unmaintained": "warning",
    "notice": "note",
}


def make_rule(advisory: dict, kind: str) -> dict:
    return {
        "id": advisory["id"],
        "name": advisory["id"],
        "shortDescription": {"text": advisory.get("title", advisory["id"])},
        "fullDescription": {"text": advisory.get("description", "")[:2000]},
        "helpUri": advisory.get("url") or f"https://rustsec.org/advisories/{advisory['id']}",
        "properties": {
            "kind": kind,
            "categories": advisory.get("categories", []),
            "tags": advisory.get("categories", []) + [kind],
        },
    }


def make_result(advisory: dict, package: dict, kind: str) -> dict:
    pkg_name = package.get("name", "unknown")
    pkg_ver = package.get("version", "?")
    return {
        "ruleId": advisory["id"],
        "level": LEVEL_BY_KIND.get(kind, "warning"),
        "message": {
            "text": f"{advisory.get('title', advisory['id'])} ({pkg_name} {pkg_ver}) [{kind}]"
        },
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": {"uri": "Cargo.lock"},
                    "region": {"startLine": 1},
                }
            }
        ],
        "properties": {
            "package": pkg_name,
            "version": pkg_ver,
            "kind": kind,
        },
    }


def main() -> int:
    raw = sys.stdin.read()
    if not raw.strip():
        # Empty input -> empty SARIF run.
        report = {}
    else:
        report = json.loads(raw)

    rules: dict[str, dict] = {}
    results: list[dict] = []

    vulns = (report.get("vulnerabilities") or {}).get("list") or []
    for entry in vulns:
        adv = entry["advisory"]
        pkg = entry.get("package", {})
        rules.setdefault(adv["id"], make_rule(adv, "vulnerability"))
        results.append(make_result(adv, pkg, "vulnerability"))

    warnings = report.get("warnings") or {}
    for kind, entries in warnings.items():
        for entry in entries:
            adv = entry["advisory"]
            pkg = entry.get("package", {})
            rules.setdefault(adv["id"], make_rule(adv, kind))
            results.append(make_result(adv, pkg, kind))

    sarif = {
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "cargo-audit",
                        "informationUri": "https://github.com/rustsec/rustsec/tree/main/cargo-audit",
                        "rules": list(rules.values()),
                    }
                },
                "results": results,
            }
        ],
    }
    json.dump(sarif, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
