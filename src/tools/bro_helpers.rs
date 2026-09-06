use std::path::Path;

use crate::index;
use crate::orchestration;
use crate::orchestration::providers::Provider;
use crate::tools::bro_runtime_params::BroRosterEntry;

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
    let _ = s;
    None
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
