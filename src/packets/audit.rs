use super::Packet;
use super::apply::{ApplyMode, NoopResolver, PacketResolver, apply_all_with, apply_with};
use super::ast::Value;
use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuditParams {
    #[schemars(regex(pattern = r"^(packet-)?[0-9a-f]{8}$"))]
    pub packet_id: String,
    /// Dataset shape depends on `mode`:
    /// - `mode="first"` (default): JSON array of `{entity, expected}` pairs
    ///   where `expected` is the Value the packet's first matching rule
    ///   should emit. Matches the original audit shape.
    /// - `mode="all"`: JSON array of
    ///   `{entity, expected_verdict?: string, expected_rule_ids?: [string]}`
    ///   pairs. `expected_verdict` matches `ApplyAllResult.verdict`;
    ///   `expected_rule_ids` is compared as a SET (order-invariant) against
    ///   the rule IDs that fired. Either can be omitted if you only care
    ///   about one check; a row with both omitted trivially passes.
    pub dataset: serde_json::Value,
    /// `"first"` (default) compares single-rule consequent; `"all"`
    /// compares aggregate verdict + fired-rule-id set. Use `"all"` to
    /// validate review/design packets that rely on multi-finding shape.
    #[serde(default)]
    pub mode: Option<ApplyMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FidelityReport {
    pub total: usize,
    pub correct: usize,
    pub fidelity: f32,
    pub mismatches: Vec<Mismatch>,
    pub uncovered: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mismatch {
    pub entity: serde_json::Value,
    pub expected: Value,
    pub predicted: Option<Value>,
    pub rule_id: Option<String>,
}

/// Fidelity report for `audit_mode="all"`. Compares aggregate verdict
/// and the set of fired rule IDs independently; a row can fail on
/// either dimension, and the report tags which one so fixes are
/// targeted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllModeFidelityReport {
    pub total: usize,
    pub correct: usize,
    pub fidelity: f32,
    pub mismatches: Vec<AllModeMismatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllModeMismatch {
    pub entity: serde_json::Value,
    /// `"verdict"` when aggregate verdict diverged; `"rule_ids"` when
    /// fired-rule-id set diverged; `"both"` when both diverged.
    pub check: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rule_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_rule_ids: Option<Vec<String>>,
}

/// Apply packet in `mode="all"` to every row of `dataset`. Row shape:
/// `{entity, expected_verdict?, expected_rule_ids?}`. Compares aggregate
/// verdict + fired-rule-id set independently; mismatches tag which
/// check failed so fixes are targeted.
#[allow(dead_code)] // test-only wrapper around `verify_all_with`
pub fn verify_all(packet: &Packet, dataset: &serde_json::Value) -> Result<AllModeFidelityReport> {
    verify_all_with(packet, dataset, &NoopResolver)
}

/// Composition-aware variant of [`verify_all`].
pub fn verify_all_with(
    packet: &Packet,
    dataset: &serde_json::Value,
    resolver: &dyn PacketResolver,
) -> Result<AllModeFidelityReport> {
    let rows = dataset.as_array().context(
        "dataset must be a JSON array of {entity, expected_verdict?, expected_rule_ids?} objects",
    )?;

    let mut total = 0usize;
    let mut correct = 0usize;
    let mut mismatches = Vec::new();

    for row in rows {
        let entity = row
            .get("entity")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_verdict: Option<String> = row
            .get("expected_verdict")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let expected_rule_ids: Option<Vec<String>> = row
            .get("expected_rule_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });

        // Row with no expectation at all trivially passes but doesn't
        // count toward fidelity — skip.
        if expected_verdict.is_none() && expected_rule_ids.is_none() {
            continue;
        }

        total += 1;
        let result = apply_all_with(packet, &entity, resolver);
        let actual_verdict = result.verdict.clone();
        let mut actual_rule_ids: Vec<String> =
            result.findings.iter().map(|p| p.rule_id.clone()).collect();
        actual_rule_ids.sort();

        let verdict_ok = expected_verdict
            .as_ref()
            .map(|ev| actual_verdict.as_ref() == Some(ev))
            .unwrap_or(true);

        let mut expected_ids_sorted = expected_rule_ids.clone();
        if let Some(ids) = expected_ids_sorted.as_mut() {
            ids.sort();
        }
        let ids_ok = expected_ids_sorted
            .as_ref()
            .map(|eids| &actual_rule_ids == eids)
            .unwrap_or(true);

        if verdict_ok && ids_ok {
            correct += 1;
        } else {
            let check = match (verdict_ok, ids_ok) {
                (false, false) => "both",
                (false, true) => "verdict",
                (true, false) => "rule_ids",
                _ => unreachable!(),
            };
            mismatches.push(AllModeMismatch {
                entity: entity.clone(),
                check: check.to_string(),
                expected_verdict: if !verdict_ok {
                    expected_verdict.clone()
                } else {
                    None
                },
                actual_verdict: if !verdict_ok { actual_verdict } else { None },
                expected_rule_ids: if !ids_ok { expected_ids_sorted } else { None },
                actual_rule_ids: if !ids_ok { Some(actual_rule_ids) } else { None },
            });
        }
    }

    let fidelity = if total == 0 {
        0.0
    } else {
        correct as f32 / total as f32
    };

    Ok(AllModeFidelityReport {
        total,
        correct,
        fidelity,
        mismatches,
    })
}

/// Apply packet to every entry in `dataset`. Dataset is a JSON array of
/// `{entity, expected}` pairs. Returns a fidelity report.
#[allow(dead_code)] // test-only wrapper around `verify_with`
pub fn verify(packet: &Packet, dataset: &serde_json::Value) -> Result<FidelityReport> {
    verify_with(packet, dataset, &NoopResolver)
}

/// Composition-aware variant of [`verify`].
pub fn verify_with(
    packet: &Packet,
    dataset: &serde_json::Value,
    resolver: &dyn PacketResolver,
) -> Result<FidelityReport> {
    let rows = dataset
        .as_array()
        .context("dataset must be a JSON array of {entity, expected} objects")?;

    let mut total = 0usize;
    let mut correct = 0usize;
    let mut mismatches = Vec::new();
    let mut uncovered = Vec::new();

    for row in rows {
        let entity = row
            .get("entity")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected_json = row
            .get("expected")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let expected = match Value::from_json(&expected_json) {
            Some(v) => v,
            None => continue, // skip malformed rows
        };

        total += 1;
        match apply_with(packet, &entity, resolver) {
            Some(prediction) if prediction.consequent == expected => {
                correct += 1;
            }
            Some(prediction) => {
                mismatches.push(Mismatch {
                    entity: entity.clone(),
                    expected,
                    predicted: Some(prediction.consequent),
                    rule_id: Some(prediction.rule_id),
                });
            }
            None => {
                uncovered.push(entity.clone());
            }
        }
    }

    let fidelity = if total == 0 {
        0.0
    } else {
        correct as f32 / total as f32
    };

    Ok(FidelityReport {
        total,
        correct,
        fidelity,
        mismatches,
        uncovered,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::super::test_support::tmp_packets;
    use super::super::{
        ApplyMode, Emit, Packet, Packets, Predicate, Rule, Value, review_lattice,
        review_prefix_inference,
    };
    use super::{AuditParams, verify_all};

    fn bare_packet(rules: Vec<Rule>) -> Packet {
        let now = Packets::now_iso();
        Packet {
            id: "packet-phase2t".into(),
            domain: "phase2-test".into(),
            scope: "global".into(),
            project: None,
            rank_table: BTreeMap::new(),
            threshold_table: BTreeMap::new(),
            rank_lookup_key: "role".into(),
            threshold_lookup_key: "resource".into(),
            classification_lattice: review_lattice(),
            prefix_inference: review_prefix_inference(),
            rules,
            source_ids: vec![],
            self_audit_fidelity: None,
            created_at: now.clone(),
            updated_at: now,
            superseded_by: None,
            merged_from: vec![],
        }
    }

    fn rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
        Rule {
            id: id.into(),
            antecedent,
            consequent: Value::String(consequent.into()),
            classification: class.into(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    fn fallback_rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
        Rule {
            id: id.into(),
            antecedent,
            consequent: Value::String(consequent.into()),
            classification: class.into(),
            emit: Emit::Fallback,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    fn multi_find_packet() -> Packet {
        bare_packet(vec![
            rule("fail_always", Predicate::AlwaysTrue {}, "FAIL", "fail"),
            rule(
                "flag_on_x",
                Predicate::Eq {
                    field: "x".into(),
                    value: Value::Int(1),
                },
                "FLAG_X",
                "flag",
            ),
            fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
        ])
    }

    #[test]
    fn verify_all_matches_verdict_and_rule_ids() {
        let p = multi_find_packet();
        // Entity with x=1: both fail_always and flag_on_x fire; verdict = fail.
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "fail",
            "expected_rule_ids": ["fail_always", "flag_on_x"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.correct, 1);
        assert!(report.mismatches.is_empty());
    }

    #[test]
    fn verify_all_flags_verdict_mismatch() {
        let p = multi_find_packet();
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "flag",
            "expected_rule_ids": ["fail_always", "flag_on_x"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.correct, 0);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].check, "verdict");
        assert_eq!(
            report.mismatches[0].expected_verdict.as_deref(),
            Some("flag")
        );
        assert_eq!(report.mismatches[0].actual_verdict.as_deref(), Some("fail"));
    }

    #[test]
    fn verify_all_flags_rule_ids_mismatch() {
        let p = multi_find_packet();
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "fail",
            "expected_rule_ids": ["fail_always"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.correct, 0);
        assert_eq!(report.mismatches[0].check, "rule_ids");
    }

    #[test]
    fn verify_all_flags_both_mismatches() {
        let p = multi_find_packet();
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_verdict": "pass",
            "expected_rule_ids": ["nonexistent_rule"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.mismatches[0].check, "both");
    }

    #[test]
    fn verify_all_rule_ids_order_invariant() {
        let p = multi_find_packet();
        // Order of expected_rule_ids differs from firing order — still matches.
        let dataset = json!([{
            "entity": {"x": 1},
            "expected_rule_ids": ["flag_on_x", "fail_always"]
        }]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(
            report.correct, 1,
            "rule_ids comparison is a set, not a list"
        );
    }

    #[test]
    fn verify_all_partial_expectations_ok() {
        let p = multi_find_packet();
        // Only expected_verdict set → only verdict checked.
        let dataset = json!([
            {"entity": {"x": 1}, "expected_verdict": "fail"},
            {"entity": {"x": 99}, "expected_verdict": "fail"}
        ]);
        let report = verify_all(&p, &dataset).unwrap();
        assert_eq!(report.correct, 2);
    }

    #[test]
    fn audit_tool_all_mode_via_mcp_surface() {
        let (_dir, store) = tmp_packets();
        store.save_packet(&multi_find_packet()).unwrap();
        let packet_id = multi_find_packet().id;

        let report = store
            .audit_tool(&AuditParams {
                packet_id: packet_id.clone(),
                dataset: json!([{
                    "entity": {"x": 1},
                    "expected_verdict": "fail",
                    "expected_rule_ids": ["fail_always", "flag_on_x"]
                }]),
                mode: Some(ApplyMode::All),
            })
            .unwrap();
        assert!(report.contains("\"mode\": \"all\""));
        assert!(report.contains("\"correct\": 1"));
        assert!(report.contains("\"fidelity\": 1.0"));
    }
}
