use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::artifacts;
use crate::knowledge::{Approval, Knowledge, KnowledgeEntry, Status};
use crate::notes::{NoteKind, NoteResolution, Notes};
use crate::orchestration::{TaskStatus, TaskStore};
use crate::threads::{Thread, ThreadStatus, Threads};

// ── MCP parameter struct ──────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InboxParams {
    /// Filter to a project path substring
    #[serde(default)]
    pub project: Option<String>,
    /// Max rows per section (default: 10)
    #[serde(default)]
    pub limit: Option<u64>,
    /// Threads idle ≥ this many days are flagged stale (default: 7)
    #[serde(default)]
    pub stale_days: Option<u64>,
    /// Include failed bro tasks (default: true)
    #[serde(default)]
    pub include_tasks: Option<bool>,
}

// ── Aggregator ────────────────────────────────────────────────────

pub fn compute_inbox(
    kb: &Knowledge,
    threads: &Threads,
    notes: &Notes,
    task_store: &TaskStore,
    p: &InboxParams,
) -> Result<String> {
    let limit = p.limit.unwrap_or(10).max(1) as usize;
    let stale_days = p.stale_days.unwrap_or(7);
    let include_tasks = p.include_tasks.unwrap_or(true);
    let project_filter = p.project.as_deref().map(|s| s.to_lowercase());

    let mut out = String::new();
    out.push_str("# Inbox\n\n");

    // 1. Unresolved urgent notes: disputes, blocked, surprises
    let urgent = unresolved_notes_of(
        notes,
        &[NoteKind::Dispute, NoteKind::Blocked, NoteKind::Surprise],
        project_filter.as_deref(),
        limit,
    );
    if !urgent.is_empty() {
        out.push_str(&format!("## Unresolved ({})\n", urgent.len()));
        for n in &urgent {
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                n.kind,
                n.id,
                truncate(&n.body, 120)
            ));
        }
        out.push('\n');
    }

    // 2. Followups — things deferred, still open
    let followups = unresolved_notes_of(
        notes,
        &[NoteKind::Followup],
        project_filter.as_deref(),
        limit,
    );
    if !followups.is_empty() {
        out.push_str(&format!("## Followups ({})\n", followups.len()));
        for n in &followups {
            out.push_str(&format!("  {} — {}\n", n.id, truncate(&n.body, 120)));
        }
        out.push('\n');
    }

    if let Some(project) = p.project.as_deref() {
        let discovered = artifacts::discover_project_artifacts(std::path::Path::new(project))?;
        if !discovered.is_empty() {
            out.push_str(&format!(
                "## Discovered .bbox artifacts ({})\n",
                discovered.len()
            ));
            for a in discovered.iter().take(limit) {
                out.push_str(&format!("  [{}] {}\n", a.kind, a.path));
            }
            out.push('\n');
        }
    }

    // 3. Stale threads — still open/active past threshold
    let stale = stale_threads(threads, stale_days, project_filter.as_deref(), limit);
    if !stale.is_empty() {
        out.push_str(&format!(
            "## Stale threads ≥{}d ({})\n",
            stale_days,
            stale.len()
        ));
        for (t, age) in &stale {
            let name = t.name.as_deref().unwrap_or("-");
            out.push_str(&format!(
                "  {} ({}) — {}d — {}\n",
                t.id,
                name,
                age,
                truncate(&t.topic, 100)
            ));
        }
        out.push('\n');
    }

    // 4. Unverified knowledge (agent-inferred, awaiting review)
    let unverified = unverified_knowledge(kb, project_filter.as_deref(), limit);
    if !unverified.is_empty() {
        out.push_str(&format!("## Unverified knowledge ({})\n", unverified.len()));
        for e in &unverified {
            out.push_str(&format!(
                "  {} [{:?}] — {}\n",
                e.id,
                e.approval,
                truncate(&e.title, 100)
            ));
        }
        out.push('\n');
    }

    // 5. Failed bro tasks (optional)
    if include_tasks {
        let failed = failed_tasks(task_store, limit);
        if !failed.is_empty() {
            out.push_str(&format!("## Failed tasks ({})\n", failed.len()));
            for (id, provider, started_at) in &failed {
                out.push_str(&format!(
                    "  {} ({}) — started {}\n",
                    id, provider, started_at
                ));
            }
            out.push('\n');
        }
    }

    if out.trim_end() == "# Inbox" {
        out.push_str("_nothing needs attention — clean plate._\n");
    }

    Ok(out)
}

// ── Helpers ───────────────────────────────────────────────────────

struct NoteRow {
    id: String,
    kind: String,
    body: String,
}

fn unresolved_notes_of(
    notes: &Notes,
    kinds: &[NoteKind],
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<NoteRow> {
    let mut rows: Vec<(String, NoteRow)> = notes
        .all()
        .iter()
        .filter(|n| kinds.contains(&n.kind))
        .filter(|n| n.resolution != NoteResolution::Addressed)
        .filter(|n| match project_filter {
            Some(pf) => n
                .project
                .as_deref()
                .map(|p| p.to_lowercase().contains(pf))
                .unwrap_or(false),
            None => true,
        })
        .map(|n| {
            (
                n.created_at.clone(),
                NoteRow {
                    id: n.id.clone(),
                    kind: n.kind.as_ref().to_string(),
                    body: n.body.clone(),
                },
            )
        })
        .collect();

    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().take(limit).map(|(_, r)| r).collect()
}

fn stale_threads<'a>(
    threads: &'a Threads,
    stale_days: u64,
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<(&'a Thread, u64)> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut rows: Vec<(&Thread, u64)> = threads
        .all()
        .iter()
        .filter(|t| matches!(t.status, ThreadStatus::Open | ThreadStatus::Active))
        .filter(|t| match project_filter {
            Some(pf) => t.project.to_lowercase().contains(pf),
            None => true,
        })
        .map(|t| (t, thread_age_days(t, now_secs)))
        .filter(|(_, age)| *age >= stale_days)
        .collect();

    rows.sort_by(|a, b| b.1.cmp(&a.1));
    rows.truncate(limit);
    rows
}

fn unverified_knowledge<'a>(
    kb: &'a Knowledge,
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<&'a KnowledgeEntry> {
    let mut rows: Vec<&KnowledgeEntry> = kb
        .all_entries()
        .iter()
        .filter(|e| e.status == Status::Active)
        .filter(|e| matches!(e.approval, Approval::AgentInferred | Approval::Imported))
        .filter(|e| match project_filter {
            Some(pf) => e
                .project
                .as_deref()
                .map(|p| p.to_lowercase().contains(pf))
                .unwrap_or(false),
            None => true,
        })
        .collect();

    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    rows.truncate(limit);
    rows
}

fn failed_tasks(task_store: &TaskStore, limit: usize) -> Vec<(String, String, u64)> {
    let mut rows: Vec<(String, String, u64)> = task_store
        .all_tasks()
        .iter()
        .filter_map(|t| {
            let inner = t.inner.lock();
            if inner.status == TaskStatus::Failed {
                Some((
                    inner.id.clone(),
                    format!("{:?}", inner.provider),
                    inner.started_at,
                ))
            } else {
                None
            }
        })
        .collect();

    rows.sort_by(|a, b| b.2.cmp(&a.2));
    rows.truncate(limit);
    rows
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::LearnParams;
    use crate::notes::NoteParams;
    use crate::threads::ThreadParams;
    use tempfile::tempdir;

    #[test]
    fn inbox_clean_plate_when_nothing_pending() {
        let dir = tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let task_store = TaskStore::new();

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &InboxParams {
                project: None,
                limit: None,
                stale_days: None,
                include_tasks: None,
            },
        )
        .unwrap();
        assert!(out.contains("clean plate"));
    }

    #[test]
    fn inbox_surfaces_mixed_signals() {
        let dir = tempdir().unwrap();
        let mut kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let mut threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let mut notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let task_store = TaskStore::new();

        // Agent-inferred knowledge → should appear in "Unverified"
        kb.learn(
            &LearnParams {
                content: "always use bbox_note".into(),
                category: "convention".into(),
                format: None,
                title: Some("note habit".into()),
                scope: None,
                project: None,
                providers: None,
                priority: None,
                weight: None,
                expires_at: None,
                cluster: None,
                id: None,
            },
            true,
        )
        .unwrap();

        // Open a thread, then antedate it to force staleness
        threads
            .thread(&ThreadParams {
                action: "open".into(),
                topic: Some("reviewing ingestion".into()),
                project: Some("/repo/x".into()),
                name: Some("review".into()),
                id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: None,
            })
            .unwrap();

        // Notes: dispute + followup
        notes
            .create(&NoteParams {
                kind: "dispute".into(),
                body: "brief assumes invariant X".into(),
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        notes
            .create(&NoteParams {
                kind: "followup".into(),
                body: "add tests for the cycle detector".into(),
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &InboxParams {
                project: None,
                limit: None,
                stale_days: Some(0), // any open thread counts as stale
                include_tasks: None,
            },
        )
        .unwrap();

        assert!(out.contains("## Unresolved"));
        assert!(out.contains("brief assumes invariant X"));
        assert!(out.contains("## Followups"));
        assert!(out.contains("add tests for the cycle detector"));
        assert!(out.contains("## Stale threads"));
        assert!(out.contains("reviewing ingestion"));
        assert!(out.contains("## Unverified knowledge"));
    }
}

fn thread_age_days(thread: &Thread, now_secs: u64) -> u64 {
    let ts = &thread.last_activity;
    if ts.len() < 10 {
        return 0;
    }
    let y: i64 = ts[0..4].parse().unwrap_or(2026);
    let m: u32 = ts[5..7].parse().unwrap_or(1);
    let d: u32 = ts[8..10].parse().unwrap_or(1);

    let mut epoch_days: i64 = 0;
    for yr in 1970..y {
        epoch_days += if yr % 4 == 0 && (yr % 100 != 0 || yr % 400 == 0) {
            366
        } else {
            365
        };
    }
    let months = [
        31,
        if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            29
        } else {
            28
        },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for days in months.iter().take((m as usize - 1).min(11)) {
        epoch_days += *days as i64;
    }
    epoch_days += d as i64 - 1;

    let activity_secs = epoch_days as u64 * 86400;
    now_secs.saturating_sub(activity_secs) / 86400
}
