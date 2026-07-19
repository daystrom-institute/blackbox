//! Internal hook subsystem — named interception points on the agent loop that
//! observe turn state and contribute *ambient meta* (nudges) steering the model
//! toward the rich blackbox toolbox. Nudges never gate or remove tools.
//!
//! See design/bro-harness/bro-harness-hooks.md: the scaffold + gating ledger
//! + the two delivery mechanisms + the shipped rule set.
//!
//! Adoption is deliberately *not* tracked here. Whether a nudge was adopted (or
//! declined, with or without a gap note) is a retrospective query over the
//! indexed tool-call transcript corpus — which already logs every call and
//! contains the `<harness-note>` rider itself. See bro-harness-hooks.md §6.
//!
//! Shape (separation of concerns):
//! - A [`Hook`] is a pure matcher: given turn state it returns [`Candidate`]s.
//!   Hooks hold no state and never see the ledger, so their triggers are
//!   trivially unit-testable.
//! - The [`HookEngine`] owns the [`NudgeLedger`], applies the one-time / cooldown
//!   gate, ranks, caps to **one nudge per turn**, and hands back the [`Nudge`]s
//!   to deliver.
//! - The agent loop routes a [`Nudge`] by its [`Delivery`]: a `Rider` is appended
//!   to an existing tool_result (persists, contextual → one-time signposts); a
//!   `SystemTail` is dropped into the next turn's volatile (uncached) system
//!   block (ephemeral, recomposed → periodic reminders). The split keeps a
//!   changing tail from busting the cached prefix (§1).
//!
//! The ledger rides the session `side` cell exactly like `bro_tools::TodoList`,
//! so it survives `exec → resume`.

use crate::transport::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Appended at most once per session — the adopt-or-explain half of the
/// feedback loop. An agent that declines the steer should say *why* via a gap
/// note, turning a silent fallback into actionable signal for refining the tool
/// surface. Cross-cutting policy, so it's added at delivery (not per rule), but
/// session-deduped so periodic nudges cannot repeatedly compel notes.
const GAP_NOTE_DIRECTIVE: &str = " If this suggestion is wrong for the task, ignore it; \
    if the tool surface is actually missing or wrong-shaped, file `bbox_note(kind=\"followup\")`.";

/// Focused kill switch for the gap-note rider. `BRO_HARNESS_NUDGES=0` disables
/// the whole hook subsystem; this leaves nudges on while making "quiet down"
/// satisfiable for note-storm mitigation.
const GAP_NOTE_DIRECTIVE_ENV: &str = "BRO_HARNESS_NUDGE_GAP_NOTES";

/// Where a nudge is delivered. The choice follows its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Append to an existing `tool_result`'s content. Persists in the
    /// conversation; contextual to the triggering action. Use for one-time
    /// signposts. Only meaningful from `on_tool_result`.
    Rider,
    /// Drop into the next turn's volatile system tail. Ephemeral — recomposed
    /// each turn, never accumulates. Use for periodic / ambient reminders.
    SystemTail,
}

/// One-time vs recurring, with the ledger gate that implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeKind {
    /// Fires at most once per session (deduped via the fired ledger).
    Signpost,
    /// May re-fire after `cooldown` turns elapse.
    Periodic { cooldown: u64 },
}

/// A nudge a hook proposes this turn, before the ledger gate.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub rule_id: String,
    pub message: String,
    pub delivery: Delivery,
    pub kind: NudgeKind,
    /// Higher wins when several candidates match the same turn.
    pub priority: u8,
}

/// A nudge that passed the gate and should be delivered.
#[derive(Debug, Clone)]
pub struct Nudge {
    pub rule_id: String,
    pub message: String,
    pub delivery: Delivery,
}

impl Nudge {
    /// Render the rider form: a low-salience tagged block, honestly attributed
    /// to the harness (mirrors Claude Code's `<system-reminder>` convention).
    pub fn rider_block(&self) -> String {
        format!("\n\n<harness-note>{}</harness-note>", self.message)
    }
}

/// Persisted nudge *gating* state. Rides the session `side` cell (see
/// `from_side` / `to_side`), so one-time signposts stay fired and cooldowns
/// persist across `exec → resume`.
///
/// Note: there is deliberately **no** adoption/telemetry record here. Whether a
/// nudge was adopted — or declined, with or without a gap note — is a
/// retrospective query over the indexed tool-call transcript corpus (which
/// already logs every call and contains the `<harness-note>` rider itself), not
/// per-session state the harness duplicates. See bro-harness-hooks.md §6.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NudgeLedger {
    /// Signpost rule_ids that have already fired — never repeat.
    fired: HashSet<String>,
    /// Periodic rule_id -> turns remaining before it may fire again.
    cooldown: HashMap<String, u64>,
    /// Whether the cross-cutting gap-note rider has already been delivered.
    /// Default false for older persisted side blobs.
    #[serde(default)]
    gap_note_directive_delivered: bool,
}

impl NudgeLedger {
    /// Tolerant restore from the `side` blob (absent/garbage → empty), matching
    /// `TodoList::from_side`.
    pub fn from_side(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }
    pub fn to_side(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// One-time gate: true exactly the first time `rule_id` is seen.
    fn try_fire_once(&mut self, rule_id: &str) -> bool {
        self.fired.insert(rule_id.to_string())
    }

    /// Cooldown gate: true if the rule is off cooldown; arms the cooldown.
    fn try_fire_periodic(&mut self, rule_id: &str, cooldown: u64) -> bool {
        if self.cooldown.get(rule_id).copied().unwrap_or(0) > 0 {
            return false;
        }
        self.cooldown.insert(rule_id.to_string(), cooldown);
        true
    }

    /// Decrement all cooldowns by one turn.
    fn tick(&mut self) {
        for v in self.cooldown.values_mut() {
            *v = v.saturating_sub(1);
        }
    }

    /// True once per session, used to prevent periodic nudges from repeatedly
    /// reintroducing a note-filing instruction.
    fn try_deliver_gap_note_directive(&mut self) -> bool {
        if self.gap_note_directive_delivered {
            return false;
        }
        self.gap_note_directive_delivered = true;
        true
    }
}

/// A pure trigger matcher. Each method defaults to "no candidates" so a hook
/// implements only the phase(s) it cares about.
pub trait Hook: Send + Sync {
    fn on_user_turn(&self, _prompt: &str) -> Vec<Candidate> {
        Vec::new()
    }
    fn on_assistant_turn(&self, _text: &str, _calls: &[ToolCall]) -> Vec<Candidate> {
        Vec::new()
    }
    fn on_tool_result(&self, _call: &ToolCall, _result: &ToolResult) -> Vec<Candidate> {
        Vec::new()
    }
}

/// Owns the hooks and the ledger; applies the gate/rank/cap policy.
pub struct HookEngine {
    hooks: Vec<Box<dyn Hook>>,
    ledger: NudgeLedger,
    gap_note_directive_enabled: bool,
}

impl HookEngine {
    pub fn new(hooks: Vec<Box<dyn Hook>>, ledger: NudgeLedger) -> Self {
        Self::with_gap_note_directive(hooks, ledger, true)
    }

    fn with_gap_note_directive(
        hooks: Vec<Box<dyn Hook>>,
        ledger: NudgeLedger,
        gap_note_directive_enabled: bool,
    ) -> Self {
        Self {
            hooks,
            ledger,
            gap_note_directive_enabled,
        }
    }

    /// The default rule set shipped with the harness (§2: one trivial rule).
    /// Gated by `BRO_HARNESS_NUDGES` (default on; `0`/`false` disables) so the
    /// whole subsystem can be switched off without code changes. Uses
    /// transport session env first, then process env, so daemon-dispatched
    /// in-process sessions can override it without mutating global env.
    pub fn from_env(ledger: NudgeLedger) -> Self {
        let enabled = session_flag_enabled("BRO_HARNESS_NUDGES", true);
        let gap_note_directive_enabled = session_flag_enabled(GAP_NOTE_DIRECTIVE_ENV, true);
        let hooks: Vec<Box<dyn Hook>> = if enabled {
            vec![
                Box::new(TodoAllDoneHook),
                Box::new(ShellGrepHook),
                Box::new(RefactorSignpostHook),
                Box::new(HedgedConventionHook),
            ]
        } else {
            Vec::new()
        };
        Self::with_gap_note_directive(hooks, ledger, gap_note_directive_enabled)
    }

    pub fn to_side(&self) -> Value {
        self.ledger.to_side()
    }

    /// Decrement cooldowns at the end of a turn.
    pub fn tick(&mut self) {
        self.ledger.tick();
    }

    pub fn on_user_turn(&mut self, prompt: &str) -> Vec<Nudge> {
        let cands: Vec<Candidate> = self
            .hooks
            .iter()
            .flat_map(|h| h.on_user_turn(prompt))
            .collect();
        self.admit(cands)
    }

    pub fn on_assistant_turn(&mut self, text: &str, calls: &[ToolCall]) -> Vec<Nudge> {
        let cands: Vec<Candidate> = self
            .hooks
            .iter()
            .flat_map(|h| h.on_assistant_turn(text, calls))
            .collect();
        self.admit(cands)
    }

    pub fn on_tool_result(&mut self, call: &ToolCall, result: &ToolResult) -> Vec<Nudge> {
        let cands: Vec<Candidate> = self
            .hooks
            .iter()
            .flat_map(|h| h.on_tool_result(call, result))
            .collect();
        self.admit(cands)
    }

    /// Gate → rank → cap-to-one. Returns the single highest-priority candidate
    /// that passes its ledger gate (one nudge per turn; the rest are dropped
    /// this turn).
    fn admit(&mut self, mut cands: Vec<Candidate>) -> Vec<Nudge> {
        // Deterministic ranking: priority desc, then rule_id for stable ties.
        cands.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.rule_id.cmp(&b.rule_id))
        });
        for c in cands {
            let pass = match c.kind {
                NudgeKind::Signpost => self.ledger.try_fire_once(&c.rule_id),
                NudgeKind::Periodic { cooldown } => {
                    self.ledger.try_fire_periodic(&c.rule_id, cooldown)
                }
            };
            if pass {
                let mut message = c.message;
                if self.gap_note_directive_enabled && self.ledger.try_deliver_gap_note_directive() {
                    message.push_str(GAP_NOTE_DIRECTIVE);
                }
                let n = Nudge {
                    rule_id: c.rule_id,
                    message,
                    delivery: c.delivery,
                };
                tracing::info!(rule = %n.rule_id, ?n.delivery, "nudge fired");
                return vec![n];
            }
        }
        Vec::new()
    }
}

fn session_flag_enabled(key: &str, default: bool) -> bool {
    crate::transport::session_var(key)
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(default)
}

// ── Rules ────────────────────────────────────────────────────────────────

/// §2's trivial rule: the model ran a manual repo search via `shell_run`
/// (`grep`/`rg`/`find`/…) — steer it toward the indexed search surface. A
/// behavioral trigger (inspects the dispatched call), delivered as a rider on
/// the shell result so it's contextual to what just happened.
struct ShellGrepHook;

impl ShellGrepHook {
    /// True when the effective command (after peeling wrappers) is a known
    /// repo-search tool.
    fn is_manual_search(command: &str) -> bool {
        let base = effective_command_basename(command).unwrap_or("");
        matches!(
            base,
            "grep" | "rg" | "egrep" | "fgrep" | "ag" | "ack" | "find"
        )
    }
}

/// Operator command-proxy wrappers that this host requires in front of every
/// shell command (see `dispatch_extra_path_entries` in the daemon's
/// `providers::exec_args`). A wrapper doesn't change which underlying tool runs,
/// so first-token matchers must peel it off — otherwise `rtk grep foo` hides the
/// `grep` and the nudge never fires.
const COMMAND_PROXY_WRAPPERS: &[&str] = &["rtk"];

/// Leading no-op prefixes that delegate to the command that follows them,
/// without consuming flag arguments of their own. Conservative on purpose:
/// prefixes like `nice`/`nohup`/`env -i` take options that would make naive
/// peeling wrong, so they're deliberately excluded.
const PASSTHROUGH_PREFIXES: &[&str] = &["time", "command", "exec"];

/// Basename of a token, stripping any leading path (`/usr/bin/grep` → `grep`).
fn token_basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

/// `VAR=value` shell env assignment that precedes the actual command word.
fn is_env_assignment(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((name, _)) if !name.is_empty() => name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())),
        _ => false,
    }
}

/// Basename of the *effective* command after peeling shell prefixes that don't
/// change which tool runs: leading `VAR=val` env assignments, passthrough
/// prefixes (`time`/`command`/`exec`), and operator proxy wrappers (`rtk`, and
/// the `rtk proxy <cmd>` escape-hatch form). Returns `None` for an empty or
/// fully-consumed command. Conservative — anything unrecognized stops peeling,
/// since a false-positive nudge is worse than a miss.
fn effective_command_basename(command: &str) -> Option<&str> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut i = 0;
    while let Some(&tok) = tokens.get(i) {
        if is_env_assignment(tok) {
            i += 1;
            continue;
        }
        let base = token_basename(tok);
        if PASSTHROUGH_PREFIXES.contains(&base) {
            i += 1;
            continue;
        }
        if COMMAND_PROXY_WRAPPERS.contains(&base) {
            i += 1;
            // `rtk proxy <cmd>` runs <cmd> raw; skip the `proxy` subcommand too.
            if tokens.get(i).map(|t| token_basename(t)) == Some("proxy") {
                i += 1;
            }
            continue;
        }
        return Some(base);
    }
    None
}

impl Hook for ShellGrepHook {
    fn on_tool_result(&self, call: &ToolCall, _result: &ToolResult) -> Vec<Candidate> {
        if call.name != "shell_run" {
            return Vec::new();
        }
        let command = call
            .args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !Self::is_manual_search(command) {
            return Vec::new();
        }
        vec![Candidate {
            rule_id: "shell-grep-to-code-search".into(),
            message: "For broad or repeated repo searches, prefer indexed, \
                      gitignore-aware code/graph search via the `code.*` cell \
                      bindings or `bbox_hybrid_search` when those fit and are \
                      available."
                .into(),
            delivery: Delivery::Rider,
            kind: NudgeKind::Periodic { cooldown: 6 },
            priority: 10,
        }]
    }
}

/// Todo hygiene nudge: when the model has marked every task complete but leaves
/// the shared list populated, remind it that purely operational checklists can be
/// cleared. Delivered on the todo_write result itself so the advice is
/// contextual and reaches the model even though the fleet TUI summarizes todo
/// results instead of showing their raw JSON.
struct TodoAllDoneHook;

impl Hook for TodoAllDoneHook {
    fn on_tool_result(&self, call: &ToolCall, result: &ToolResult) -> Vec<Candidate> {
        if call.name != "todo_write" || result.is_error {
            return Vec::new();
        }
        let Some(items) = call.args.get("items").and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        if items.is_empty() {
            return Vec::new();
        }
        let all_done = items.iter().all(|item| {
            item.get("status")
                .and_then(|s| s.as_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("completed"))
        });
        if !all_done {
            return Vec::new();
        }
        vec![Candidate {
            rule_id: "todo-all-done-clear-exhaust".into(),
            message: "All todo tasks are marked completed. Consider clearing the task list by calling `todo_write` with `{\"items\": []}` if it is only operational exhaust and does not contain details the user may care about later."
                .into(),
            delivery: Delivery::Rider,
            kind: NudgeKind::Periodic { cooldown: 3 },
            priority: 16,
        }]
    }
}

/// Lexical signpost: the work looks like a structured refactor — point at the
/// refactor runbook + guarded refactor surface, once per session. Fires from
/// either the user ask or the assistant's own framing.
struct RefactorSignpostHook;

const REFACTOR_CUES: &[&str] = &[
    "refactor",
    "rename across",
    "rename the function",
    "rename the method",
    "extract function",
    "extract method",
    "move it to a module",
    "move to a new module",
    "organize imports",
    "change the signature",
    "pull this into",
];

impl RefactorSignpostHook {
    fn match_text(&self, t: &str) -> Vec<Candidate> {
        let lc = t.to_ascii_lowercase();
        if !REFACTOR_CUES.iter().any(|c| lc.contains(c)) {
            return Vec::new();
        }
        vec![Candidate {
            rule_id: "refactor-signpost".into(),
            message: "This looks like structured refactor work. Use the in-box \
                      refactor bindings (`code.*` facts, `java.*` transforms, \
                      `edits.*` mutation choke point, `analysis.*`, `lsp.*`) — \
                      they do guarded, hash-anchored structural edits (rename, \
                      move item, organize imports) that beat hand-editing."
                .into(),
            delivery: Delivery::SystemTail,
            kind: NudgeKind::Signpost,
            priority: 8,
        }]
    }
}

impl Hook for RefactorSignpostHook {
    fn on_user_turn(&self, prompt: &str) -> Vec<Candidate> {
        self.match_text(prompt)
    }
    fn on_assistant_turn(&self, text: &str, _calls: &[ToolCall]) -> Vec<Candidate> {
        self.match_text(text)
    }
}

/// Lexical: the assistant is *inferring* a project convention instead of
/// confirming it ("I think we use…"). Nudge toward the authoritative stores.
/// Lowest priority and periodic — lexical hedging is the noisiest signal, so it
/// yields to behavioral rules and self-throttles.
struct HedgedConventionHook;

const HEDGE_CUES: &[&str] = &[
    "i think we use",
    "i believe we use",
    "probably uses",
    "i assume the convention",
    "likely the convention",
    "i'm guessing the",
    "presumably the project",
];

impl Hook for HedgedConventionHook {
    fn on_assistant_turn(&self, text: &str, _calls: &[ToolCall]) -> Vec<Candidate> {
        let lc = text.to_ascii_lowercase();
        if !HEDGE_CUES.iter().any(|c| lc.contains(c)) {
            return Vec::new();
        }
        vec![Candidate {
            rule_id: "hedged-convention".into(),
            message: "You're inferring a project convention rather than confirming it. \
                      `bbox_knowledge` (durable rules/decisions) and `bbox_hybrid_search` (the \
                      indexed graph) likely hold the authoritative answer."
                .into(),
            delivery: Delivery::SystemTail,
            kind: NudgeKind::Periodic { cooldown: 10 },
            priority: 5,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell_call(command: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "shell_run".into(),
            args: json!({ "command": command }),
        }
    }
    fn ok_result() -> ToolResult {
        ToolResult {
            id: "1".into(),
            content: "out".into(),
            is_error: false,
        }
    }

    #[test]
    fn shell_grep_hook_triggers_on_search_tools() {
        let h = ShellGrepHook;
        for cmd in [
            "grep -r foo .",
            "rg foo",
            "find . -name '*.rs'",
            "/usr/bin/grep x",
            // proxy-wrapped forms still fire (host prefixes commands with rtk)
            "rtk grep -r foo .",
            "rtk rg foo",
            "rtk proxy grep x",
            // peel env assignments / passthrough prefixes too
            "RUST_LOG=debug rtk rg foo",
            "time grep foo",
        ] {
            assert_eq!(
                h.on_tool_result(&shell_call(cmd), &ok_result()).len(),
                1,
                "{cmd}"
            );
        }
        for cmd in [
            "cargo build",
            "ls -la",
            "echo grep",
            // wrappers in front of a non-search command must NOT fire
            "rtk cargo build",
            "rtk proxy bash -lc 'sed -n 1,5p f'",
            "rtk",
        ] {
            assert!(
                h.on_tool_result(&shell_call(cmd), &ok_result()).is_empty(),
                "{cmd}"
            );
        }
    }

    #[test]
    fn non_shell_tool_is_ignored() {
        let mut call = shell_call("grep foo");
        call.name = "file_read".into();
        assert!(ShellGrepHook.on_tool_result(&call, &ok_result()).is_empty());
    }

    #[test]
    fn engine_caps_to_one_and_signpost_fires_once() {
        struct Always(&'static str, u8);
        impl Hook for Always {
            fn on_user_turn(&self, _: &str) -> Vec<Candidate> {
                vec![Candidate {
                    rule_id: self.0.into(),
                    message: "m".into(),
                    delivery: Delivery::SystemTail,
                    kind: NudgeKind::Signpost,
                    priority: self.1,
                }]
            }
        }
        let mut eng = HookEngine::new(
            vec![Box::new(Always("low", 1)), Box::new(Always("high", 9))],
            NudgeLedger::default(),
        );
        // Two candidates, capped to one — the higher priority wins.
        let first = eng.on_user_turn("hi");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].rule_id, "high");
        // Signpost already fired; next call "high" is gated out, "low" wins.
        let second = eng.on_user_turn("hi");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].rule_id, "low");
        // Both fired once now → nothing left.
        assert!(eng.on_user_turn("hi").is_empty());
    }

    #[test]
    fn periodic_respects_cooldown() {
        let mut l = NudgeLedger::default();
        assert!(l.try_fire_periodic("r", 2)); // fires, arms cooldown=2
        assert!(!l.try_fire_periodic("r", 2)); // still cooling
        l.tick();
        assert!(!l.try_fire_periodic("r", 2)); // 1 remaining
        l.tick();
        assert!(l.try_fire_periodic("r", 2)); // cooled down → fires again
    }

    #[test]
    fn gating_state_round_trips_through_side() {
        // Only gating state persists — fired-once set + cooldowns. (Adoption is
        // a transcript query, not harness state; nothing to round-trip there.)
        let mut l = NudgeLedger::default();
        l.try_fire_once("a");
        l.try_fire_periodic("b", 3);
        let mut r = NudgeLedger::from_side(&l.to_side());
        // Fired-once dedup survives resume.
        assert!(!r.try_fire_once("a"));
        // Cooldown survives resume (still armed → won't re-fire).
        assert!(!r.try_fire_periodic("b", 3));
    }

    // ── §3 rule tests ──────────────────────────────────────────────────

    #[test]
    fn todo_all_done_hook_suggests_clearing_operational_exhaust() {
        let h = TodoAllDoneHook;
        let call = ToolCall {
            id: "todo".into(),
            name: "todo_write".into(),
            args: json!({"items":[
                {"task":"implement","status":"completed"},
                {"task":"validate","status":"completed"}
            ]}),
        };
        let fired = h.on_tool_result(&call, &ok_result());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].rule_id, "todo-all-done-clear-exhaust");
        assert_eq!(fired[0].delivery, Delivery::Rider);
        assert!(fired[0].message.contains("todo_write"));
        assert!(fired[0].message.contains("operational exhaust"));
    }

    #[test]
    fn todo_all_done_hook_ignores_incomplete_empty_or_error_results() {
        let h = TodoAllDoneHook;
        let incomplete = ToolCall {
            id: "todo".into(),
            name: "todo_write".into(),
            args: json!({"items":[
                {"task":"implement","status":"completed"},
                {"task":"validate","status":"pending"}
            ]}),
        };
        assert!(h.on_tool_result(&incomplete, &ok_result()).is_empty());

        let empty = ToolCall {
            id: "todo".into(),
            name: "todo_write".into(),
            args: json!({"items":[]}),
        };
        assert!(h.on_tool_result(&empty, &ok_result()).is_empty());

        let error = ToolResult {
            id: "todo".into(),
            content: "bad input".into(),
            is_error: true,
        };
        assert!(h.on_tool_result(&incomplete, &error).is_empty());
    }

    #[test]
    fn refactor_signpost_matches_user_or_assistant_text() {
        let h = RefactorSignpostHook;
        assert_eq!(h.on_user_turn("please refactor the auth module").len(), 1);
        assert_eq!(
            h.on_assistant_turn("I'll extract method here", &[]).len(),
            1
        );
        assert!(h.on_user_turn("add a new endpoint").is_empty());
        // It's a signpost (one-time semantics enforced by the engine ledger).
        assert_eq!(h.on_user_turn("rework this").len(), 0); // "rework" not a cue
        assert!(matches!(
            h.on_user_turn("refactor x")[0].kind,
            NudgeKind::Signpost
        ));
    }

    #[test]
    fn hedged_convention_matches_hedges_only() {
        let h = HedgedConventionHook;
        assert_eq!(
            h.on_assistant_turn("I think we use tokio here", &[]).len(),
            1
        );
        assert!(
            h.on_assistant_turn("We use tokio, confirmed via bbox_knowledge", &[])
                .is_empty()
        );
        assert_eq!(
            h.on_assistant_turn("this PROBABLY USES serde", &[]).len(),
            1
        ); // case-insensitive
    }

    #[test]
    fn behavioral_outranks_lexical_when_both_match_a_turn() {
        // copy-paste (priority 20) beats refactor signpost (8) under the
        // one-per-turn cap. Drive both through the engine via tool_result —
        // but lexical rules fire on assistant_turn, so verify ranking directly
        // through admit by colliding two candidates on the same phase.
        struct Hi;
        impl Hook for Hi {
            fn on_assistant_turn(&self, _: &str, _: &[ToolCall]) -> Vec<Candidate> {
                vec![
                    Candidate {
                        rule_id: "lex".into(),
                        message: "m".into(),
                        delivery: Delivery::SystemTail,
                        kind: NudgeKind::Signpost,
                        priority: 8,
                    },
                    Candidate {
                        rule_id: "beh".into(),
                        message: "m".into(),
                        delivery: Delivery::Rider,
                        kind: NudgeKind::Signpost,
                        priority: 20,
                    },
                ]
            }
        }
        let mut eng = HookEngine::new(vec![Box::new(Hi)], NudgeLedger::default());
        let out = eng.on_assistant_turn("t", &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule_id, "beh");
    }

    #[test]
    fn delivered_nudges_carry_quiet_gap_followup_escape_hatch() {
        // The first delivered nudge gets the gap-note directive appended at the
        // engine choke point.
        let mut eng = HookEngine::new(vec![Box::new(ShellGrepHook)], NudgeLedger::default());
        let out = eng.on_tool_result(&shell_call("grep -r x ."), &ok_result());
        assert_eq!(out.len(), 1);
        let msg = &out[0].message;
        assert!(
            msg.contains("bbox_note"),
            "directive names the followup-note tool"
        );
        assert!(
            msg.contains("If this suggestion is wrong"),
            "directive frames non-applicable suggestions as ignorable"
        );
        // The rule's own body is still there ahead of the directive.
        assert!(msg.contains("indexed"));
        // And it's inside the rider envelope when delivered as a rider.
        assert!(out[0].rider_block().contains("<harness-note>"));
    }

    #[test]
    fn gap_note_directive_is_session_deduped() {
        struct TwoSignposts;
        impl Hook for TwoSignposts {
            fn on_user_turn(&self, _: &str) -> Vec<Candidate> {
                vec![
                    Candidate {
                        rule_id: "first".into(),
                        message: "first nudge".into(),
                        delivery: Delivery::SystemTail,
                        kind: NudgeKind::Signpost,
                        priority: 2,
                    },
                    Candidate {
                        rule_id: "second".into(),
                        message: "second nudge".into(),
                        delivery: Delivery::SystemTail,
                        kind: NudgeKind::Signpost,
                        priority: 1,
                    },
                ]
            }
        }

        let mut eng = HookEngine::new(vec![Box::new(TwoSignposts)], NudgeLedger::default());
        let first = eng.on_user_turn("x");
        assert_eq!(first[0].rule_id, "first");
        assert!(first[0].message.contains("bbox_note"));

        let second = eng.on_user_turn("x");
        assert_eq!(second[0].rule_id, "second");
        assert!(second[0].message.contains("second nudge"));
        assert!(
            !second[0].message.contains("bbox_note"),
            "gap-note rider must not repeat on later nudges"
        );
    }

    #[test]
    fn gap_note_directive_can_be_disabled_without_disabling_nudges() {
        let mut eng = HookEngine::with_gap_note_directive(
            vec![Box::new(ShellGrepHook)],
            NudgeLedger::default(),
            false,
        );
        let out = eng.on_tool_result(&shell_call("grep -r x ."), &ok_result());
        assert_eq!(out.len(), 1);
        assert!(out[0].message.contains("indexed"));
        assert!(!out[0].message.contains("bbox_note"));
    }

    #[tokio::test]
    async fn from_env_reads_session_env_for_gap_note_directive() {
        crate::transport::with_session_env(
            std::collections::BTreeMap::from([(
                GAP_NOTE_DIRECTIVE_ENV.to_string(),
                "0".to_string(),
            )]),
            async {
                let mut eng = HookEngine::from_env(NudgeLedger::default());
                let out = eng.on_tool_result(&shell_call("grep -r x ."), &ok_result());
                assert_eq!(out.len(), 1);
                assert!(out[0].message.contains("indexed"));
                assert!(!out[0].message.contains("bbox_note"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn from_env_reads_session_env_for_all_nudges_flag() {
        crate::transport::with_session_env(
            std::collections::BTreeMap::from([("BRO_HARNESS_NUDGES".to_string(), "0".to_string())]),
            async {
                let mut eng = HookEngine::from_env(NudgeLedger::default());
                let out = eng.on_tool_result(&shell_call("grep -r x ."), &ok_result());
                assert!(out.is_empty());
            },
        )
        .await;
    }
}
