use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::artifacts;
use crate::knowledge::{Approval, Knowledge, KnowledgeEntry, Status};
use crate::notes::{GapImpact, GapNoteView, Note, NoteKind, NoteResolution, Notes};
use crate::orchestration::{TaskStatus, TaskStore};
use crate::threads::{Thread, ThreadStatus, Threads};
use crate::whiteboards::{Phase, WhiteboardRegistry};

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
    /// Explicitly import JSON gap files from .bbox/gaps/inbox and the host spool
    #[serde(default)]
    pub import_gap_spool: Option<bool>,
    /// Include read-only grouped gap-note counts
    #[serde(default)]
    pub aggregate_gaps: Option<bool>,
    /// Check git commit trailers that claim gap-note close-outs
    #[serde(default)]
    pub check_gap_closeouts: Option<bool>,
    /// Optional git rev/range for check_gap_closeouts (default: HEAD)
    #[serde(default)]
    pub gap_commit_range: Option<String>,
}

// ── Aggregator ────────────────────────────────────────────────────

pub fn compute_inbox(
    kb: &Knowledge,
    threads: &Threads,
    notes: &Notes,
    task_store: &TaskStore,
    whiteboards: &WhiteboardRegistry,
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

    // 2. Gap notes — structured substrate gaps filed as followups
    let gap_notes = gap_notes(notes, project_filter.as_deref(), limit);
    if !gap_notes.is_empty() {
        out.push_str(&format!("## Gap notes ({})\n", gap_notes.len()));
        for gap in &gap_notes {
            let gap_kind = gap.gap_kind.as_deref().unwrap_or("gap");
            let project = project_leaf(gap.note.project.as_deref()).unwrap_or("-");
            let detail = gap
                .dedupe_key
                .as_deref()
                .or(gap.domain.as_deref())
                .or(gap.blocking_level.as_deref())
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {} [{} {}] {} — {}{}\n",
                gap.note.id,
                gap.impact.as_str(),
                gap_kind,
                project,
                truncate(&gap.title, 120),
                detail
            ));
        }
        out.push('\n');
    }

    let stale_gaps =
        stale_high_impact_gap_notes(notes, project_filter.as_deref(), stale_days, limit);
    if !stale_gaps.is_empty() {
        out.push_str(&format!(
            "## Stale high-impact gap notes ≥{}d ({})\n",
            stale_days,
            stale_gaps.len()
        ));
        for (gap, age) in &stale_gaps {
            let gap_kind = gap.gap_kind.as_deref().unwrap_or("gap");
            let project = project_leaf(gap.note.project.as_deref()).unwrap_or("-");
            out.push_str(&format!(
                "  {} [{} {}] {} — {} (open {}d)\n",
                gap.note.id,
                gap.impact.as_str(),
                gap_kind,
                project,
                truncate(&gap.title, 120),
                age
            ));
        }
        out.push('\n');
    }

    // 3. Followups — things deferred, still open
    let followups = followup_notes(notes, project_filter.as_deref(), limit);
    if !followups.is_empty() {
        out.push_str(&format!("## Followups ({})\n", followups.len()));
        for n in &followups {
            out.push_str(&format!("  {} — {}\n", n.id, truncate(&n.body, 120)));
        }
        out.push('\n');
    }

    let auto_digest = unresolved_notes_matching(
        notes,
        "Auto-digest candidate held for review",
        project_filter.as_deref(),
        limit,
    );
    if !auto_digest.is_empty() {
        out.push_str(&format!(
            "## Auto-digest entries held for review ({})\n",
            auto_digest.len()
        ));
        for n in &auto_digest {
            out.push_str(&format!("  {} — {}\n", n.id, truncate(&n.body, 120)));
        }
        out.push('\n');
    }

    let tier0 = unresolved_notes_matching(
        notes,
        "Tier-0 contradiction detected",
        project_filter.as_deref(),
        limit,
    );
    if !tier0.is_empty() {
        out.push_str(&format!("## Tier-0 contradictions ({})\n", tier0.len()));
        for n in &tier0 {
            out.push_str(&format!("  {} — {}\n", n.id, truncate(&n.body, 120)));
        }
        out.push('\n');
    }

    let eval_drift =
        unresolved_notes_matching(notes, "eval drift", project_filter.as_deref(), limit);
    if !eval_drift.is_empty() {
        out.push_str(&format!("## Eval drift alerts ({})\n", eval_drift.len()));
        for n in &eval_drift {
            out.push_str(&format!("  {} — {}\n", n.id, truncate(&n.body, 120)));
        }
        out.push('\n');
    }

    let boards = contradiction_boards_waiting(whiteboards, project_filter.as_deref(), limit);
    if !boards.is_empty() {
        out.push_str(&format!(
            "## Contradiction-review boards waiting on synthesis ({})\n",
            boards.len()
        ));
        for (id, topic, phase) in &boards {
            out.push_str(&format!(
                "  {} [{}] — {}\n",
                id,
                phase,
                truncate(topic, 120)
            ));
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
                out.push_str(&format!("  [{}] {}\n", a.kind.as_str(), a.path));
            }
            out.push('\n');
        }
    }

    // 4. Stale threads — still open/active past threshold
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

    // 5. Unverified knowledge (agent-inferred, awaiting review)
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

    // 6. Failed bro tasks (optional)
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

    if p.aggregate_gaps.unwrap_or(false) {
        let aggregate = render_gap_aggregates(notes, project_filter.as_deref());
        if !aggregate.is_empty() {
            out.push_str(&aggregate);
        }
    }

    if p.check_gap_closeouts.unwrap_or(false) {
        if let Some(project) = p.project.as_deref() {
            match crate::gap_closeout::render_git_closeout_check(
                notes,
                std::path::Path::new(project),
                p.gap_commit_range.as_deref(),
            ) {
                Ok(report) if !report.is_empty() => out.push_str(&report),
                Ok(_) => {}
                Err(err) => {
                    out.push_str("## Gap close-out checks\n");
                    out.push_str(&format!("  error — {err:#}\n\n"));
                }
            }
        } else {
            out.push_str("## Gap close-out checks\n");
            out.push_str("  error — project is required for git trailer checks\n\n");
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

fn followup_notes(notes: &Notes, project_filter: Option<&str>, limit: usize) -> Vec<NoteRow> {
    let mut rows: Vec<(String, NoteRow)> = notes
        .all()
        .iter()
        .filter(|n| n.kind == NoteKind::Followup)
        .filter(|n| n.resolution != NoteResolution::Addressed)
        .filter(|n| note_matches_project(n, project_filter))
        .filter(|n| GapNoteView::parse(n).is_none())
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

fn gap_notes<'a>(
    notes: &'a Notes,
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<GapNoteView<'a>> {
    let mut rows: Vec<GapNoteView<'a>> = notes
        .all()
        .iter()
        .filter(|n| n.resolution != NoteResolution::Addressed)
        .filter(|n| note_matches_project(n, project_filter))
        .filter_map(GapNoteView::parse)
        .collect();
    rows.sort_by(|a, b| {
        resolution_rank(a.note.resolution)
            .cmp(&resolution_rank(b.note.resolution))
            .then_with(|| b.impact.cmp(&a.impact))
            .then_with(|| b.note.created_at.cmp(&a.note.created_at))
    });
    rows.truncate(limit);
    rows
}

fn note_matches_project(note: &Note, project_filter: Option<&str>) -> bool {
    match project_filter {
        Some(pf) => note
            .project
            .as_deref()
            .map(|p| p.to_lowercase().contains(pf))
            .unwrap_or(false),
        None => true,
    }
}

fn resolution_rank(resolution: NoteResolution) -> u8 {
    match resolution {
        NoteResolution::Unresolved => 0,
        NoteResolution::Acknowledged => 1,
        NoteResolution::Addressed => 2,
    }
}

fn is_high_impact(impact: GapImpact) -> bool {
    matches!(impact, GapImpact::Critical | GapImpact::High)
}

fn stale_high_impact_gap_notes<'a>(
    notes: &'a Notes,
    project_filter: Option<&str>,
    stale_days: u64,
    limit: usize,
) -> Vec<(GapNoteView<'a>, u64)> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut rows: Vec<(GapNoteView<'a>, u64)> = notes
        .all()
        .iter()
        .filter(|n| n.resolution != NoteResolution::Addressed)
        .filter(|n| note_matches_project(n, project_filter))
        .filter_map(GapNoteView::parse)
        .filter(|gap| is_high_impact(gap.impact))
        .filter_map(|gap| {
            let age = iso_age_days(&gap.note.created_at, now_secs);
            (age >= stale_days).then_some((gap, age))
        })
        .collect();
    rows.sort_by(|a, b| b.0.impact.cmp(&a.0.impact).then_with(|| b.1.cmp(&a.1)));
    rows.truncate(limit);
    rows
}

#[derive(Default)]
struct GapBucket {
    unresolved: usize,
    acknowledged: usize,
    addressed: usize,
    oldest_open: Option<String>,
    newest_open: Option<String>,
}

impl GapBucket {
    fn add(&mut self, note: &Note) {
        match note.resolution {
            NoteResolution::Unresolved => self.unresolved += 1,
            NoteResolution::Acknowledged => self.acknowledged += 1,
            NoteResolution::Addressed => self.addressed += 1,
        }
        if note.resolution != NoteResolution::Addressed {
            if self
                .oldest_open
                .as_deref()
                .map(|oldest| note.created_at.as_str() < oldest)
                .unwrap_or(true)
            {
                self.oldest_open = Some(note.created_at.clone());
            }
            if self
                .newest_open
                .as_deref()
                .map(|newest| note.created_at.as_str() > newest)
                .unwrap_or(true)
            {
                self.newest_open = Some(note.created_at.clone());
            }
        }
    }
}

fn render_gap_aggregates(notes: &Notes, project_filter: Option<&str>) -> String {
    use std::collections::BTreeMap;

    let mut by_kind: BTreeMap<String, GapBucket> = BTreeMap::new();
    let mut by_domain: BTreeMap<String, GapBucket> = BTreeMap::new();
    let mut by_dedupe: BTreeMap<String, GapBucket> = BTreeMap::new();

    for note in notes
        .all()
        .iter()
        .filter(|n| note_matches_project(n, project_filter))
    {
        let Some(gap) = GapNoteView::parse(note) else {
            continue;
        };
        by_kind
            .entry(gap.gap_kind.clone().unwrap_or_else(|| "gap".into()))
            .or_default()
            .add(note);
        by_domain
            .entry(gap.domain.clone().unwrap_or_else(|| "-".into()))
            .or_default()
            .add(note);
        if let Some(key) = &gap.dedupe_key {
            by_dedupe.entry(key.clone()).or_default().add(note);
        }
    }

    if by_kind.is_empty() && by_domain.is_empty() && by_dedupe.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("## Gap aggregates\n");
    render_gap_bucket_group(&mut out, "gap_kind", &by_kind, false);
    render_gap_bucket_group(&mut out, "domain", &by_domain, false);
    render_gap_bucket_group(&mut out, "dedupe_key", &by_dedupe, true);
    out.push('\n');
    out
}

fn render_gap_bucket_group(
    out: &mut String,
    label: &str,
    buckets: &std::collections::BTreeMap<String, GapBucket>,
    only_repeated_open: bool,
) {
    let rows: Vec<_> = buckets
        .iter()
        .filter(|(_, bucket)| !only_repeated_open || bucket.unresolved + bucket.acknowledged >= 2)
        .collect();
    if rows.is_empty() {
        return;
    }

    out.push_str(&format!("### by {label}\n"));
    for (key, bucket) in rows {
        let oldest = bucket.oldest_open.as_deref().unwrap_or("-");
        let newest = bucket.newest_open.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "  {} — unresolved={} acknowledged={} addressed={} oldest_open={} newest_open={}\n",
            key, bucket.unresolved, bucket.acknowledged, bucket.addressed, oldest, newest
        ));
    }
}

fn unresolved_notes_matching(
    notes: &Notes,
    needle: &str,
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<NoteRow> {
    let needle = needle.to_lowercase();
    let mut rows: Vec<(String, NoteRow)> = notes
        .all()
        .iter()
        .filter(|n| n.resolution != NoteResolution::Addressed)
        .filter(|n| n.body.to_lowercase().contains(&needle))
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

fn contradiction_boards_waiting(
    whiteboards: &WhiteboardRegistry,
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for id in whiteboards.list_ids() {
        let Some(board) = whiteboards.get(&id) else {
            continue;
        };
        let board = board.read();
        if board.phase != Phase::Resolve {
            continue;
        }
        if !board.topic.to_lowercase().contains("contradiction review") {
            continue;
        }
        if let Some(pf) = project_filter {
            if !board.project.to_lowercase().contains(pf) {
                continue;
            }
        }
        rows.push((
            board.id.clone(),
            board.topic.clone(),
            board.phase.as_str().to_string(),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.truncate(limit);
    rows
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

    rows.sort_by_key(|b| std::cmp::Reverse(b.1));
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

    rows.sort_by_key(|b| std::cmp::Reverse(b.2));
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

fn project_leaf(project: Option<&str>) -> Option<&str> {
    project.and_then(|p| p.rsplit('/').find(|part| !part.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::LearnParams;
    use crate::notes::{NoteParams, NoteStore};
    use crate::threads::ThreadParams;
    use crate::whiteboards::Role;
    use tempfile::tempdir;

    fn empty_context(
        dir: &tempfile::TempDir,
    ) -> (Knowledge, Threads, TaskStore, WhiteboardRegistry) {
        (
            Knowledge::open(&dir.path().join("kb.json")).unwrap(),
            Threads::open(&dir.path().join("th.json")).unwrap(),
            TaskStore::new(),
            WhiteboardRegistry::new(),
        )
    }

    fn note(
        id: &str,
        kind: NoteKind,
        body: &str,
        project: Option<&str>,
        resolution: NoteResolution,
        created_at: &str,
    ) -> Note {
        Note {
            id: id.into(),
            kind,
            body: body.into(),
            task_id: None,
            session_id: None,
            project: project.map(ToOwned::to_owned),
            thread_id: None,
            provider: None,
            bro: None,
            resolution,
            created_at: created_at.into(),
            updated_at: created_at.into(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    fn gap_body(
        title: &str,
        impact: &str,
        gap_kind: &str,
        domain: &str,
        dedupe_key: &str,
    ) -> String {
        serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": title,
            "impact": impact,
            "gap_kind": gap_kind,
            "domain": domain,
            "dedupe_key": dedupe_key
        })
        .to_string()
    }

    fn open_notes_with(dir: &tempfile::TempDir, stored_notes: Vec<Note>) -> Notes {
        let path = dir.path().join("notes.json");
        std::fs::write(
            &path,
            serde_json::to_string(&NoteStore {
                version: 1,
                notes: stored_notes,
            })
            .unwrap(),
        )
        .unwrap();
        Notes::open(&path).unwrap()
    }

    #[test]
    fn inbox_clean_plate_when_nothing_pending() {
        let dir = tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let task_store = TaskStore::new();
        let whiteboards = WhiteboardRegistry::new();

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &whiteboards,
            &InboxParams {
                project: None,
                limit: None,
                stale_days: None,
                include_tasks: None,
                import_gap_spool: None,
                aggregate_gaps: None,
                check_gap_closeouts: None,
                gap_commit_range: None,
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
        let whiteboards = WhiteboardRegistry::new();

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
        notes
            .create(&NoteParams {
                kind: "followup".into(),
                body: "Auto-digest candidate held for review: durable note".into(),
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
                kind: "surprise".into(),
                body: "Tier-0 contradiction detected between knowledge:a and knowledge:b".into(),
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
                body: "eval drift alert: drift_minor on nightly suite".into(),
                session_id: None,
                project: None,
                task_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        whiteboards
            .open(
                "board-1",
                "Contradiction review: knowledge:a vs knowledge:b",
                "/repo/x",
                None,
                "operator",
            )
            .unwrap();
        whiteboards
            .register("board-1", "operator", Role::Operator, "operator")
            .unwrap();
        whiteboards
            .transition("board-1", "operator", Phase::Read, None)
            .unwrap();
        whiteboards
            .transition("board-1", "operator", Phase::Debate, None)
            .unwrap();
        whiteboards
            .transition("board-1", "operator", Phase::Resolve, None)
            .unwrap();

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &whiteboards,
            &InboxParams {
                project: None,
                limit: None,
                stale_days: Some(0), // any open thread counts as stale
                include_tasks: None,
                import_gap_spool: None,
                aggregate_gaps: None,
                check_gap_closeouts: None,
                gap_commit_range: None,
            },
        )
        .unwrap();

        assert!(out.contains("## Unresolved"));
        assert!(out.contains("brief assumes invariant X"));
        assert!(out.contains("## Followups"));
        assert!(out.contains("add tests for the cycle detector"));
        assert!(out.contains("## Auto-digest entries held for review"));
        assert!(out.contains("## Tier-0 contradictions"));
        assert!(out.contains("## Eval drift alerts"));
        assert!(out.contains("## Contradiction-review boards waiting on synthesis"));
        assert!(out.contains("## Stale threads"));
        assert!(out.contains("reviewing ingestion"));
        assert!(out.contains("## Unverified knowledge"));
    }

    #[test]
    fn inbox_surfaces_gap_notes_once_before_followups() {
        let dir = tempdir().unwrap();
        let (kb, threads, task_store, whiteboards) = empty_context(&dir);
        let notes = open_notes_with(
            &dir,
            vec![
                note(
                    "note-00000001",
                    NoteKind::Followup,
                    &gap_body(
                        "Packet AST cannot express rate predicates",
                        "high",
                        "packet_ast",
                        "review-policy",
                        "packet_ast/review-policy/rate-window-predicate",
                    ),
                    Some("/repo/transcript-search"),
                    NoteResolution::Unresolved,
                    "2026-05-12T10:00:00Z",
                ),
                note(
                    "note-00000002",
                    NoteKind::Followup,
                    "add tests for the cycle detector",
                    Some("/repo/transcript-search"),
                    NoteResolution::Unresolved,
                    "2026-05-12T11:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &whiteboards,
            &InboxParams {
                project: Some("transcript-search".into()),
                limit: None,
                stale_days: None,
                include_tasks: None,
                import_gap_spool: None,
                aggregate_gaps: None,
                check_gap_closeouts: None,
                gap_commit_range: None,
            },
        )
        .unwrap();

        let gap_idx = out.find("## Gap notes").unwrap();
        let followup_idx = out.find("## Followups").unwrap();
        assert!(gap_idx < followup_idx);
        assert!(out.contains("note-00000001 [high packet_ast] transcript-search"));
        assert!(out.contains("Packet AST cannot express rate predicates"));
        assert!(out.contains("note-00000002"));
        assert!(!out.contains("note-00000001 —"));
    }

    #[test]
    fn inbox_gap_notes_honor_resolution_project_filter_and_ordering() {
        let dir = tempdir().unwrap();
        let (kb, threads, task_store, whiteboards) = empty_context(&dir);
        let notes = open_notes_with(
            &dir,
            vec![
                note(
                    "note-00000001",
                    NoteKind::Followup,
                    &gap_body(
                        "old low unresolved",
                        "low",
                        "gap",
                        "workflow",
                        "gap/old-low",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Unresolved,
                    "2026-05-12T08:00:00Z",
                ),
                note(
                    "note-00000002",
                    NoteKind::Followup,
                    &gap_body(
                        "new high unresolved",
                        "high",
                        "packet_ast",
                        "packets",
                        "gap/new-high",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Unresolved,
                    "2026-05-12T07:00:00Z",
                ),
                note(
                    "note-00000003",
                    NoteKind::Followup,
                    &gap_body(
                        "critical acknowledged",
                        "critical",
                        "gap",
                        "workflow",
                        "gap/ack",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Acknowledged,
                    "2026-05-12T12:00:00Z",
                ),
                note(
                    "note-00000004",
                    NoteKind::Followup,
                    &gap_body(
                        "critical addressed",
                        "critical",
                        "gap",
                        "workflow",
                        "gap/addressed",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Addressed,
                    "2026-05-12T13:00:00Z",
                ),
                note(
                    "note-00000005",
                    NoteKind::Followup,
                    &gap_body("other project", "critical", "gap", "workflow", "gap/other"),
                    Some("/repo/y"),
                    NoteResolution::Unresolved,
                    "2026-05-12T14:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &whiteboards,
            &InboxParams {
                project: Some("/repo/x".into()),
                limit: Some(10),
                stale_days: None,
                include_tasks: Some(false),
                import_gap_spool: None,
                aggregate_gaps: None,
                check_gap_closeouts: None,
                gap_commit_range: None,
            },
        )
        .unwrap();

        let high = out.find("note-00000002").unwrap();
        let low = out.find("note-00000001").unwrap();
        let ack = out.find("note-00000003").unwrap();
        assert!(high < low);
        assert!(low < ack, "acknowledged notes sort after unresolved notes");
        assert!(!out.contains("note-00000004"));
        assert!(!out.contains("note-00000005"));
    }

    #[test]
    fn inbox_reports_stale_high_impact_gap_notes() {
        let dir = tempdir().unwrap();
        let (kb, threads, task_store, whiteboards) = empty_context(&dir);
        let notes = open_notes_with(
            &dir,
            vec![
                note(
                    "note-00000001",
                    NoteKind::Followup,
                    &gap_body(
                        "old high",
                        "high",
                        "workflow",
                        "orchestration",
                        "gap/old-high",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Unresolved,
                    "2020-01-01T00:00:00Z",
                ),
                note(
                    "note-00000002",
                    NoteKind::Followup,
                    &gap_body("old low", "low", "workflow", "orchestration", "gap/old-low"),
                    Some("/repo/x"),
                    NoteResolution::Unresolved,
                    "2020-01-01T00:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &whiteboards,
            &InboxParams {
                project: Some("/repo/x".into()),
                limit: None,
                stale_days: Some(1),
                include_tasks: Some(false),
                import_gap_spool: None,
                aggregate_gaps: None,
                check_gap_closeouts: None,
                gap_commit_range: None,
            },
        )
        .unwrap();

        assert!(out.contains("## Stale high-impact gap notes"));
        assert!(out.contains("note-00000001"));
        assert!(!out.contains("note-00000002 [low workflow] x — old low (open"));
    }

    #[test]
    fn inbox_can_render_gap_aggregates() {
        let dir = tempdir().unwrap();
        let (kb, threads, task_store, whiteboards) = empty_context(&dir);
        let notes = open_notes_with(
            &dir,
            vec![
                note(
                    "note-00000001",
                    NoteKind::Followup,
                    &gap_body(
                        "first",
                        "high",
                        "packet_ast",
                        "review",
                        "packet/review/rate",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Unresolved,
                    "2026-01-01T00:00:00Z",
                ),
                note(
                    "note-00000002",
                    NoteKind::Followup,
                    &gap_body(
                        "second",
                        "medium",
                        "packet_ast",
                        "review",
                        "packet/review/rate",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Acknowledged,
                    "2026-02-01T00:00:00Z",
                ),
                note(
                    "note-00000003",
                    NoteKind::Followup,
                    &gap_body(
                        "closed",
                        "medium",
                        "workflow",
                        "dispatch",
                        "workflow/dispatch",
                    ),
                    Some("/repo/x"),
                    NoteResolution::Addressed,
                    "2026-03-01T00:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &task_store,
            &whiteboards,
            &InboxParams {
                project: Some("/repo/x".into()),
                limit: None,
                stale_days: None,
                include_tasks: Some(false),
                import_gap_spool: None,
                aggregate_gaps: Some(true),
                check_gap_closeouts: None,
                gap_commit_range: None,
            },
        )
        .unwrap();

        assert!(out.contains("## Gap aggregates"));
        assert!(out.contains("packet_ast — unresolved=1 acknowledged=1 addressed=0"));
        assert!(out.contains("review — unresolved=1 acknowledged=1 addressed=0"));
        assert!(out.contains("packet/review/rate — unresolved=1 acknowledged=1 addressed=0"));
        assert!(out.contains("workflow — unresolved=0 acknowledged=0 addressed=1"));
    }
}

fn thread_age_days(thread: &Thread, now_secs: u64) -> u64 {
    iso_age_days(&thread.last_activity, now_secs)
}

fn iso_age_days(ts: &str, now_secs: u64) -> u64 {
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
