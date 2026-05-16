#!/usr/bin/env python3
"""Mechanical recompose-time assertions for the phase-decompose smoke."""

import json
import os
import sys


TERMINAL_VERDICTS = {"satisfied", "work_remains", "untenable"}


def load_payload():
    if "--payload-stdin" not in sys.argv:
        return {}
    raw = sys.stdin.read()
    if not raw.strip():
        return {}
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"stdin payload is not valid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise SystemExit("stdin payload must be a JSON object")
    return payload


def load_scalar(payload, name, default=""):
    if name in payload:
        value = payload.get(name)
    else:
        value = os.environ.get(name)
    if value is None:
        return default
    if isinstance(value, str):
        return value
    return str(value)


def load_json(payload, name, default):
    raw = payload.get(name) if name in payload else os.environ.get(name)
    if raw is None or raw == "":
        return default
    if isinstance(raw, (dict, list)):
        return raw
    if not isinstance(raw, str):
        return raw
    if raw.startswith("json:"):
        raw = raw[len("json:") :]
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{name} is not valid JSON: {exc}") from exc


def criterion_id(item):
    if isinstance(item, dict):
        return item.get("criterion_id") or item.get("id")
    return str(item)


def main() -> int:
    payload = load_payload()
    triage_verdict = load_scalar(payload, "TRIAGE_VERDICT")
    live_parent_arc_id = load_scalar(payload, "LIVE_PARENT_ARC_ID")
    recompose_verdict = load_scalar(payload, "RECOMPOSE_VERDICT")
    sub_results = load_json(payload, "SUB_RESULTS", [])
    dag = load_json(payload, "DAG", {})
    acceptance = load_json(payload, "ACCEPTANCE_CRITERIA", [])
    errors = []

    if triage_verdict != "needs_decompose":
        errors.append(f"triage_verdict must be needs_decompose, got {triage_verdict!r}")
    if not live_parent_arc_id:
        errors.append("LIVE_PARENT_ARC_ID is required")
    if recompose_verdict not in TERMINAL_VERDICTS:
        errors.append(
            "recompose_verdict must be one of "
            + ", ".join(sorted(TERMINAL_VERDICTS))
            + f", got {recompose_verdict!r}"
        )

    if not isinstance(sub_results, list) or not sub_results:
        errors.append("sub_results must be a non-empty array")
    else:
        dag_units = dag.get("sub_units", []) if isinstance(dag, dict) else []
        if isinstance(dag_units, list) and len(sub_results) != len(dag_units):
            errors.append(
                f"sub_results count must equal dag.sub_units count ({len(sub_results)} != {len(dag_units)})"
            )
        expected_keys = {
            str(unit.get("sub_unit_id"))
            for unit in dag_units
            if isinstance(unit, dict) and unit.get("sub_unit_id")
        }
        observed_keys = set()
        for idx, item in enumerate(sub_results):
            exports = item.get("exports") if isinstance(item, dict) else None
            if isinstance(item, dict):
                if item.get("status") != "completed":
                    errors.append(f"sub_results[{idx}].status must be completed")
                key = item.get("key")
                if isinstance(key, str) and key:
                    observed_keys.add(key)
            if not isinstance(exports, dict):
                errors.append(f"sub_results[{idx}].exports must be an object")
                continue
            implementation = exports.get("implementation_output")
            if not isinstance(implementation, dict):
                errors.append(
                    f"sub_results[{idx}].exports.implementation_output must be an object"
                )
            elif implementation.get("status") != "completed":
                errors.append(
                    f"sub_results[{idx}].implementation_output.status must be completed"
                )
            if (
                live_parent_arc_id
                and isinstance(implementation, dict)
                and implementation.get("live_arc_id") != live_parent_arc_id
            ):
                errors.append(
                    f"sub_results[{idx}].implementation_output.live_arc_id must equal live parent arc id"
                )
            files_touched = implementation.get("files_touched") if isinstance(implementation, dict) else None
            if files_touched != []:
                errors.append(f"sub_results[{idx}].implementation_output.files_touched must be []")
            if exports.get("advisor_verdict") != "accept":
                errors.append(f"sub_results[{idx}].exports.advisor_verdict must be accept")
            if exports.get("acceptance_status") != "passed":
                errors.append(f"sub_results[{idx}].exports.acceptance_status must be passed")
        if expected_keys and observed_keys != expected_keys:
            errors.append(
                "sub_results keys must equal dag sub_unit_id set: "
                f"missing={sorted(expected_keys - observed_keys)} extra={sorted(observed_keys - expected_keys)}"
            )

    expected_ids = {str(cid) for cid in (criterion_id(item) for item in acceptance) if cid}
    covered_ids = set()
    for unit in dag.get("sub_units", []) if isinstance(dag, dict) else []:
        if not isinstance(unit, dict):
            continue
        for criterion in unit.get("acceptance_subset") or []:
            cid = criterion_id(criterion)
            if cid:
                covered_ids.add(str(cid))
    missing_ids = sorted(expected_ids - covered_ids)
    if missing_ids:
        errors.append("acceptance criteria not covered by DAG: " + ", ".join(missing_ids))

    contract = dag.get("recompose_contract") if isinstance(dag, dict) else None
    cross_tests = contract.get("cross_subunit_tests") if isinstance(contract, dict) else None
    if not isinstance(cross_tests, list) or not cross_tests:
        errors.append("dag.recompose_contract.cross_subunit_tests must be non-empty")
    else:
        assertions = []
        terminal_sets = []
        for test in cross_tests:
            if not isinstance(test, dict):
                continue
            assertions.extend(str(item) for item in test.get("assertions") or [])
            terminal_sets.append(set(map(str, test.get("terminal_verdicts") or [])))
        required_fragments = [
            "triage_verdict",
            "sub_results",
            "recompose_verdict",
            "files_touched",
        ]
        assertion_text = "\n".join(assertions)
        for fragment in required_fragments:
            if fragment not in assertion_text:
                errors.append(f"cross_subunit_tests assertions must mention {fragment}")
        if not any(TERMINAL_VERDICTS == values for values in terminal_sets):
            errors.append("cross_subunit_tests must name terminal verdict set exactly")

    if errors:
        print("phase-decompose recompose assertions failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
    print(json.dumps({"ok": not errors, "errors": errors}, separators=(",", ":")))
    return 0 if not errors else 1


if __name__ == "__main__":
    raise SystemExit(main())
