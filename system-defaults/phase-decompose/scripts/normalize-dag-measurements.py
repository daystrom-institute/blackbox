#!/usr/bin/env python3
"""Mechanically normalize a facilitator-emitted DAG before lint.

The facilitator is an LLM emitting a large strict-JSON DAG in one pass, so it
reliably slips on a few *deterministic* details that do not need a model to fix:

1. sub_unit.bytes that disagree with the measured ref-size sum.
2. A ref transcribed wrong (e.g. a spliced 40-char commit SHA) that no longer
   resolves — but whose intended canonical ref is unambiguous in the evidence
   bundle.
3. recompose_contract.degraded_refs left empty / wrong when the evidence bundle
   already says which refs were unresolved at discovery.
4. cross_subunit_tests entries keyed `name` instead of the required `test_id`.

Each of these is repaired here so the downstream lint only fails on *real*
decomposition errors (e.g. a sub-unit whose true measured payload exceeds the
target context window — which must NOT be papered over).
"""
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


def evidence_ref_sizes(evidence):
    """Canonical {ref: bytes} from the inlet-measured evidence bundle."""
    out = {}
    for item in evidence.get("refs") or [] if isinstance(evidence, dict) else []:
        if isinstance(item, dict) and isinstance(item.get("ref"), str) and isinstance(item.get("bytes"), int):
            out[item["ref"]] = item["bytes"]
    return out


def evidence_unresolved(evidence):
    degraded = evidence.get("degraded") if isinstance(evidence, dict) else None
    out = []
    for item in degraded.get("unresolved_refs") or [] if isinstance(degraded, dict) else []:
        ref = item.get("ref") or item.get("entity_ref") if isinstance(item, dict) else item
        if isinstance(ref, str) and ref:
            out.append(ref)
    return out


def _common_prefix_len(a, b):
    n = min(len(a), len(b))
    i = 0
    while i < n and a[i] == b[i]:
        i += 1
    return i


# Minimum shared-prefix length before we trust a snap. Canonical refs are
# `<type>:<project>:<hash>:...` or `file:<path>`; 24 chars clears the type +
# project prefix and bites into the discriminating body, so a match this long
# to a *unique* candidate is a transcription repair, not a coincidence.
_SNAP_MIN_PREFIX = 24


def snap_ref(ref, canonical):
    """Repair a corrupted ref to its canonical evidence ref, conservatively.

    Returns the canonical ref only when there is a single candidate sharing a
    long prefix and it is strictly longer than any other candidate's prefix.
    Otherwise returns the ref unchanged (let the lint fail loud).
    """
    if ref in canonical:
        return ref
    ranked = sorted(canonical, key=lambda c: _common_prefix_len(ref, c), reverse=True)
    if not ranked:
        return ref
    best = ranked[0]
    best_cp = _common_prefix_len(ref, best)
    second_cp = _common_prefix_len(ref, ranked[1]) if len(ranked) > 1 else 0
    if best_cp >= _SNAP_MIN_PREFIX and best_cp > second_cp:
        return best
    return ref


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--dag", required=True)
    p.add_argument("--ref-size", required=True)
    p.add_argument("--evidence", default="{}")
    args = p.parse_args()

    dag = load("dag", args.dag)
    ref_size = load("ref-size", args.ref_size)
    evidence = load("evidence", args.evidence)

    # Byte authority: this turn's ref-size measurement, backed by the
    # inlet-measured evidence sizes (used for snapped/canonical refs).
    canonical = evidence_ref_sizes(evidence)
    sizes = {**canonical, **ref_size_index(ref_size)}

    if isinstance(dag, dict):
        for unit in dag.get("sub_units") or []:
            if not isinstance(unit, dict):
                continue
            refs = unit.get("refs") or []
            # Repair corrupted refs to their canonical evidence ref.
            repaired = [snap_ref(ref, canonical) if isinstance(ref, str) else ref for ref in refs]
            unit["refs"] = repaired
            # Recompute bytes from measurement once every ref resolves.
            if repaired and all(isinstance(ref, str) and ref in sizes for ref in repaired):
                unit["bytes"] = sum(sizes[ref] for ref in repaired)

        contract = dag.get("recompose_contract")
        if isinstance(contract, dict):
            # Inject the canonical degraded set from the evidence bundle,
            # preserving any reason the facilitator supplied for a matching ref.
            reasons = {
                item.get("ref"): item.get("reason")
                for item in contract.get("degraded_refs") or []
                if isinstance(item, dict) and item.get("ref")
            }
            contract["degraded_refs"] = [
                {"ref": ref, "reason": reasons.get(ref, "unresolved at discovery")}
                for ref in evidence_unresolved(evidence)
            ]

            # Normalize cross_subunit_tests keyed `name` to the required `test_id`.
            for test in contract.get("cross_subunit_tests") or []:
                if isinstance(test, dict) and not test.get("test_id") and test.get("name"):
                    test["test_id"] = test["name"]

    json.dump(dag, sys.stdout, separators=(",", ":"))
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
