//! Classifier companion ("intern") for the `bro fleet` cockpit.
//!
//! Experimental research vehicle (not yet core-harness functionality): when
//! `fleet.json` enables a classifier, every executor dispatched from the cockpit
//! gets a paired, hidden classifier session. The monitor below watches the
//! executor's transcript, and on each turn-end hands the new activity to the
//! classifier and reads back a `PASS` / `SUGGEST:` verdict. Suggestions are
//! surfaced in the TUI for operator visibility; when `auto_send` is on they're
//! also relayed into the executor as `[INTERN]`-prefixed user turns.
//!
//! The architecture mirrors the orchestration-layer supervision tiers
//! (`design/orchestration/supervision/*`): the executor stream is the
//! mechanical tap, this monitor is the cheap observe-only classifier, and the
//! "action" is the lowest-stakes one there is — inject an advisory turn. Voice
//! disambiguation + self-echo detection both ride the single `[INTERN]` prefix
//! (see `blackbox::fleet::intern_rider` and `DEFAULT_CLASSIFIER_PROMPT`).

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blackbox::fleet::{
    AgentHandle, CLASSIFIER_NAME_PREFIX, ClassifierConfig, DispatchSpec, FleetOrchestrator,
    INTERN_PREFIX, TaskStatus, TranscriptItem,
};

/// One surfaced classifier suggestion, sent from a monitor task to the TUI loop.
#[derive(Debug, Clone)]
pub struct ClassifierNote {
    /// The one-line suggestion text (without the `SUGGEST:` / `[INTERN]` tags).
    pub text: String,
    /// Whether it was also relayed into the executor (auto_send + executor idle).
    pub auto_sent: bool,
    #[allow(dead_code)]
    pub at_ms: u64,
}

/// Spawn a classifier companion for `executor` and the async monitor loop that
/// drives it. Fire-and-forget: the loop exits on its own when the executor goes
/// terminal or the session pipe breaks, stopping the companion behind it.
pub fn spawn_monitor(
    rt: &tokio::runtime::Handle,
    orch: Arc<FleetOrchestrator>,
    executor: AgentHandle,
    executor_name: String,
    cfg: ClassifierConfig,
    note_tx: mpsc::Sender<ClassifierNote>,
) -> AgentHandle {
    let cadence = Duration::from_secs(cfg.cadence_secs_resolved());
    let auto_send = cfg.auto_send_resolved();
    let min_activity = cfg.min_activity_resolved() as usize;

    // The companion is a normal bidi session, hidden from the roster by its
    // sentinel name, co-located with the executor's cwd.
    let mut spec = DispatchSpec::new(cfg.provider_resolved(), cfg.resolved_prompt());
    spec.model = cfg.model.clone();
    spec.effort = cfg.effort.clone();
    spec.cwd = executor.snapshot().cwd;
    spec.name = Some(format!("{CLASSIFIER_NAME_PREFIX}{executor_name}"));
    let classifier = orch.dispatch(spec);
    let classifier_for_monitor = classifier.clone();

    rt.spawn(async move {
        let mut executor_items_seen = 0usize;

        loop {
            tokio::time::sleep(cadence).await;

            let snap = executor.snapshot();
            if is_terminal(snap.status) {
                break;
            }
            let items = executor.transcript();
            let new = items.len().saturating_sub(executor_items_seen);
            // Digest mid-turn once enough new activity accrues, and flush the
            // remainder at turn-end. A long autonomous turn (hundreds of tool
            // calls) thus gets periodic check-ins instead of a single
            // end-of-turn look — gating on turn boundaries left the intern blind
            // for the whole turn.
            let enough = new >= min_activity || (!snap.turn_active && new > 0);
            if !enough {
                continue;
            }
            let delta = digest_items(&items[executor_items_seen..]);
            executor_items_seen = items.len();
            if delta.trim().is_empty() {
                continue;
            }

            // Hand the activity delta to the classifier and read its verdict.
            let seen_before = classifier_for_monitor.transcript().len();
            if classifier_for_monitor.send_user_turn(&delta).await.is_err() {
                break; // companion pipe gone
            }
            let Some(reply) = wait_for_reply(&classifier_for_monitor, seen_before).await else {
                continue;
            };
            let Some(suggestion) = parse_suggest(&reply) else {
                continue; // PASS / anything else → stay quiet
            };

            // No per-turn cooldown: rate is governed by min_activity batching,
            // the classifier's own response latency, and its PASS-by-default
            // calibration. One hint per (possibly 30-minute) turn was too stingy.

            // Relay regardless of whether the executor is mid-turn: a user turn
            // sent mid-turn QUEUES at the next turn boundary (both the Claude CLI
            // and bro-harness `session_loop` push it to `pending`) — it does not
            // interrupt. An earlier `!turn_active` gate here silently DROPPED the
            // suggestion whenever the executor was busy, so the intern produced
            // verdicts the executor never received.
            let mut auto_sent = false;
            if auto_send {
                let line = format!("{INTERN_PREFIX} {suggestion}");
                if executor.send_user_turn(&line).await.is_ok() {
                    auto_sent = true;
                }
            }

            if note_tx
                .send(ClassifierNote {
                    text: suggestion,
                    auto_sent,
                    at_ms: now_ms(),
                })
                .is_err()
            {
                break; // TUI gone
            }
        }

        // Stop the hidden companion when we stop watching.
        let _ = orch.stop(&classifier_for_monitor);
    });

    classifier
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

/// Poll the classifier transcript until a fresh assistant reply lands after our
/// steer (and its turn settles), or we give up. Bounded so a hung companion
/// can't wedge the monitor.
async fn wait_for_reply(classifier: &AgentHandle, seen_before: usize) -> Option<String> {
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snap = classifier.snapshot();
        let items = classifier.transcript();
        if items.len() > seen_before && !snap.turn_active {
            return items.iter().rev().find_map(|i| match i {
                TranscriptItem::AssistantText(t) if !t.trim().is_empty() => Some(t.clone()),
                _ => None,
            });
        }
    }
    None
}

/// Compact the executor's new transcript items into a bounded digest for the
/// classifier. Keeps the classifier's context cheap — one line per item, each
/// truncated.
fn digest_items(items: &[TranscriptItem]) -> String {
    let mut s = String::from("Executor activity since last check:\n");
    for it in items {
        match it {
            TranscriptItem::UserSteer(t) => {
                s.push_str(&format!("[user] {}\n", oneline(t)));
            }
            TranscriptItem::AssistantText(t) => {
                s.push_str(&format!("[assistant] {}\n", oneline(t)));
            }
            TranscriptItem::ToolCall { name, args } => {
                s.push_str(&format!("[tool_call] {} {}\n", name, oneline(args)));
            }
            TranscriptItem::ToolResult {
                tool,
                content,
                is_error,
                ..
            } => {
                s.push_str(&format!(
                    "[tool_result{}] {} -> {}\n",
                    if *is_error { " ERROR" } else { "" },
                    tool.as_deref().unwrap_or("?"),
                    oneline(content)
                ));
            }
            TranscriptItem::Report { message, .. } => {
                s.push_str(&format!("[report] {}\n", oneline(message)));
            }
            _ => {}
        }
    }
    s.push_str("\nReply PASS or SUGGEST: <one line>.");
    s
}

/// Collapse to a single bounded line for the digest.
fn oneline(t: &str) -> String {
    let flat = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 200 {
        let mut out: String = flat.chars().take(199).collect();
        out.push('…');
        out
    } else {
        flat
    }
}

/// Extract the suggestion from a classifier reply. Any `SUGGEST:` line (bare or
/// bolded) wins; everything else — including `PASS` — yields nothing.
fn parse_suggest(reply: &str) -> Option<String> {
    for raw in reply.lines() {
        let line = raw.trim().trim_start_matches('*').trim();
        if let Some(rest) = line.strip_prefix("SUGGEST:") {
            // Strip surrounding markdown emphasis (e.g. `**SUGGEST:** text**`).
            let s = rest.trim().trim_matches('*').trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_suggest_picks_suggestion_lines() {
        assert_eq!(parse_suggest("PASS"), None);
        assert_eq!(parse_suggest("pass, nothing here"), None);
        assert_eq!(
            parse_suggest("SUGGEST: try atom_search for this rename").as_deref(),
            Some("try atom_search for this rename")
        );
        // Bolded / prefixed reply still parses.
        assert_eq!(
            parse_suggest("Some reasoning...\n**SUGGEST:** use bbox_slice_move**").as_deref(),
            Some("use bbox_slice_move")
        );
        // Empty suggestion is not a suggestion.
        assert_eq!(parse_suggest("SUGGEST:   "), None);
    }

    #[test]
    fn oneline_bounds_length() {
        let long = "x ".repeat(500);
        let o = oneline(&long);
        assert!(o.chars().count() <= 200);
    }

    #[test]
    fn digest_labels_items() {
        let items = vec![
            TranscriptItem::AssistantText("on it".into()),
            TranscriptItem::ToolCall {
                name: "shell_run".into(),
                args: "{\"cmd\":\"grep foo\"}".into(),
            },
        ];
        let d = digest_items(&items);
        assert!(d.contains("[assistant] on it"));
        assert!(d.contains("[tool_call] shell_run"));
        assert!(d.contains("Reply PASS or SUGGEST:"));
    }
}
