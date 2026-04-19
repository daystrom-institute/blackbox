# Review packets — code review via rule-packets

A review packet encodes "what good looks like" for a change as a set of rules. Apply the packet in `mode="all"` to a feature entity and the packet produces a list of findings + an aggregate verdict. The packet's rules ARE the review criteria — making them inspectable, mergeable, and mechanically re-runnable.

See `sm-rule-packets` for the universal mechanism. This runbook is the review-domain instance.

## Lattice

`["fail", "flag", "manual", "pass", "info"]` — highest priority first.

- **fail** — definitional correctness issues (tests fail, warnings introduced, invariant broken). Blocks merge.
- **flag** — concern worth a reviewer's attention but not a blocker.
- **manual** — something a human must decide (security review, contract stability, performance tradeoff). Neither blocker nor clean.
- **pass** — positive signal; catchall "nothing else fired."
- **info** — diagnostic noise; not surfaced to the top of reports.

Verdict aggregation: any `fail` → verdict = fail. Else any `flag` → flag. Etc.

## Prefix inference (default — review)

The default prefix inference map is the review one:

```
fail_   → fail
flag_   → flag
manual_ → manual
review_ → manual
pass_   → pass
```

Rule IDs should use one of these prefixes so the classification infers automatically and ordering is auditable at a glance.

## Canonical shape

```json
{
  "domain": "code-review/my-feature",
  "classification_lattice": ["fail", "flag", "manual", "pass", "info"],
  "rules": [
    {
      "id": "fail_warnings_introduced",
      "antecedent": {"op": "Gt", "field": "new_warnings_from_this_change", "value": 0},
      "consequent": "FAIL: new compiler warnings introduced"
    },
    {
      "id": "fail_tests_regressed",
      "antecedent": {"op": "Eq", "field": "tests_pass", "value": false},
      "consequent": "FAIL: tests regressed"
    },
    {
      "id": "flag_readonly_fs_untested",
      "antecedent": {"op": "All", "args": [
        {"op": "Eq", "field": "startup_mkdir_called", "value": true},
        {"op": "Eq", "field": "readonly_fs_tested", "value": false}
      ]},
      "consequent": "FLAG: startup mkdir without readonly-fs test — could crash in containers"
    },
    {
      "id": "manual_security_surface_widened",
      "antecedent": {"op": "Gt", "field": "new_mcp_tools_exposed", "value": 0},
      "consequent": "MANUAL: new MCP tools — verify authz/input validation on each"
    },
    {
      "id": "pass_all_clean",
      "emit": "fallback",
      "antecedent": {"op": "True"},
      "consequent": "PASS: no FAIL/FLAG/MANUAL rule matched"
    }
  ]
}
```

## Adversarial-review pattern

1. Compile the packet on dev.
2. Build a feature entity from the PR's observable properties.
3. Apply in `mode="all"` — capture all findings + verdict.
4. Dispatch peer bros in parallel to critique the *packet itself* ("what rules are missing? what's over-specific? wrong ordering?").
5. Merge critiques into a v2 packet.
6. Iterate until bros stop finding structural issues.

This is how phase 2 + 2.5 got built — see `thread-0b20e854` and `thread-cc7ff97d`.

## Anti-patterns

- **Severity in the consequent string** (old v1/v2). Severity is first-class now via `classification`. The consequent is just display text.
- **First-match-wins for multi-finding review.** Use `mode="all"`. First-match is only right for classification tasks.
- **Non-fallback pass catchall.** A `pass_all_clean` with `emit: "independent"` fires alongside real findings. Always use `emit: "fallback"` for catchalls.
- **Phony review dimensions.** `flag_many_files > 5` is vibes-as-a-rule; churn ≠ risk. Drop rules that don't have mechanical backing.
- **Category errors.** `flag_permissive_confidence_default` reviews a policy of the evaluator, not the code change. Keep the packet focused on the entity.
- **Hardcoded constants that should be field-vs-field.** `Lt(tool_docs_stanzas_added, 3)` is brittle; use `FieldLt(tool_docs_stanzas_added, mcp_tools_added)` with an `IsNonNull` applicability guard.
