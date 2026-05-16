#!/usr/bin/env python3
import argparse, json, sys

TERM = {"satisfied", "work_remains", "untenable"}


def load(name, raw):
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{name} invalid JSON: {exc}") from exc


def crit_id(item, idx=None):
    v = item.get("criterion_id") or item.get("id") if isinstance(item, dict) else str(item) if item is not None else None
    return str(v) if v else f"criterion-{idx + 1}" if idx is not None else None


def unique_refs(units):
    out, seen = [], set()
    for u in units:
        for r in u.get("refs") or [] if isinstance(u, dict) else []:
            if isinstance(r, str) and r and r not in seen:
                seen.add(r)
                out.append(r)
    return out


def evidence_refs(evidence):
    out = set()
    for item in evidence.get("refs") or [] if isinstance(evidence, dict) else []:
        r = item if isinstance(item, str) else item.get("ref") or item.get("entity_ref") if isinstance(item, dict) else None
        if isinstance(r, str) and r:
            out.add(r)
    return out


def degraded_refs(evidence):
    degraded = evidence.get("degraded") if isinstance(evidence, dict) else None
    out = []
    for item in degraded.get("unresolved_refs") or [] if isinstance(degraded, dict) else []:
        r = item.get("ref") or item.get("entity_ref") if isinstance(item, dict) else item
        if isinstance(r, str) and r:
            out.append(r)
    return out


def ref_size_index(payload):
    if not isinstance(payload, dict):
        return {}, ["ref_size not object"]
    errors, sizes = [], {}
    degraded = payload.get("degraded")
    if isinstance(degraded, dict):
        unresolved = degraded.get("unresolved_refs") or []
        if unresolved:
            refs = [str(x.get("ref") or x.get("entity_ref") or x) if isinstance(x, dict) else str(x) for x in unresolved]
            errors.append("unresolved refs: " + ", ".join(refs))
        if degraded.get("omitted_refs"):
            errors.append(f"omitted refs: {degraded.get('omitted_refs')}")
    per_ref = payload.get("per_ref") or []
    if not isinstance(per_ref, list):
        return sizes, errors + ["ref_size.per_ref not array"]
    for item in per_ref:
        if isinstance(item, dict) and isinstance(item.get("ref"), str) and isinstance(item.get("bytes"), int):
            sizes[item["ref"]] = item["bytes"]
    return sizes, errors


def check_units(units, measured, target):
    errors, ids, covered = [], [], set()
    for i, u in enumerate(units):
        if not isinstance(u, dict):
            errors.append(f"sub_units[{i}] not object")
            continue
        sid, refs, declared = u.get("sub_unit_id"), u.get("refs"), u.get("bytes")
        if sid:
            ids.append(str(sid))
        else:
            errors.append(f"sub_units[{i}].sub_unit_id required")
        if not isinstance(refs, list) or not refs:
            errors.append(f"sub_units[{i}].refs required")
            refs = []
        if not isinstance(declared, int) or declared < 0:
            errors.append(f"sub_units[{i}].bytes invalid")
        elif target is not None and declared > target:
            errors.append(f"sub_units[{i}].bytes > target ({declared} > {target})")
        if measured is not None:
            missing = [r for r in refs if isinstance(r, str) and r not in measured]
            if missing:
                errors.append(f"sub_units[{i}].refs unmeasured: " + ", ".join(missing))
            total = sum(measured.get(r, 0) for r in refs if isinstance(r, str))
            if target is not None and total > target:
                errors.append(f"sub_units[{i}].measured > target ({total} > {target})")
            if isinstance(declared, int) and declared != total:
                errors.append(f"sub_units[{i}].bytes != measured ({declared} != {total})")
        if not isinstance(u.get("depends_on"), list):
            errors.append(f"sub_units[{i}].depends_on not array")
        if "assigned_brofile" in u:
            errors.append(f"sub_units[{i}].assigned_brofile forbidden")
        if not isinstance(u.get("predicted_writes"), list):
            errors.append(f"sub_units[{i}].predicted_writes not array")
        subset = u.get("acceptance_subset")
        if not isinstance(subset, list) or not subset:
            errors.append(f"sub_units[{i}].acceptance_subset required")
        for item in subset or []:
            cid = crit_id(item)
            if cid:
                covered.add(cid)
    if len(ids) != len(set(ids)):
        errors.append("duplicate sub_unit_id")
    return errors, ids, covered


def check_deps(units, ids):
    errors, graph = [], {}
    for i, u in enumerate(units):
        if not isinstance(u, dict):
            continue
        sid = str(u.get("sub_unit_id") or f"index-{i}")
        graph[sid] = []
        for dep in u.get("depends_on") or []:
            dep = str(dep)
            if dep not in ids:
                errors.append(f"{sid}.depends_on unknown {dep}")
            elif dep == sid:
                errors.append(f"{sid}.depends_on self")
            else:
                graph[sid].append(dep)
    active, done, path = set(), set(), []

    def walk(n):
        if n in done:
            return
        if n in active:
            start = path.index(n) if n in path else 0
            errors.append("cycle: " + " -> ".join(path[start:] + [n]))
            return
        active.add(n)
        path.append(n)
        for d in graph.get(n, []):
            walk(d)
        path.pop()
        active.remove(n)
        done.add(n)

    for n in graph:
        walk(n)
    return errors


def check_contract(dag, ids):
    errors = []
    c = dag.get("recompose_contract") if isinstance(dag, dict) else None
    order = c.get("merge_order") if isinstance(c, dict) else None
    if not isinstance(order, list):
        errors.append("merge_order required")
    elif set(map(str, order)) != set(ids):
        errors.append("merge_order must equal sub_unit ids")
    tests = c.get("cross_subunit_tests") if isinstance(c, dict) else None
    if not isinstance(tests, list) or not tests:
        errors.append("cross_subunit_tests required")
        return errors, c
    for i, t in enumerate(tests):
        if not isinstance(t, dict):
            errors.append(f"cross_subunit_tests[{i}] not object")
            continue
        if not t.get("test_id"):
            errors.append(f"cross_subunit_tests[{i}].test_id required")
        if not isinstance(t.get("assertions"), list) or not t.get("assertions"):
            errors.append(f"cross_subunit_tests[{i}].assertions required")
        verdicts = t.get("terminal_verdicts")
        if not isinstance(verdicts, list) or not verdicts:
            errors.append(f"cross_subunit_tests[{i}].terminal_verdicts required")
        elif set(map(str, verdicts)) != TERM:
            errors.append(f"cross_subunit_tests[{i}].terminal_verdicts wrong")
    return errors, c


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--dag", required=True)
    p.add_argument("--acceptance", default="[]")
    p.add_argument("--target-context-window", type=int)
    p.add_argument("--ref-size")
    p.add_argument("--evidence", default="{}")
    p.add_argument("--emit-refs", action="store_true")
    a = p.parse_args()
    dag, acceptance, evidence = load("dag", a.dag), load("acceptance", a.acceptance), load("evidence", a.evidence)
    units, errors = dag.get("sub_units") if isinstance(dag, dict) else None, []
    if not isinstance(units, list) or not units:
        errors.append("dag.sub_units required")
        units = []
    elif len(units) < 2:
        errors.append("dag.sub_units needs at least 2")
    if a.emit_refs:
        json.dump(unique_refs(units), sys.stdout, separators=(",", ":"))
        print()
        return 0
    measured = None
    if a.ref_size is not None:
        measured, more = ref_size_index(load("ref-size", a.ref_size))
        errors += more
    more, ids, covered = check_units(units, measured, a.target_context_window)
    errors += more + check_deps(units, set(ids))
    more, contract = check_contract(dag, ids)
    errors += more
    unresolved = degraded_refs(evidence)
    contract_degraded = set()
    contract_degraded_field = None
    if isinstance(contract, dict):
        contract_degraded_field = contract.get("degraded_refs")
        if isinstance(contract_degraded_field, list):
            contract_degraded = {
                str(x.get("ref"))
                for x in contract_degraded_field
                if isinstance(x, dict) and x.get("ref")
            }
    if unresolved:
        if not isinstance(contract_degraded_field, list):
            errors.append("degraded_refs required")
        missing = sorted(set(unresolved) - contract_degraded)
        if missing:
            errors.append("degraded refs missing: " + ", ".join(missing))
    extra_degraded = sorted(contract_degraded - set(unresolved))
    if extra_degraded:
        errors.append("unexpected degraded refs: " + ", ".join(extra_degraded))
    unresolved_set = set(unresolved)
    missing_evidence = sorted((evidence_refs(evidence) - unresolved_set) - set(unique_refs(units)))
    if missing_evidence:
        errors.append("evidence refs missing: " + ", ".join(missing_evidence))
    needed = {crit_id(x, i) for i, x in enumerate(acceptance or [])}
    missing_ac = sorted(needed - covered)
    if missing_ac:
        errors.append("acceptance not covered: " + ", ".join(missing_ac))
    json.dump({"ok": not errors, "errors": errors, "sub_unit_count": len(units)}, sys.stdout, separators=(",", ":"))
    print()
    if errors:
        print("phase-decompose DAG lint failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
    return 0 if not errors else 1


if __name__ == "__main__":
    raise SystemExit(main())
