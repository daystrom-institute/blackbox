use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use bbox_gaps::gaps::{GapImpact, GapNote, GapResolution, GapStore};
use bbox_knowledge::knowledge::{Approval, Knowledge, KnowledgeEntry, Status};
use bbox_threads::notes::{Note, NoteKind, NoteResolution, Notes};
use bbox_threads::threads::{Thread, ThreadStatus, Threads};

// ── MCP parameter struct ──────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InboxParams {
    /// Filter to a project path substring
    #[serde(default)]
    pub project: Option<String>,
    /// Knowledge visibility policy: published, own, or all.
    #[serde(default)]
    pub provisional: Option<String>,
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

/// One vector partition whose HNSW connectivity has degraded past the
/// notify threshold — live vector-recall risk (gap-1168b0bd). Plain rows
/// like `failed_task_rows`: the vector store sits outside this crate's
/// DAG, so the adapter in the daemon's attention tool builds these from
/// partition metrics.
#[derive(Debug, Clone)]
pub enum VectorConnectivityAlert {
    Breach {
        route: String,
        active_nodes: usize,
        zero_in_degree_nodes: usize,
        risk_ratio: f32,
    },
    DiagnosticsUnavailable {
        route: String,
        reason: String,
    },
}

/// Conversation producer silence: the "satellite quietly died" class. A
/// granted conversation scope is supposed to have a live producer polling it,
/// and one that stops calling lands nothing further while every durable
/// surface still reads healthy. Plain rows like `failed_task_rows`: the
/// presence state is daemon-side and in memory, so the attention tool builds
/// these rows from the contact map.
#[derive(Debug, Clone)]
pub enum ConversationProducerSilence {
    /// A scope with authenticated contact, but none inside the window.
    Stale {
        scope: String,
        last_seen_at: String,
        silent_minutes: u64,
    },
    /// A granted scope with no authenticated contact since daemon boot.
    NeverSeen { scope: String },
}

pub fn compute_inbox(
    kb: &Knowledge,
    threads: &Threads,
    notes: &Notes,
    gaps: &GapStore,
    failed_task_rows: &[(String, String, u64)],
    vector_alerts: &[VectorConnectivityAlert],
    conversation_silence: &[ConversationProducerSilence],
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

    // 1b. Vector connectivity risk — host-level search-recall degradation
    // (gap-1168b0bd). Not project-filtered: orphaned vectors degrade
    // retrieval for every project on the host.
    if !vector_alerts.is_empty() {
        out.push_str(&format!(
            "## Vector connectivity risk ({})\n",
            vector_alerts.len()
        ));
        for alert in vector_alerts {
            match alert {
                VectorConnectivityAlert::Breach {
                    route,
                    active_nodes,
                    zero_in_degree_nodes,
                    risk_ratio,
                } => out.push_str(&format!(
                    "  {route} — {:.2}% of {active_nodes} active vectors unreachable ({zero_in_degree_nodes} zero-in-degree); daily connectivity maintenance will attempt repair; use bbox_embed_status for current diagnostics\n",
                    risk_ratio * 100.0,
                )),
                VectorConnectivityAlert::DiagnosticsUnavailable { route, reason } => {
                    out.push_str(&format!(
                        "  {route} — connectivity diagnostics unavailable ({reason}); health is unknown, not healthy\n"
                    ));
                }
            }
        }
        out.push('\n');
    }

    // 1d. Conversation producer silence: an ingestion lane whose satellite
    // stopped calling the corpus wire. Not project-filtered: the scope to
    // project mapping is catalog state the presence map deliberately does
    // not hold, and a silent lane degrades whatever project it feeds.
    if !conversation_silence.is_empty() {
        out.push_str(&format!(
            "## Conversation producer silence ({})\n",
            conversation_silence.len()
        ));
        for alert in conversation_silence {
            match alert {
                ConversationProducerSilence::Stale {
                    scope,
                    last_seen_at,
                    silent_minutes,
                } => out.push_str(&format!(
                    "  {scope} - last producer contact {last_seen_at}, silent for \
                     {silent_minutes}m; the satellite stopped calling the corpus wire\n"
                )),
                ConversationProducerSilence::NeverSeen { scope } => out.push_str(&format!(
                    "  {scope} - no producer contact since boot; the satellite has never \
                     called the corpus wire\n"
                )),
            }
        }
        out.push('\n');
    }

    // 2. Gap notes — first-class substrate gaps from the repo-owned gap store
    let gaps_open = open_gaps(gaps, project_filter.as_deref(), limit);
    if !gaps_open.is_empty() {
        out.push_str(&format!("## Gap notes ({})\n", gaps_open.len()));
        for gap in &gaps_open {
            let project = project_leaf(gap.project.as_deref()).unwrap_or("-");
            out.push_str(&format!(
                "  {} [{} {}] {} — {} ({})\n",
                gap.id,
                gap.impact.as_ref(),
                gap.gap_kind.as_ref(),
                project,
                truncate(&gap.title, 120),
                gap.dedupe_key,
            ));
        }
        out.push('\n');
    }

    let stale_gaps = stale_high_impact_gaps(gaps, project_filter.as_deref(), stale_days, limit);
    if !stale_gaps.is_empty() {
        out.push_str(&format!(
            "## Stale high-impact gap notes ≥{}d ({})\n",
            stale_days,
            stale_gaps.len()
        ));
        for (gap, age) in &stale_gaps {
            let project = project_leaf(gap.project.as_deref()).unwrap_or("-");
            out.push_str(&format!(
                "  {} [{} {}] {} — {} (open {}d)\n",
                gap.id,
                gap.impact.as_ref(),
                gap.gap_kind.as_ref(),
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
        let failed = failed_tasks(failed_task_rows, limit);
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
        let aggregate = render_gap_aggregates(gaps, project_filter.as_deref());
        if !aggregate.is_empty() {
            out.push_str(&aggregate);
        }
    }

    if p.check_gap_closeouts.unwrap_or(false) {
        if let Some(project) = p.project.as_deref() {
            match bbox_gaps::gap_closeout::render_git_closeout_check(
                gaps,
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
        // Historical gap-bodied followups (pre-`bbox_gap` era) stay hidden from
        // the followups view — they're historical exhaust, surfaced (if at all)
        // only through their new home in the gap store.
        .filter(|n| !is_legacy_gap_body(&n.body))
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

/// Cheap type-sniff for a legacy `blackbox.gap_note.v1` JSON body, used only to
/// keep pre-migration gap rows out of the followups view. Not a full parse.
fn is_legacy_gap_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .as_ref()
        .and_then(|v| v.get("type"))
        .and_then(|t| t.as_str())
        == Some(bbox_gaps::gaps::GAP_NOTE_TYPE)
}

fn open_gaps<'a>(
    gaps: &'a GapStore,
    project_filter: Option<&str>,
    limit: usize,
) -> Vec<&'a GapNote> {
    let mut rows: Vec<&GapNote> = gaps
        .all()
        .iter()
        .filter(|g| g.resolution != GapResolution::Addressed)
        .filter(|g| gap_matches_project(g, project_filter))
        .collect();
    rows.sort_by(|a, b| {
        gap_resolution_rank(a.resolution)
            .cmp(&gap_resolution_rank(b.resolution))
            .then_with(|| b.impact.cmp(&a.impact))
            .then_with(|| b.created_at.cmp(&a.created_at))
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

fn gap_matches_project(gap: &GapNote, project_filter: Option<&str>) -> bool {
    match project_filter {
        Some(pf) => gap
            .project
            .as_deref()
            .map(|p| p.to_lowercase().contains(pf))
            .unwrap_or(false),
        None => true,
    }
}

fn gap_resolution_rank(resolution: GapResolution) -> u8 {
    match resolution {
        GapResolution::Unresolved => 0,
        GapResolution::Acknowledged => 1,
        GapResolution::Addressed => 2,
    }
}

fn is_high_impact(impact: GapImpact) -> bool {
    matches!(impact, GapImpact::Critical | GapImpact::High)
}

fn stale_high_impact_gaps<'a>(
    gaps: &'a GapStore,
    project_filter: Option<&str>,
    stale_days: u64,
    limit: usize,
) -> Vec<(&'a GapNote, u64)> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut rows: Vec<(&GapNote, u64)> = gaps
        .all()
        .iter()
        .filter(|g| g.resolution != GapResolution::Addressed)
        .filter(|g| gap_matches_project(g, project_filter))
        .filter(|g| is_high_impact(g.impact))
        .filter_map(|gap| {
            let age = iso_age_days(&gap.created_at, now_secs);
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
    fn add(&mut self, gap: &GapNote) {
        match gap.resolution {
            GapResolution::Unresolved => self.unresolved += 1,
            GapResolution::Acknowledged => self.acknowledged += 1,
            GapResolution::Addressed => self.addressed += 1,
        }
        if gap.resolution != GapResolution::Addressed {
            if self
                .oldest_open
                .as_deref()
                .map(|oldest| gap.created_at.as_str() < oldest)
                .unwrap_or(true)
            {
                self.oldest_open = Some(gap.created_at.clone());
            }
            if self
                .newest_open
                .as_deref()
                .map(|newest| gap.created_at.as_str() > newest)
                .unwrap_or(true)
            {
                self.newest_open = Some(gap.created_at.clone());
            }
        }
    }
}

fn render_gap_aggregates(gaps: &GapStore, project_filter: Option<&str>) -> String {
    use std::collections::BTreeMap;

    let mut by_kind: BTreeMap<String, GapBucket> = BTreeMap::new();
    let mut by_domain: BTreeMap<String, GapBucket> = BTreeMap::new();
    let mut by_dedupe: BTreeMap<String, GapBucket> = BTreeMap::new();

    for gap in gaps
        .all()
        .iter()
        .filter(|g| gap_matches_project(g, project_filter))
    {
        by_kind
            .entry(gap.gap_kind.as_ref().to_string())
            .or_default()
            .add(gap);
        by_domain.entry(gap.domain.clone()).or_default().add(gap);
        by_dedupe
            .entry(gap.dedupe_key.clone())
            .or_default()
            .add(gap);
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

/// `rows` are (task_id, provider, started_at) tuples extracted by the
/// caller from its task store (dependency inversion: this module sits
/// below orchestration in the crate DAG).
fn failed_tasks(rows: &[(String, String, u64)], limit: usize) -> Vec<(String, String, u64)> {
    let mut rows = rows.to_vec();
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

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_knowledge::knowledge::LearnParams;
    use bbox_threads::notes::{NoteParams, NoteStore};
    use bbox_threads::threads::ThreadParams;
    use tempfile::tempdir;

    fn empty_context(dir: &tempfile::TempDir) -> (Knowledge, Threads) {
        (
            Knowledge::open(&dir.path().join("kb.json")).unwrap(),
            Threads::open(&dir.path().join("th.json")).unwrap(),
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
            project_id: None,
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

    #[allow(clippy::too_many_arguments)]
    fn gap(
        id: &str,
        title: &str,
        impact: GapImpact,
        gap_kind: bbox_gaps::gaps::GapKind,
        domain: &str,
        dedupe_key: &str,
        project: Option<&str>,
        resolution: GapResolution,
        created_at: &str,
    ) -> GapNote {
        GapNote {
            id: id.into(),
            title: title.into(),
            gap_kind,
            domain: domain.into(),
            wanted_capability: "x".into(),
            missing_primitive: None,
            fallback_used: None,
            evidence: Vec::new(),
            impact,
            blocking_level: bbox_gaps::gaps::BlockingLevel::WorkaroundAvailable,
            dedupe_key: dedupe_key.into(),
            suggested_owner: None,
            notes: None,
            supersedes: None,
            superseded_by: None,
            resolution,
            project: project.map(ToOwned::to_owned),
            project_id: None,
            write_dir: None,
            provisional_checkout_id: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: created_at.into(),
            updated_at: created_at.into(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    fn empty_gaps(dir: &tempfile::TempDir) -> GapStore {
        GapStore::open(&dir.path().join("gaps.json")).unwrap()
    }

    fn open_gaps_with(dir: &tempfile::TempDir, stored: Vec<GapNote>) -> GapStore {
        let path = dir.path().join("gaps.json");
        std::fs::write(
            &path,
            serde_json::to_string(&bbox_gaps::gaps::GapStoreData {
                version: 1,
                gaps: stored,
            })
            .unwrap(),
        )
        .unwrap();
        GapStore::open(&path).unwrap()
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
        let gaps = empty_gaps(&dir);

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &[],
            &[],
            &InboxParams {
                project: None,
                provisional: None,
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

    /// Connectivity alerts are host-level recall risk: they render their own
    /// section, survive a project filter, and defeat the clean plate.
    #[test]
    fn inbox_surfaces_vector_connectivity_alerts() {
        let dir = tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = empty_gaps(&dir);
        let alerts = vec![VectorConnectivityAlert::Breach {
            route: "voyage-1024".into(),
            active_nodes: 399_000,
            zero_in_degree_nodes: 12_000,
            risk_ratio: 0.0301,
        }];

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &alerts,
            &[],
            &InboxParams {
                project: Some("/repo/unrelated-project".into()),
                provisional: None,
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
        assert!(out.contains("## Vector connectivity risk (1)"));
        assert!(out.contains("voyage-1024"));
        assert!(out.contains("3.01%"));
        assert!(out.contains("daily connectivity maintenance"));
        assert!(!out.contains("embed-compaction-arc"));
        assert!(!out.contains("clean plate"));
    }

    #[test]
    fn inbox_does_not_treat_unavailable_vector_diagnostics_as_healthy() {
        let dir = tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = empty_gaps(&dir);
        let alerts = vec![VectorConnectivityAlert::DiagnosticsUnavailable {
            route: "voyage-1024".into(),
            reason: "deadline_exceeded".into(),
        }];

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &alerts,
            &[],
            &InboxParams {
                project: None,
                provisional: None,
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
        assert!(out.contains("diagnostics unavailable (deadline_exceeded)"));
        assert!(out.contains("health is unknown, not healthy"));
    }

    /// Silence rows are host-level ingestion risk: they render their own
    /// section, survive a project filter, and defeat the clean plate, because
    /// a satellite that stopped calling lands nothing further anywhere.
    #[test]
    fn inbox_surfaces_conversation_producer_silence() {
        let dir = tempdir().unwrap();
        let kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = empty_gaps(&dir);
        let silence = vec![
            ConversationProducerSilence::Stale {
                scope: "slack/csrc_fixture01".into(),
                last_seen_at: "2026-08-14T12:00:00Z".into(),
                silent_minutes: 47,
            },
            ConversationProducerSilence::NeverSeen {
                scope: "slack/csrc_fixture02".into(),
            },
        ];

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &[],
            &silence,
            &InboxParams {
                project: Some("/repo/unrelated-project".into()),
                provisional: None,
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
        assert!(out.contains("## Conversation producer silence (2)"));
        assert!(out.contains("slack/csrc_fixture01 - last producer contact 2026-08-14T12:00:00Z"));
        assert!(out.contains("silent for 47m"));
        assert!(out.contains("slack/csrc_fixture02 - no producer contact since boot"));
        assert!(!out.contains("clean plate"));
    }

    #[test]
    fn inbox_surfaces_mixed_signals() {
        let dir = tempdir().unwrap();
        let mut kb = Knowledge::open(&dir.path().join("kb.json")).unwrap();
        let mut threads = Threads::open(&dir.path().join("th.json")).unwrap();
        let mut notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = empty_gaps(&dir);

        // Agent-inferred knowledge → should appear in "Unverified"
        kb.learn(
            &LearnParams {
                content: "always use bbox_note".into(),
                category: "convention".into(),
                format: None,
                title: Some("note habit".into()),
                scope: None,
                project: None,
                project_id: None,
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
                project_id: None,
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
                origin: None,
            })
            .unwrap();

        // Notes: dispute + followup
        notes
            .create(&NoteParams {
                kind: "dispute".into(),
                body: "brief assumes invariant X".into(),
                session_id: None,
                project: None,
                project_id: None,
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
                project_id: None,
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
                project_id: None,
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
                project_id: None,
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
                project_id: None,
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
            &gaps,
            &[],
            &[],
            &[],
            &InboxParams {
                project: None,
                provisional: None,
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
        assert!(!out.contains("Contradiction-review boards"));
        assert!(out.contains("## Stale threads"));
        assert!(out.contains("reviewing ingestion"));
        assert!(out.contains("## Unverified knowledge"));
    }

    #[test]
    fn inbox_surfaces_gap_notes_before_followups() {
        let dir = tempdir().unwrap();
        let (kb, threads) = empty_context(&dir);
        let notes = open_notes_with(
            &dir,
            vec![note(
                "note-00000002",
                NoteKind::Followup,
                "add tests for the cycle detector",
                Some("/repo/transcript-search"),
                NoteResolution::Unresolved,
                "2026-05-12T11:00:00Z",
            )],
        );
        let gaps = open_gaps_with(
            &dir,
            vec![gap(
                "gap-00000001",
                "Packet AST cannot express rate predicates",
                GapImpact::High,
                bbox_gaps::gaps::GapKind::PacketAst,
                "review-policy",
                "packet_ast/review-policy/rate-window-predicate",
                Some("/repo/transcript-search"),
                GapResolution::Unresolved,
                "2026-05-12T10:00:00Z",
            )],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &[],
            &[],
            &InboxParams {
                project: Some("transcript-search".into()),
                provisional: None,
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
        assert!(out.contains("gap-00000001 [high packet_ast] transcript-search"));
        assert!(out.contains("Packet AST cannot express rate predicates"));
        assert!(out.contains("note-00000002"));
    }

    #[test]
    fn inbox_gap_notes_honor_resolution_project_filter_and_ordering() {
        use bbox_gaps::gaps::GapKind;
        let dir = tempdir().unwrap();
        let (kb, threads) = empty_context(&dir);
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = open_gaps_with(
            &dir,
            vec![
                gap(
                    "gap-00000001",
                    "old low unresolved",
                    GapImpact::Low,
                    GapKind::Workflow,
                    "workflow",
                    "workflow/wf/old-low",
                    Some("/repo/x"),
                    GapResolution::Unresolved,
                    "2026-05-12T08:00:00Z",
                ),
                gap(
                    "gap-00000002",
                    "new high unresolved",
                    GapImpact::High,
                    GapKind::PacketAst,
                    "packets",
                    "packet_ast/packets/new-high",
                    Some("/repo/x"),
                    GapResolution::Unresolved,
                    "2026-05-12T07:00:00Z",
                ),
                gap(
                    "gap-00000003",
                    "critical acknowledged",
                    GapImpact::Critical,
                    GapKind::Workflow,
                    "workflow",
                    "workflow/wf/ack",
                    Some("/repo/x"),
                    GapResolution::Acknowledged,
                    "2026-05-12T12:00:00Z",
                ),
                gap(
                    "gap-00000004",
                    "critical addressed",
                    GapImpact::Critical,
                    GapKind::Workflow,
                    "workflow",
                    "workflow/wf/addressed",
                    Some("/repo/x"),
                    GapResolution::Addressed,
                    "2026-05-12T13:00:00Z",
                ),
                gap(
                    "gap-00000005",
                    "other project",
                    GapImpact::Critical,
                    GapKind::Workflow,
                    "workflow",
                    "workflow/wf/other",
                    Some("/repo/y"),
                    GapResolution::Unresolved,
                    "2026-05-12T14:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &[],
            &[],
            &InboxParams {
                project: Some("/repo/x".into()),
                provisional: None,
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

        let high = out.find("gap-00000002").unwrap();
        let low = out.find("gap-00000001").unwrap();
        let ack = out.find("gap-00000003").unwrap();
        assert!(high < low);
        assert!(low < ack, "acknowledged gaps sort after unresolved gaps");
        assert!(!out.contains("gap-00000004"));
        assert!(!out.contains("gap-00000005"));
    }

    #[test]
    fn inbox_reports_stale_high_impact_gap_notes() {
        use bbox_gaps::gaps::GapKind;
        let dir = tempdir().unwrap();
        let (kb, threads) = empty_context(&dir);
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = open_gaps_with(
            &dir,
            vec![
                gap(
                    "gap-00000001",
                    "old high",
                    GapImpact::High,
                    GapKind::Workflow,
                    "orchestration",
                    "workflow/orchestration/old-high",
                    Some("/repo/x"),
                    GapResolution::Unresolved,
                    "2020-01-01T00:00:00Z",
                ),
                gap(
                    "gap-00000002",
                    "old low",
                    GapImpact::Low,
                    GapKind::Workflow,
                    "orchestration",
                    "workflow/orchestration/old-low",
                    Some("/repo/x"),
                    GapResolution::Unresolved,
                    "2020-01-01T00:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &[],
            &[],
            &InboxParams {
                project: Some("/repo/x".into()),
                provisional: None,
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
        assert!(out.contains("gap-00000001"));
        assert!(!out.contains("gap-00000002 [low workflow] x — old low (open"));
    }

    #[test]
    fn inbox_can_render_gap_aggregates() {
        use bbox_gaps::gaps::GapKind;
        let dir = tempdir().unwrap();
        let (kb, threads) = empty_context(&dir);
        let notes = Notes::open(&dir.path().join("notes.json")).unwrap();
        let gaps = open_gaps_with(
            &dir,
            vec![
                gap(
                    "gap-00000001",
                    "first",
                    GapImpact::High,
                    GapKind::PacketAst,
                    "review",
                    "packet/review/rate",
                    Some("/repo/x"),
                    GapResolution::Unresolved,
                    "2026-01-01T00:00:00Z",
                ),
                gap(
                    "gap-00000002",
                    "second",
                    GapImpact::Medium,
                    GapKind::PacketAst,
                    "review",
                    "packet/review/rate",
                    Some("/repo/x"),
                    GapResolution::Acknowledged,
                    "2026-02-01T00:00:00Z",
                ),
                gap(
                    "gap-00000003",
                    "closed",
                    GapImpact::Medium,
                    GapKind::Workflow,
                    "dispatch",
                    "workflow/dispatch",
                    Some("/repo/x"),
                    GapResolution::Addressed,
                    "2026-03-01T00:00:00Z",
                ),
            ],
        );

        let out = compute_inbox(
            &kb,
            &threads,
            &notes,
            &gaps,
            &[],
            &[],
            &[],
            &InboxParams {
                project: Some("/repo/x".into()),
                provisional: None,
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
