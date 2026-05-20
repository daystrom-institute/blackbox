use std::fs;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::OpEffect;
use super::json_ops::{coerce_json_value, ensure_objectish_json, normalize_array_field};

pub(super) fn exec_normalize_arch_pathology_atom_requests(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    let into =
        into_var.ok_or_else(|| anyhow!("NormalizeArchPathologyAtomRequests requires into_var"))?;
    let requests = args
        .get("requests")
        .ok_or_else(|| anyhow!("NormalizeArchPathologyAtomRequests requires args.requests"))?;
    let defaults = args
        .get("defaults")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!("NormalizeArchPathologyAtomRequests requires args.defaults object")
        })?;
    let requests = coerce_json_value(requests);
    let requests = requests.as_array().ok_or_else(|| {
        anyhow!("NormalizeArchPathologyAtomRequests requires requests to be an array")
    })?;

    let default_allowed_atoms = [
        "atom:java-architecture-role-behavior-coherence@v1",
        "atom:java-architecture-responsibility-bleed@v1",
        "atom:java-architecture-conceptual-duplicate-discovery@v1",
        "atom:java-architecture-anemic-data-remote-behavior@v1",
        "atom:java-architecture-scoped-context-capture@v1",
        "atom:java-architecture-framework-contract-violation@v1",
        "atom:java-architecture-test-implied-architecture@v1",
        "atom:java-architecture-transcript-anchored-pressure@v1",
    ];
    let allowed_atoms = match args.get("allowed_atoms").map(coerce_json_value) {
        Some(value) => value
            .as_array()
            .ok_or_else(|| anyhow!("allowed_atoms must be an array"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("allowed_atoms entries must be strings"))
            })
            .collect::<Result<Vec<_>>>()?,
        None => default_allowed_atoms
            .iter()
            .map(|atom| atom.to_string())
            .collect(),
    };

    let mut normalized = Vec::with_capacity(requests.len());
    for (idx, request) in requests.iter().enumerate() {
        let request = coerce_json_value(request);
        let request = request
            .as_object()
            .ok_or_else(|| anyhow!("atom request #{idx} must be an object, got {request:?}"))?;
        let atom_ref = request
            .get("atom_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("atom request #{idx} missing atom_ref"))?;
        if !allowed_atoms.iter().any(|allowed| allowed == atom_ref) {
            bail!("atom request #{idx} uses unsupported atom_ref '{atom_ref}'");
        }

        let mut request_args = request
            .get("args")
            .map(coerce_json_value)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let request_args_obj = request_args.as_object_mut().ok_or_else(|| {
            anyhow!("atom request #{idx} args must be an object after normalization")
        })?;
        for key in [
            "project_dir",
            "scope_filter",
            "target_loci",
            "operator_hints",
            "layer_model_path",
            "target_context_window",
            "whole_project_mode",
            "whiteboard_id",
        ] {
            if !request_args_obj.contains_key(key)
                && let Some(value) = defaults.get(key)
            {
                request_args_obj.insert(key.to_string(), value.clone());
            }
        }

        let survey_json = request_args_obj
            .remove("survey_json")
            .map(|v| ensure_objectish_json(coerce_json_value(&v)))
            .transpose()?
            .or_else(|| defaults.get("survey_json").cloned())
            .map(ensure_objectish_json)
            .transpose()?
            .unwrap_or_else(|| json!({}));
        request_args_obj.insert("survey_json".to_string(), survey_json);

        normalize_array_field(request_args_obj, defaults, "target_loci");
        normalize_array_field(request_args_obj, defaults, "operator_hints");

        normalized.push(json!({
            "atom_ref": atom_ref,
            "args": Value::Object(request_args_obj.clone()),
        }));
    }

    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: Value::Array(normalized),
    })
}

pub(super) fn exec_write_arch_pathology_plan(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    let project_dir = args
        .get("project_dir")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.project_dir"))?;
    let slug = args
        .get("slug")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.slug"))?;
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.scope"))?;
    let baseline_commit = args
        .get("baseline_commit")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.baseline_commit"))?;
    let target_context_window = args
        .get("target_context_window")
        .and_then(|v| v.as_i64())
        .unwrap_or(10_000);
    let generated_by = args
        .get("generated_by")
        .and_then(Value::as_str)
        .unwrap_or("arch-pathology");
    let criteria_prefix = args
        .get("criteria_prefix")
        .and_then(Value::as_str)
        .unwrap_or("AP");
    let plan = args
        .get("plan")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.plan object"))?;

    let project_root = PathBuf::from(project_dir)
        .canonicalize()
        .map_err(|e| anyhow!("canonicalize project_dir {project_dir}: {e}"))?;
    let slug = arch_pathology_slug(slug);
    let out_dir = project_root.join("design").join("refactor").join("plans");
    fs::create_dir_all(&out_dir).map_err(|e| {
        anyhow!(
            "create correction-plan directory {}: {e}",
            out_dir.display()
        )
    })?;
    let out_path = out_dir.join(format!("{slug}.md"));

    let title = plan
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Architecture Correction Plan: {scope}"));
    let brief = plan
        .get("brief")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Architecture pathology correction plan.");
    let criteria = plan
        .get("acceptance_criteria")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let plan_rel = out_path
        .strip_prefix(&project_root)
        .unwrap_or(&out_path)
        .to_string_lossy()
        .replace('\\', "/");
    let today = chrono::Utc::now().date_naive();
    let body = format!(
        concat!(
            "---\n",
            "title: \"{}\"\n",
            "kind: correction-plan\n",
            "lifecycle: proposed\n",
            "corpus: project-refactor\n",
            "topic:\n",
            "  - refactor-plan\n",
            "  - architecture\n",
            "date: {}\n",
            "baseline_commit: {}\n",
            "generated_by: {}\n",
            "scope: \"{}\"\n",
            "brief: \"{}\"\n",
            "---\n\n",
            "# {}\n\n",
            "## Diagnosis Summary\n\n{}\n\n",
            "## Evidence\n\n{}\n\n",
            "{}{}",
            "## Remediation Plan\n\n{}\n\n",
            "## Acceptance Criteria\n\n{}\n\n",
            "## Deferred\n\n{}\n\n",
            "## Dispatch Payload\n\n{}\n"
        ),
        yaml_quote(&title),
        today,
        baseline_commit.trim(),
        generated_by,
        yaml_quote(scope),
        yaml_quote(brief),
        title,
        arch_pathology_markdown(
            plan.get("diagnosis_summary"),
            "No diagnosis survived review."
        ),
        arch_pathology_markdown(plan.get("evidence"), "No evidence was retained."),
        arch_pathology_optional_section(plan.get("authority_grades"), "Authority Grades"),
        arch_pathology_optional_section(plan.get("atom_mapping"), "Atom Mapping"),
        arch_pathology_markdown(
            plan.get("remediation_plan"),
            "No remediation slices were retained."
        ),
        arch_pathology_criteria_markdown(&criteria, criteria_prefix),
        arch_pathology_markdown(
            plan.get("deferred"),
            "No deferred candidates were recorded."
        ),
        arch_pathology_dispatch_payload(
            project_root.to_string_lossy().as_ref(),
            &plan_rel,
            &criteria,
            target_context_window
        )
    );
    fs::write(&out_path, body)
        .map_err(|e| anyhow!("write correction plan {}: {e}", out_path.display()))?;

    let result = json!({
        "plan_path": plan_rel,
        "absolute_plan_path": out_path.to_string_lossy(),
        "acceptance_criteria": criteria,
    });
    if let Some(key) = into_var {
        Ok(OpEffect::SetVar {
            key: key.to_string(),
            value: result,
        })
    } else {
        Ok(OpEffect::None)
    }
}

fn arch_pathology_slug(raw: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug
        .trim_matches(|c| matches!(c, '-' | '.' | '_'))
        .to_string();
    if slug.is_empty() {
        "architecture-correction-plan".to_string()
    } else {
        slug
    }
}

fn yaml_quote(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

fn arch_pathology_markdown(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| match item {
                Value::String(s) => format!("- {s}"),
                other => format!("- `{}`", serde_json::to_string(other).unwrap_or_default()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => fallback.to_string(),
    }
}

fn arch_pathology_optional_section(value: Option<&Value>, heading: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            format!("## {heading}\n\n{}\n\n", s.trim())
        }
        Some(Value::Array(items)) if !items.is_empty() => {
            format!("## {heading}\n\n{}\n\n", arch_pathology_markdown(value, ""))
        }
        _ => String::new(),
    }
}

fn arch_pathology_criteria_markdown(criteria: &Value, criteria_prefix: &str) -> String {
    let Some(items) = criteria.as_array() else {
        return format!(
            "- {criteria_prefix}-1: The reviewed correction plan has at least one concrete acceptance criterion before PD dispatch."
        );
    };
    if items.is_empty() {
        return format!(
            "- {criteria_prefix}-1: The reviewed correction plan has at least one concrete acceptance criterion before PD dispatch."
        );
    }
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let default_id = format!("{criteria_prefix}-{}", idx + 1);
            if let Some(obj) = item.as_object() {
                let id = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_id);
                let text = obj
                    .get("criterion_text")
                    .or_else(|| obj.get("text"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| serde_json::to_string(item).unwrap_or_default());
                format!("- {id}: {text}")
            } else {
                format!("- {default_id}: {item}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn arch_pathology_dispatch_payload(
    project_dir: &str,
    plan_rel: &str,
    criteria: &Value,
    target_context_window: i64,
) -> String {
    let criteria = criteria.as_array().cloned().unwrap_or_default();
    let payload = json!({
        "workflow_id": "phase-decompose-main-edit",
        "project_dir": project_dir,
        "initial_vars": {
            "phase_doc_path": plan_rel,
            "phase_doc_text": "<full correction plan text>",
            "project_dir": project_dir,
            "target_context_window": target_context_window,
            "epoch": 0,
            "max_epochs": 3,
            "acceptance_criteria": criteria,
        }
    });
    format!(
        "```json\n{}\n```",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    )
}
