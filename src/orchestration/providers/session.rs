use std::path::Path;

use serde_json::Value;

use super::Provider;

pub fn discover_vibe_session(start_ms: u64, project_dir: &str) -> Option<String> {
    let session_dir = std::env::var("VIBE_SESSION_DIR").unwrap_or_else(|_| {
        let home = dirs::home_dir().unwrap_or_default();
        home.join(".vibe/logs/session")
            .to_string_lossy()
            .to_string()
    });
    let session_path = Path::new(&session_dir);

    let entries: Vec<String> = match std::fs::read_dir(session_path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("session_"))
            .collect(),
        Err(_) => return None,
    };

    let resolved_project =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    let mut scored: Vec<(String, u64, bool, bool)> = entries
        .iter()
        .filter_map(|name| {
            let meta_file = session_path.join(name).join("meta.json");
            let stat = std::fs::metadata(&meta_file).ok()?;
            let mtime_ms = stat
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            let data: Value =
                serde_json::from_str(&std::fs::read_to_string(&meta_file).ok()?).ok()?;
            let env = data.get("environment")?.as_object()?;
            let wd = env.get("working_directory")?.as_str()?;

            let matches_dir = std::fs::canonicalize(wd)
                .map(|c| c == resolved_project)
                .unwrap_or(wd == project_dir);
            let recent = mtime_ms >= start_ms.saturating_sub(2000);
            let session_id = data["session_id"].as_str()?.to_string();

            Some((session_id, mtime_ms, matches_dir, recent))
        })
        .collect();

    scored.sort_by_key(|b| std::cmp::Reverse(b.1));

    scored
        .iter()
        .find(|(_, _, dir, recent)| *dir && *recent)
        .or_else(|| scored.iter().find(|(_, _, dir, _)| *dir))
        .or_else(|| scored.iter().find(|(_, _, _, recent)| *recent))
        .map(|(sid, _, _, _)| sid.clone())
}

pub fn discover_gemini_session(
    start_ms: u64,
    project_dir: &str,
    task_id: Option<&str>,
) -> Option<String> {
    let tmp_root = dirs::home_dir()?.join(".gemini").join("tmp");
    discover_gemini_session_in(&tmp_root, start_ms, project_dir, task_id)
}

pub fn discover_gemini_session_in(
    tmp_root: &Path,
    start_ms: u64,
    project_dir: &str,
    task_id: Option<&str>,
) -> Option<String> {
    let resolved_project =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    let task_marker = task_id.map(|id| format!("task: {id}"));
    let mut scored: Vec<(String, u64, bool, bool, bool)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(tmp_root) else {
        return None;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let proj_dir = entry.path();
        if !proj_dir.is_dir() {
            continue;
        }
        let root_file = proj_dir.join(".project_root");
        let chats_dir = proj_dir.join("chats");
        if !root_file.exists() || !chats_dir.is_dir() {
            continue;
        }

        let Ok(root) = std::fs::read_to_string(&root_file) else {
            continue;
        };
        let root = root.trim();
        let matches_dir = std::fs::canonicalize(root)
            .map(|c| c == resolved_project)
            .unwrap_or_else(|_| Path::new(root) == resolved_project);

        let Ok(chats) = std::fs::read_dir(&chats_dir) else {
            continue;
        };
        for chat in chats.filter_map(|e| e.ok()) {
            let path = chat.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !is_gemini_session_file_name(name) {
                continue;
            }
            let Ok(stat) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = stat.modified() else {
                continue;
            };
            let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) else {
                continue;
            };
            let mtime_ms = duration.as_millis() as u64;
            let recent = mtime_ms >= start_ms.saturating_sub(2000);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let matches_task = task_marker
                .as_ref()
                .map(|marker| raw.contains(marker))
                .unwrap_or(false);
            let Some(session_id) = extract_gemini_session_id(&raw) else {
                continue;
            };
            scored.push((session_id, mtime_ms, matches_dir, recent, matches_task));
        }
    }

    scored.sort_by_key(|b| std::cmp::Reverse(b.1));
    let task_match = scored
        .iter()
        .find(|(_, _, dir, recent, task)| *dir && *recent && *task)
        .or_else(|| scored.iter().find(|(_, _, dir, _, task)| *dir && *task))
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, _, recent, task)| *recent && *task)
        })
        .or_else(|| scored.iter().find(|(_, _, _, _, task)| *task))
        .map(|(sid, _, _, _, _)| sid.clone());

    if task_id.is_some() {
        return task_match;
    }

    task_match
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, dir, recent, _)| *dir && *recent)
                .map(|(sid, _, _, _, _)| sid.clone())
        })
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, dir, _, _)| *dir)
                .map(|(sid, _, _, _, _)| sid.clone())
        })
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, _, recent, _)| *recent)
                .map(|(sid, _, _, _, _)| sid.clone())
        })
}

/// Locate the project cwd recorded for a Gemini session UUID.
///
/// Gemini keys sessions per-cwd under `~/.gemini/tmp/<name>/chats/` with
/// filenames `session-<iso>-<first8>.json`. When `--resume <uuid>` runs
/// from a cwd whose folder doesn't contain that UUID, Gemini silently
/// forks a fresh session instead of erroring — which looks like a
/// successful resume at the daemon boundary but delivers the wrong
/// conversation context.
///
/// Returns the matched folder's `.project_root` path so the caller can
/// pin the child's cwd to what Gemini expects. `None` means no session
/// with this UUID is on disk — the caller should refuse the resume.
pub fn resolve_gemini_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    let tmp_root = dirs::home_dir()?.join(".gemini/tmp");
    resolve_gemini_session_cwd_in(&tmp_root, session_id)
}

/// Locate the cwd a Claude session was recorded in.
///
/// Scans `~/.claude/projects/<slug>/<uuid>.jsonl` plus every sibling
/// `~/.claude-*/projects/...` that looks like a Claude account root, then
/// reads the cwd out of the first JSONL line that carries one. Lets
/// agents resume each other across repo boundaries without the caller
/// having to hand-pass the right `project_dir`.
pub fn resolve_claude_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    resolve_claude_session_cwd_in(&home, session_id)
}

pub fn resolve_claude_session_cwd_in(
    home: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    let mut accounts: Vec<std::path::PathBuf> = vec![home.join(".claude")];
    if let Ok(entries) = std::fs::read_dir(home) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(".claude-")
                && !name.contains("shared")
                && e.path().join("projects").exists()
            {
                accounts.push(e.path());
            }
        }
    }
    for account in &accounts {
        let projects = account.join("projects");
        let Ok(slugs) = std::fs::read_dir(&projects) else {
            continue;
        };
        for slug in slugs.flatten() {
            let path = slug.path().join(&file_name);
            if !path.is_file() {
                continue;
            }
            if let Some(cwd) = extract_claude_cwd(&path) {
                return Some(cwd);
            }
        }
    }
    None
}

fn extract_claude_cwd(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(f);
    for line in reader.lines().take(20).map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && let Some(cwd) = v.get("cwd").and_then(|c| c.as_str())
            && !cwd.is_empty()
        {
            return Some(std::path::PathBuf::from(cwd));
        }
    }
    None
}

/// Locate the cwd a Codex session was recorded in.
///
/// Codex stores sessions under `~/.codex/sessions/YYYY/MM/DD/
/// rollout-<iso>-<uuid>.jsonl` — cwd-independent storage, so the
/// resume itself would work from any working directory, but returning
/// the original cwd keeps tool resolution (path-based MCPs, git,
/// repo-local configs) correct after resurrection.
pub fn resolve_codex_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    let root = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"));
    resolve_codex_session_cwd_in(&root, session_id)
}

pub fn resolve_codex_session_cwd_in(
    codex_root: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let sessions = codex_root.join("sessions");
    let suffix = format!("-{session_id}.jsonl");
    let years = std::fs::read_dir(&sessions).ok()?;
    for year in years.flatten() {
        let Ok(months) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(days) = std::fs::read_dir(month.path()) else {
                continue;
            };
            for day in days.flatten() {
                let Ok(files) = std::fs::read_dir(day.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if !name.starts_with("rollout-") || !name.ends_with(&suffix) {
                        continue;
                    }
                    if let Some(cwd) = extract_codex_cwd(&file.path()) {
                        return Some(cwd);
                    }
                }
            }
        }
    }
    None
}

fn extract_codex_cwd(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    v.get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

pub fn resolve_copilot_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    if session_id.is_empty() || session_id == "pending" {
        return None;
    }
    let session_root = dirs::home_dir()?
        .join(".copilot")
        .join("session-state")
        .join(session_id);
    let events_path = session_root.join("events.jsonl");
    extract_copilot_cwd(&events_path)
}

fn extract_copilot_cwd(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(f);
    for line in reader.lines().take(8).map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).ok()?;
        if v["type"].as_str() != Some("session.start") {
            continue;
        }
        return v["data"]["context"]["cwd"]
            .as_str()
            .or_else(|| v["data"]["cwd"].as_str())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
    }
    None
}

pub fn resolve_vibe_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    if session_id.len() < 8 {
        return None;
    }
    let session_dir = std::env::var("VIBE_SESSION_DIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".vibe/logs/session")
        });
    let prefix = &session_id[..8];
    let needle = format!("_{prefix}");
    for entry in std::fs::read_dir(session_dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("session_") || !name.ends_with(&needle) {
            continue;
        }
        let meta_path = entry.path().join("meta.json");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(meta_path).ok()?).ok()?;
        if value["session_id"].as_str() != Some(session_id) {
            continue;
        }
        return value["environment"]["working_directory"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
    }
    None
}

/// Testable form of `resolve_gemini_session_cwd` — scoped to an explicit
/// Gemini tmp root so unit tests can build a fixture without touching
/// `~/.gemini`.
pub fn resolve_gemini_session_cwd_in(
    tmp_root: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    if session_id.len() < 8 {
        return None;
    }
    let first8 = &session_id[..8];

    for entry in std::fs::read_dir(tmp_root).ok()?.flatten() {
        let chats = entry.path().join("chats");
        let Ok(chat_entries) = std::fs::read_dir(&chats) else {
            continue;
        };
        for chat in chat_entries.flatten() {
            let name = chat.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_gemini_session_file_for_id(name, first8) {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(chat.path()) else {
                continue;
            };
            if extract_gemini_session_id(&raw).as_deref() != Some(session_id) {
                continue;
            }

            let Ok(root) = std::fs::read_to_string(entry.path().join(".project_root")) else {
                continue;
            };
            let root = root.trim();
            if root.is_empty() {
                continue;
            }
            return Some(std::path::PathBuf::from(root));
        }
    }
    None
}

fn is_gemini_session_file_name(name: &str) -> bool {
    name.starts_with("session-") && (name.ends_with(".json") || name.ends_with(".jsonl"))
}

fn is_gemini_session_file_for_id(name: &str, first8: &str) -> bool {
    is_gemini_session_file_name(name)
        && (name.ends_with(&format!("-{first8}.json"))
            || name.ends_with(&format!("-{first8}.jsonl")))
}

fn extract_gemini_session_id(raw: &str) -> Option<String> {
    let mut values = serde_json::Deserializer::from_str(raw).into_iter::<Value>();
    let data = values.next()?.ok()?;
    data.get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

impl Provider {
    /// Locate the cwd a prior session was recorded in. Enables agents to resume
    /// across repo boundaries without hand-passing project_dir. Returns None
    /// when the provider has no cwd-aware session store or when the session
    /// can't be found locally.
    pub fn resolve_session_cwd(&self, session_id: &str) -> Option<std::path::PathBuf> {
        match self {
            Provider::Claude => resolve_claude_session_cwd(session_id),
            // Harness providers persist sessions in their own store
            // (~/.bro-harness), not the claude projects dir; no cwd-aware
            // discovery, so resume needs an explicit project_dir.
            Provider::Glm | Provider::Deepseek | Provider::Brodex | Provider::VibeBh => None,
            Provider::Inception => None,
            Provider::Codex => resolve_codex_session_cwd(session_id),
            Provider::Gemini => resolve_gemini_session_cwd(session_id),
            Provider::Copilot => resolve_copilot_session_cwd(session_id),
            Provider::Vibe => resolve_vibe_session_cwd(session_id),
            Provider::Workflow => None,
        }
    }
}
