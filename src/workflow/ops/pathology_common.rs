//! Shared, behavior-neutral helpers for the pathology workflow ops
//! (`arch_pathology`, `perf_pathology`). This is a utility extraction, not a
//! shared diagnosis framework: the perf design (`design/refactor-tools/
//! perf-pathology.md`) deliberately keeps the two as *sibling* designs until
//! real usage shows what deserves a common abstraction. What lives here is only
//! the format-and-normalize plumbing that is genuinely identical between the
//! siblings — the diagnosis/evidence model and the per-sibling defaults stay in
//! each op module.

use std::fs;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::OpEffect;
use super::json_ops::{coerce_json_value, ensure_objectish_json, normalize_array_field};

/// Per-sibling normalization parameters. Arch carries `layer_model_path` /
/// `whole_project_mode`; perf carries `hot_paths` / `baseline_refs`. The
/// `survey_json` object is always special-cased and need not appear in
/// `inherit_keys`.
pub(super) struct NormalizeShape {
    pub op_label: &'static str,
    pub default_allowed_atoms: &'static [&'static str],
    /// Keys copied from `defaults` into each atom request when absent.
    pub inherit_keys: &'static [&'static str],
    /// Keys coerced to arrays (single string → one-element array) via
    /// `normalize_array_field`.
    pub array_fields: &'static [&'static str],
}

/// Normalize survey-produced atom requests: enforce the allowlist, fill missing
/// args from `defaults`, structure-coerce `survey_json`, and array-normalize the
/// declared list fields. Shared verbatim between arch and perf — the only
/// differences are the allowlist, inherit keys, and array fields in `shape`.
pub(super) fn normalize_atom_requests(
    args: &Value,
    into_var: Option<&str>,
    shape: &NormalizeShape,
) -> Result<OpEffect> {
    let label = shape.op_label;
    let into = into_var.ok_or_else(|| anyhow!("{label} requires into_var"))?;
    let requests = args
        .get("requests")
        .ok_or_else(|| anyhow!("{label} requires args.requests"))?;
    let defaults = args
        .get("defaults")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{label} requires args.defaults object"))?;
    let requests = coerce_json_value(requests);
    let requests = requests
        .as_array()
        .ok_or_else(|| anyhow!("{label} requires requests to be an array"))?;

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
        None => shape
            .default_allowed_atoms
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
        for key in shape.inherit_keys {
            if !request_args_obj.contains_key(*key)
                && let Some(value) = defaults.get(*key)
            {
                request_args_obj.insert((*key).to_string(), value.clone());
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

        for field in shape.array_fields {
            normalize_array_field(request_args_obj, defaults, field);
        }

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

/// Per-sibling correction-plan emission parameters.
pub(super) struct PlanShape {
    pub op_label: &'static str,
    /// Output directory components under the project root, e.g.
    /// `["design", "refactor", "plans"]` (arch) or
    /// `["design", "refactor", "perf", "plans"]` (perf).
    pub out_subdir: &'static [&'static str],
    /// Frontmatter `kind:` value.
    pub plan_kind: &'static str,
    /// Frontmatter `topic:` tag appended after `refactor-plan`.
    pub topic_tag: &'static str,
    /// Default `# title` / frontmatter title when the plan JSON omits one;
    /// rendered as `"{default_title_prefix}: {scope}"`.
    pub default_title_prefix: &'static str,
    pub default_brief: &'static str,
    pub slug_fallback: &'static str,
    pub default_generated_by: &'static str,
    pub default_criteria_prefix: &'static str,
}

/// Write a reviewed correction-plan markdown file from plan JSON. Shared between
/// arch and perf; `shape` carries the output path, frontmatter `kind`/`topic`,
/// and defaults that distinguish the two.
pub(super) fn write_pathology_plan(
    args: &Value,
    into_var: Option<&str>,
    shape: &PlanShape,
) -> Result<OpEffect> {
    let label = shape.op_label;
    let project_dir = args
        .get("project_dir")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{label} requires args.project_dir"))?;
    let slug = args
        .get("slug")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{label} requires args.slug"))?;
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{label} requires args.scope"))?;
    let baseline_commit = args
        .get("baseline_commit")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{label} requires args.baseline_commit"))?;
    let target_context_window = args
        .get("target_context_window")
        .and_then(|v| v.as_i64())
        .unwrap_or(10_000);
    let generated_by = args
        .get("generated_by")
        .and_then(Value::as_str)
        .unwrap_or(shape.default_generated_by);
    let criteria_prefix = args
        .get("criteria_prefix")
        .and_then(Value::as_str)
        .unwrap_or(shape.default_criteria_prefix);
    let plan = args
        .get("plan")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("{label} requires args.plan object"))?;

    let project_root = PathBuf::from(project_dir)
        .canonicalize()
        .map_err(|e| anyhow!("canonicalize project_dir {project_dir}: {e}"))?;
    let slug = pathology_slug(slug, shape.slug_fallback);
    let mut out_dir = project_root.clone();
    for component in shape.out_subdir {
        out_dir.push(component);
    }
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
        .unwrap_or_else(|| format!("{}: {scope}", shape.default_title_prefix));
    let brief = plan
        .get("brief")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(shape.default_brief);
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

    let frontmatter = format!(
        concat!(
            "---\n",
            "title: \"{title}\"\n",
            "kind: {kind}\n",
            "lifecycle: proposed\n",
            "corpus: project-refactor\n",
            "topic:\n",
            "  - refactor-plan\n",
            "  - {topic}\n",
            "date: {date}\n",
            "baseline_commit: {baseline}\n",
            "generated_by: {generated_by}\n",
            "scope: \"{scope}\"\n",
            "brief: \"{brief}\"\n",
            "---\n\n",
        ),
        title = yaml_quote(&title),
        kind = shape.plan_kind,
        topic = shape.topic_tag,
        date = today,
        baseline = baseline_commit.trim(),
        generated_by = generated_by,
        scope = yaml_quote(scope),
        brief = yaml_quote(brief),
    );

    let body = format!(
        concat!(
            "{frontmatter}",
            "# {title}\n\n",
            "## Diagnosis Summary\n\n{diagnosis}\n\n",
            "## Evidence\n\n{evidence}\n\n",
            "{authority}{atom_mapping}",
            "## Remediation Plan\n\n{remediation}\n\n",
            "## Acceptance Criteria\n\n{criteria}\n\n",
            "## Contradictions Requiring Human Judgment\n\n{contradictions}\n\n",
            "## Deferred\n\n{deferred}\n\n",
            "{refuted}",
            "## Dispatch Payload\n\n{dispatch}\n",
        ),
        frontmatter = frontmatter,
        title = title,
        diagnosis = pathology_markdown(
            plan.get("diagnosis_summary"),
            "No diagnosis survived review."
        ),
        evidence = pathology_markdown(plan.get("evidence"), "No evidence was retained."),
        authority = pathology_optional_section(plan.get("authority_grades"), "Authority Grades"),
        atom_mapping = pathology_optional_section(plan.get("atom_mapping"), "Atom Mapping"),
        remediation = pathology_markdown(
            plan.get("remediation_plan"),
            "No remediation slices were retained."
        ),
        criteria = pathology_criteria_markdown(&criteria, criteria_prefix),
        // Surviving unresolved challenges flow here instead of being force-resolved.
        contradictions = pathology_findings_markdown(
            plan.get("contradictions"),
            "No unresolved contradictions surfaced — the panel reached consensus or every challenge was resolved during debate."
        ),
        // Validator-refuted findings are excluded from the remediation slices
        // above and recorded here for audit (omitted when nothing was refuted).
        refuted =
            pathology_optional_findings_section(plan.get("refuted_findings"), "Refuted Findings"),
        deferred = pathology_markdown(
            plan.get("deferred"),
            "No deferred candidates were recorded."
        ),
        dispatch = pathology_dispatch_payload(
            project_root.to_string_lossy().as_ref(),
            &plan_rel,
            &criteria,
            target_context_window,
        ),
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

pub(super) fn pathology_slug(raw: &str, fallback: &str) -> String {
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
        fallback.to_string()
    } else {
        slug
    }
}

fn yaml_quote(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(super) fn pathology_markdown(value: Option<&Value>, fallback: &str) -> String {
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

pub(super) fn pathology_optional_section(value: Option<&Value>, heading: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            format!("## {heading}\n\n{}\n\n", s.trim())
        }
        Some(Value::Array(items)) if !items.is_empty() => {
            format!("## {heading}\n\n{}\n\n", pathology_markdown(value, ""))
        }
        _ => String::new(),
    }
}

/// Render a list of finding-shaped entries (contradictions, refuted findings)
/// as markdown bullets. Strings and string arrays pass through like
/// `pathology_markdown`; objects render as `**title** — detail`, pulling the
/// header from `title`/`claim`/`finding`/`id` and the body from
/// `detail`/`tension`/`positions`/`body`/`trace`/`reason`.
pub(super) fn pathology_findings_markdown(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| match item {
                Value::String(s) => format!("- {s}"),
                Value::Object(obj) => {
                    let title = obj
                        .get("title")
                        .or_else(|| obj.get("claim"))
                        .or_else(|| obj.get("finding"))
                        .or_else(|| obj.get("id"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("(untitled)");
                    let detail = obj
                        .get("detail")
                        .or_else(|| obj.get("tension"))
                        .or_else(|| obj.get("positions"))
                        .or_else(|| obj.get("body"))
                        .or_else(|| obj.get("trace"))
                        .or_else(|| obj.get("reason"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    match detail {
                        Some(d) => format!("- **{title}** — {d}"),
                        None => format!("- **{title}**"),
                    }
                }
                other => format!("- `{}`", serde_json::to_string(other).unwrap_or_default()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => fallback.to_string(),
    }
}

/// Like `pathology_optional_section` but for finding-shaped lists; renders a
/// `## {heading}` block only when there is at least one entry.
pub(super) fn pathology_optional_findings_section(value: Option<&Value>, heading: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            format!(
                "## {heading}\n\n{}\n\n",
                pathology_findings_markdown(value, "")
            )
        }
        Some(Value::Array(items)) if !items.is_empty() => {
            format!(
                "## {heading}\n\n{}\n\n",
                pathology_findings_markdown(value, "")
            )
        }
        _ => String::new(),
    }
}

pub(super) fn pathology_criteria_markdown(criteria: &Value, criteria_prefix: &str) -> String {
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

pub(super) fn pathology_dispatch_payload(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_shape() -> PlanShape {
        PlanShape {
            op_label: "test_write_plan",
            out_subdir: &["design", "refactor", "plans"],
            plan_kind: "correction-plan",
            topic_tag: "architecture",
            default_title_prefix: "Architecture Correction Plan",
            default_brief: "test brief",
            slug_fallback: "plan",
            default_generated_by: "arch-pathology",
            default_criteria_prefix: "AP",
        }
    }

    fn write_and_read(args: &Value) -> String {
        let eff = write_pathology_plan(args, Some("out"), &test_shape()).unwrap();
        let path = match eff {
            OpEffect::SetVar { value, .. } => value
                .get("absolute_plan_path")
                .and_then(Value::as_str)
                .unwrap()
                .to_string(),
            other => panic!("expected SetVar, got {other:?}"),
        };
        fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn plan_render_includes_contradictions_and_refuted_appendix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let args = json!({
            "project_dir": root.to_string_lossy(),
            "slug": "demo-scope",
            "scope": "src/demo",
            "baseline_commit": "abc123",
            "plan": {
                "diagnosis_summary": "One surviving diagnosis.",
                "evidence": "Concrete refs.",
                "remediation_plan": "Slice 1: do the thing.",
                "acceptance_criteria": [{ "id": "AP-1", "criterion_text": "thing done" }],
                "contradictions": [
                    { "title": "Ownership of SessionData", "tension": "soundness says extract; resilience says blast radius too high" }
                ],
                "refuted_findings": [
                    { "title": "False n+1 in loader", "trace": "validator: batched via join, no per-row fetch" }
                ]
            }
        });
        let body = write_and_read(&args);
        assert!(body.contains("## Contradictions Requiring Human Judgment"));
        assert!(body.contains("Ownership of SessionData"));
        assert!(body.contains("## Refuted Findings"));
        assert!(body.contains("False n+1 in loader"));
        // A refuted finding must never appear in the remediation slices.
        let before_criteria = body.split("## Acceptance Criteria").next().unwrap();
        assert!(!before_criteria.contains("False n+1 in loader"));
    }

    #[test]
    fn plan_render_omits_refuted_section_and_shows_contradiction_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let args = json!({
            "project_dir": root.to_string_lossy(),
            "slug": "clean",
            "scope": "src/clean",
            "baseline_commit": "def456",
            "plan": { "diagnosis_summary": "ok", "evidence": "ok", "remediation_plan": "ok" }
        });
        let body = write_and_read(&args);
        // No refuted findings → appendix omitted entirely.
        assert!(!body.contains("## Refuted Findings"));
        // Contradictions section always present, with its consensus fallback.
        assert!(body.contains("## Contradictions Requiring Human Judgment"));
        assert!(body.contains("No unresolved contradictions surfaced"));
    }
}
