# Auth packets — authorization matrices via rule-packets

An auth packet encodes an authorization policy: given a `(role, method, resource)` tuple (or similar subject × action × object triple), the packet returns ALLOW or DENY. Apply in `mode="first"` — authorization is classification, not multi-finding.

See `sm-rule-packets` for the universal mechanism. This runbook is the auth-domain instance.

## Lattice

`["deny", "allow"]` — DENY wins. This is the inverse of review's "fail > pass" direction: in authorization, the most restrictive classification takes precedence, which matches the industry convention that explicit DENY overrides implicit ALLOW.

## Prefix inference

```
deny_   → deny
allow_  → allow
anom_   → deny         # by convention, anomalies in auth are typically denials
```

## Canonical shape

Auth packets lean heavily on `rank_table` and `threshold_table` for the common rank-based-access pattern:

```json
{
  "domain": "auth/my-service",
  "classification_lattice": ["deny", "allow"],
  "prefix_inference": {
    "deny_": "deny",
    "allow_": "allow",
    "anom_": "deny"
  },
  "rank_table": {
    "auditor": 0,
    "reader": 1,
    "editor": 2,
    "owner": 3,
    "admin": 4
  },
  "threshold_table": {
    "public": 1,
    "team": 2,
    "private": 3,
    "billing": 3,
    "archived": 4
  },
  "rules": [
    // Anomalies FIRST — specific overrides before general rules
    {
      "id": "anom_admin_delete_billing",
      "antecedent": {"op": "All", "args": [
        {"op": "Eq", "field": "role", "value": "admin"},
        {"op": "Eq", "field": "method", "value": "DELETE"},
        {"op": "Eq", "field": "resource", "value": "billing"}
      ]},
      "consequent": "DENY"
    },
    {
      "id": "anom_editor_post_archived",
      "antecedent": {"op": "All", "args": [
        {"op": "Eq", "field": "role", "value": "editor"},
        {"op": "Eq", "field": "method", "value": "POST"},
        {"op": "Eq", "field": "resource", "value": "archived"}
      ]},
      "consequent": "ALLOW"
    },
    // General rules
    {
      "id": "allow_get_default",
      "antecedent": {"op": "Eq", "field": "method", "value": "GET"},
      "consequent": "ALLOW"
    },
    {
      "id": "allow_write_rank_ge_threshold",
      "antecedent": {"op": "RankGeFieldThreshold",
                     "rank_field": "role_rank",
                     "threshold_field": "res_threshold"},
      "consequent": "ALLOW"
    },
    {
      "id": "deny_default",
      "antecedent": {"op": "True"},
      "consequent": "DENY"
    }
  ]
}
```

The `rank_table` and `threshold_table` lookups augment each entity at eval time: `{role: "editor", resource: "team"}` gets `role_rank=2` and `res_threshold=2` added before rule evaluation.

## Apply mode

Always `mode="first"`. Authorization is a classification: one decision per request. There's no notion of "multiple matching findings" — the first applicable rule is the answer.

## Extrapolation property

This is the auth packet's killer feature. Add a new role to `rank_table` (say `"contributor": 2`) and the existing rank-gate rules apply automatically to that role without modification. The rules transmitted the *law*, not the data. This was proved empirically in E8 (thread-0b20e854): a 125-cell matrix compressed to a 60-token packet generalized correctly to a role never seen in training.

## Anti-patterns

- **Multi-finding auth.** Don't use `mode="all"` — every request gets ONE decision.
- **DENY after ALLOW for the same trigger.** Order matters: anomalies go FIRST. Auth is first-match-wins.
- **Missing catchall DENY.** Without a `deny_default` at the end, unmatched entities get `verdict=null` / no decision — insecure by default.
- **`emit: "fallback"` in auth.** Fallback is a `mode="all"` concept; don't use it here.
