use super::*;
use rmcp::schemars;

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
