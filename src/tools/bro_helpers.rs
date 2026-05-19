use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::index;
use crate::orchestration;
use crate::orchestration::providers::Provider;
use crate::server::SharedState;
use crate::tools::bro_runtime_params::BroRosterEntry;
use crate::workflow;

pub(crate) fn resolve_actor_providers(
    actor: &workflow::schema::ActorSpec,
    state: &Arc<SharedState>,
) -> Result<Vec<orchestration::providers::Provider>, String> {
    use std::collections::HashSet;
    let mut providers: HashSet<orchestration::providers::Provider> = HashSet::new();
    if let Some(runtime) = &actor.runtime {
        let config = orchestration::allocator::load_effective_config(&state.store_dir, None);
        providers.extend(orchestration::allocator::provider_candidates_for_request(
            runtime, &config,
        ));
        return Ok(providers.into_iter().collect());
    }
    match actor.kind {
        workflow::schema::ActorKind::Executor => {
            let brofile_name = actor
                .brofile
                .as_deref()
                .ok_or_else(|| format!("actor (kind={:?}) missing brofile", actor.kind))?;
            let bf = orchestration::brofile::resolve_brofile(brofile_name, &state.store_dir, None)
                .ok_or_else(|| format!("brofile '{brofile_name}' not found"))?;
            providers.insert(bf.provider);
        }
        workflow::schema::ActorKind::Ensemble => {
            let team_name = actor
                .team
                .as_deref()
                .ok_or_else(|| "ensemble actor missing team".to_string())?;
            let team = orchestration::team::load_team(team_name, &state.store_dir)
                .ok_or_else(|| format!("team '{team_name}' not found"))?;
            for member in team.members.iter() {
                let bf = orchestration::brofile::resolve_brofile(
                    &member.brofile,
                    &state.store_dir,
                    None,
                )
                .ok_or_else(|| {
                    format!(
                        "team '{team_name}' member '{}' brofile '{}' not found",
                        member.name, member.brofile
                    )
                })?;
                providers.insert(bf.provider);
            }
        }
    }
    Ok(providers.into_iter().collect())
}

/// Extract a JSON workflow spec from an LLM's response text and
/// validate it via `workflow::compile`. Tolerates: raw JSON, ```json
/// fenced blocks, and prose preamble/trailing commentary. Returns the
/// original JSON Value (pre-compile) on success so callers can re-
/// emit the exact spec the author produced.
pub(crate) fn extract_and_compile_workflow(text: &str) -> Result<Value, String> {
    let candidates = extract_json_candidates(text);
    if candidates.is_empty() {
        return Err("no JSON object found in the author's output".into());
    }
    let mut last_err = String::new();
    for cand in candidates {
        let parsed: Value = match serde_json::from_str(&cand) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("JSON parse failed: {e}");
                continue;
            }
        };
        let spec: workflow::Workflow = match serde_json::from_value(parsed.clone()) {
            Ok(s) => s,
            Err(e) => {
                last_err = format!("workflow schema mismatch: {e}");
                continue;
            }
        };
        match workflow::compile(spec) {
            Ok(_) => return Ok(parsed),
            Err(e) => {
                last_err = format!("workflow cross-validation failed: {e}");
            }
        }
    }
    Err(last_err)
}

pub(crate) fn extract_json_candidates(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Strategy 1: fenced ```json ... ``` blocks. Most LLMs wrap.
    let mut remaining = text;
    while let Some(fence_start) = remaining.find("```json") {
        let after = &remaining[fence_start + "```json".len()..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(fence_end) = body.find("```") {
            out.push(body[..fence_end].trim().to_string());
            remaining = &body[fence_end + 3..];
        } else {
            break;
        }
    }
    // Strategy 2: bare ``` blocks (no language tag).
    let mut remaining = text;
    while let Some(fence_start) = remaining.find("```") {
        let after = &remaining[fence_start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(fence_end) = body.find("```") {
            let candidate = body[..fence_end].trim();
            if candidate.starts_with('{') {
                out.push(candidate.to_string());
            }
            remaining = &body[fence_end + 3..];
        } else {
            break;
        }
    }
    // Strategy 3: first balanced JSON object. This handles trailing
    // prose and accidental extra closing delimiters without accepting
    // partial objects.
    if let Some(candidate) = first_balanced_json_object(text) {
        out.push(candidate);
    }
    // Strategy 4: first `{` to last `}` of the whole text.
    if let Some(first) = text.find('{') {
        if let Some(last) = text.rfind('}') {
            if last > first {
                out.push(text[first..=last].to_string());
            }
        }
    }
    // Dedup while preserving order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out
}

fn first_balanced_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.pop() != Some(ch) {
                    return None;
                }
                if stack.is_empty() {
                    let end = start + offset + ch.len_utf8();
                    return Some(text[start..end].to_string());
                }
            }
            _ => {}
        }
    }

    None
}

pub(crate) fn split_csv(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

pub(crate) fn infer_provider_from_path(path: &Path) -> Option<Provider> {
    let s = path.to_string_lossy();
    if s.contains("/.codex/sessions/") {
        Some(Provider::Codex)
    } else if s.contains("/.gemini/tmp/") {
        Some(Provider::Gemini)
    } else if s.contains("/.copilot/session-state/") {
        Some(Provider::Copilot)
    } else if s.contains("/.vibe/logs/session/") {
        Some(Provider::Vibe)
    } else if s.contains("/projects/") {
        Some(Provider::Claude)
    } else {
        None
    }
}

pub(crate) fn build_member_entry(
    team: &orchestration::team::Team,
    member: &orchestration::team::TeamMember,
    store_dir: &Path,
    config: &index::ReindexConfig,
) -> BroRosterEntry {
    let brofile = orchestration::brofile::resolve_brofile(
        &member.brofile,
        store_dir,
        team.project_dir.as_deref(),
    );
    let provider = brofile.as_ref().map(|b| b.provider);
    let session_id = member
        .session_id
        .as_ref()
        .filter(|s| s.as_str() != "pending")
        .cloned();
    let jsonl_path = session_id
        .as_deref()
        .and_then(|sid| index::find_session_file(sid, &config.roots, config.codex_root.as_deref()))
        .map(|p| p.to_string_lossy().into_owned());
    BroRosterEntry {
        bro: member.name.clone(),
        bro_selector: format!("{}::{}", team.name, member.name),
        team: team.name.clone(),
        provider: provider
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".into()),
        account: brofile.as_ref().and_then(|b| {
            orchestration::brofile::effective_account(b.provider, b.account.as_deref(), store_dir)
        }),
        session_id,
        jsonl_path,
        brofile: member.brofile.clone(),
        model: brofile.and_then(|b| b.model),
    }
}

pub(crate) fn roster_entry_key(entry: &BroRosterEntry) -> String {
    if let Some(ref sid) = entry.session_id {
        format!("session::{sid}")
    } else {
        format!("member::{}", entry.bro_selector)
    }
}

pub(crate) fn tier0_cosine_threshold_from_env() -> f32 {
    const DEFAULT: f32 = 0.85;
    match std::env::var("BBOX_TIER0_COSINE_THRESHOLD") {
        Ok(raw) => match raw.parse::<f32>() {
            Ok(value) if (0.0..=1.0).contains(&value) => value,
            Ok(value) => {
                tracing::warn!(
                    value,
                    "BBOX_TIER0_COSINE_THRESHOLD outside [0.0, 1.0]; using default"
                );
                DEFAULT
            }
            Err(err) => {
                tracing::warn!(
                    value = raw,
                    error = %err,
                    "invalid BBOX_TIER0_COSINE_THRESHOLD; using default"
                );
                DEFAULT
            }
        },
        Err(_) => DEFAULT,
    }
}
