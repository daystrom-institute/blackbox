#!/usr/bin/env python3
"""Mechanical DAG shape and acceptance-coverage lint for phase-decompose."""

import argparse
import json
import sys


def load_json_arg(name: str, value: str):
    try:
        return json.loads(value)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{name} is not valid JSON: {exc}") from exc


def criterion_ids(criteria):
    ids = set()
    for idx, item in enumerate(criteria or []):
        if isinstance(item, dict):
            cid = item.get("criterion_id") or item.get("id")
        else:
            cid = str(item)
        if cid:
            ids.add(str(cid))
        else:
            ids.add(f"criterion-{idx + 1}")
    return ids


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dag", required=True)
    parser.add_argument("--acceptance", default="[]")
    parser.add_argument("--target-context-window", type=int)
    args = parser.parse_args()

    dag = load_json_arg("dag", args.dag)
    acceptance = load_json_arg("acceptance", args.acceptance)
    errors = []

    sub_units = dag.get("sub_units") if isinstance(dag, dict) else None
    if not isinstance(sub_units, list) or not sub_units:
        errors.append("dag.sub_units must be a non-empty array")
        sub_units = []
    elif len(sub_units) < 2:
        errors.append("dag.sub_units must contain at least two sub-units for a decomposed path")

    ids = []
    covered = set()
    for idx, unit in enumerate(sub_units):
        if not isinstance(unit, dict):
            errors.append(f"sub_units[{idx}] must be an object")
            continue
        sid = unit.get("sub_unit_id")
        if not sid:
            errors.append(f"sub_units[{idx}].sub_unit_id is required")
        else:
            ids.append(str(sid))
        refs = unit.get("refs")
        if not isinstance(refs, list) or not refs:
            errors.append(f"sub_units[{idx}].refs must be a non-empty array")
        bytes_value = unit.get("bytes")
        if not isinstance(bytes_value, int) or bytes_value < 0:
            errors.append(f"sub_units[{idx}].bytes must be a non-negative integer")
        elif args.target_context_window is not None and bytes_value > args.target_context_window:
            errors.append(
                f"sub_units[{idx}].bytes exceeds target_context_window "
                f"({bytes_value} > {args.target_context_window})"
            )
        depends_on = unit.get("depends_on")
        if depends_on is None:
            errors.append(f"sub_units[{idx}].depends_on is required")
        elif not isinstance(depends_on, list):
            errors.append(f"sub_units[{idx}].depends_on must be an array")
        predicted_writes = unit.get("predicted_writes")
        if predicted_writes is None:
            errors.append(f"sub_units[{idx}].predicted_writes is required")
        elif not isinstance(predicted_writes, list):
            errors.append(f"sub_units[{idx}].predicted_writes must be an array")
        subset = unit.get("acceptance_subset")
        if not isinstance(subset, list) or not subset:
            errors.append(f"sub_units[{idx}].acceptance_subset must be a non-empty array")
        for criterion in subset or []:
            if isinstance(criterion, dict):
                cid = criterion.get("criterion_id") or criterion.get("id")
            else:
                cid = str(criterion)
            if cid:
                covered.add(str(cid))

    if len(ids) != len(set(ids)):
        errors.append("sub_unit_id values must be unique")

    id_set = set(ids)
    for idx, unit in enumerate(sub_units):
        if not isinstance(unit, dict):
            continue
        sid = str(unit.get("sub_unit_id", f"index-{idx}"))
        for dep in unit.get("depends_on") or []:
            dep = str(dep)
            if dep not in id_set:
                errors.append(f"{sid}.depends_on references unknown sub_unit_id {dep}")
            if dep == sid:
                errors.append(f"{sid}.depends_on must not reference itself")

    contract = dag.get("recompose_contract") if isinstance(dag, dict) else None
    merge_order = contract.get("merge_order") if isinstance(contract, dict) else None
    if not isinstance(merge_order, list):
        errors.append("recompose_contract.merge_order must be an array")
    elif set(map(str, merge_order)) != set(ids):
        errors.append("merge_order must contain exactly the sub_unit_id set")

    cross_tests = contract.get("cross_subunit_tests") if isinstance(contract, dict) else None
    if not isinstance(cross_tests, list) or not cross_tests:
        errors.append("recompose_contract.cross_subunit_tests must be a non-empty array")
    else:
        allowed_terminal = {"satisfied", "work_remains", "untenable"}
        for idx, test in enumerate(cross_tests):
            if not isinstance(test, dict):
                errors.append(f"cross_subunit_tests[{idx}] must be an object")
                continue
            test_id = test.get("test_id")
            if not isinstance(test_id, str) or not test_id:
                errors.append(f"cross_subunit_tests[{idx}].test_id is required")
            assertions = test.get("assertions")
            if not isinstance(assertions, list) or not assertions:
                errors.append(f"cross_subunit_tests[{idx}].assertions must be a non-empty array")
            terminal_verdicts = test.get("terminal_verdicts")
            if not isinstance(terminal_verdicts, list) or not terminal_verdicts:
                errors.append(f"cross_subunit_tests[{idx}].terminal_verdicts must be a non-empty array")
                continue
            unknown = sorted({str(v) for v in terminal_verdicts} - allowed_terminal)
            if unknown:
                errors.append(
                    f"cross_subunit_tests[{idx}].terminal_verdicts contains non-recompose verdicts: "
                    + ", ".join(unknown)
                )

    required = criterion_ids(acceptance)
    missing = sorted(required - covered)
    if missing:
        errors.append("acceptance criteria not covered: " + ", ".join(missing))

    result = {"ok": not errors, "errors": errors, "sub_unit_count": len(sub_units)}
    json.dump(result, sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")
    if errors:
        print("phase-decompose DAG lint failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
    return 0 if not errors else 1


if __name__ == "__main__":
    raise SystemExit(main())
