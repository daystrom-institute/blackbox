#!/usr/bin/env python3
import argparse
import json
import sys


def load(name, raw):
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{name} invalid JSON: {exc}") from exc


def ref_size_index(payload):
    sizes = {}
    for item in payload.get("per_ref") or [] if isinstance(payload, dict) else []:
        if isinstance(item, dict) and isinstance(item.get("ref"), str) and isinstance(item.get("bytes"), int):
            sizes[item["ref"]] = item["bytes"]
    return sizes


def evidence_unresolved_refs(evidence):
    degraded = evidence.get("degraded") if isinstance(evidence, dict) else None
    out = []
    for item in degraded.get("unresolved_refs") or [] if isinstance(degraded, dict) else []:
        ref = item.get("ref") or item.get("entity_ref") if isinstance(item, dict) else item
        if isinstance(ref, str) and ref:
            out.append(ref)
    return out


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--dag", required=True)
    p.add_argument("--ref-size", required=True)
    p.add_argument("--evidence", default="{}")
    args = p.parse_args()

    dag = load("dag", args.dag)
    ref_size = load("ref-size", args.ref_size)
    evidence = load("evidence", args.evidence)
    sizes = ref_size_index(ref_size)

    if isinstance(dag, dict):
        for unit in dag.get("sub_units") or []:
            if not isinstance(unit, dict):
                continue
            refs = unit.get("refs") or []
            if all(isinstance(ref, str) and ref in sizes for ref in refs):
                unit["bytes"] = sum(sizes[ref] for ref in refs)

        contract = dag.get("recompose_contract")
        if isinstance(contract, dict):
            allowed = set(evidence_unresolved_refs(evidence))
            kept = []
            for item in contract.get("degraded_refs") or []:
                if isinstance(item, dict) and item.get("ref") in allowed:
                    kept.append(item)
            contract["degraded_refs"] = kept

    json.dump(dag, sys.stdout, separators=(",", ":"))
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
