//! Internal hook subsystem — named interception points on the agent loop that
//! observe turn state and contribute *ambient meta* (nudges) steering the model
//! toward the rich blackbox toolbox. Nudges never gate or remove tools.
//!
//! See design/orchestration/bro-harness-hooks.md: the scaffold + gating ledger
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

/// Appended to every delivered nudge — the adopt-or-explain half of the
/// feedback loop. An agent that declines the steer should say *why* via a gap
/// note, turning a silent fallback into actionable signal for refining the tool
/// surface. Cross-cutting policy, so it's added once at delivery (not per rule).
const GAP_NOTE_DIRECTIVE: &str = " — If this tool is the wrong fit because it's buggy, \
    missing a capability, or wrong-shaped for the job, don't silently fall back: file a \
    tool-surface gap note with `bbox_note(kind=\"followup\")` naming the tool and the gap. \
    That feedback is used to fix bugs, expand capabilities, and refine these tools. If the \
    nudge simply doesn't apply here, ignore it.";

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
}

impl HookEngine {
    pub fn new(hooks: Vec<Box<dyn Hook>>, ledger: NudgeLedger) -> Self {
        Self { hooks, ledger }
    }

    /// The default rule set shipped with the harness (§2: one trivial rule).
    /// Gated by `BRO_HARNESS_NUDGES` (default on; `0`/`false` disables) so the
    /// whole subsystem can be switched off without code changes.
    pub fn from_env(ledger: NudgeLedger) -> Self {
        let enabled = std::env::var("BRO_HARNESS_NUDGES")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        let hooks: Vec<Box<dyn Hook>> = if enabled {
            vec![
                Box::new(CopyPasteHook::default()),
                Box::new(ShellGrepHook),
                Box::new(RefactorSignpostHook),
                Box::new(HedgedConventionHook),
            ]
        } else {
            Vec::new()
        };
        Self::new(hooks, ledger)
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
                let n = Nudge {
                    rule_id: c.rule_id,
                    message: format!("{}{GAP_NOTE_DIRECTIVE}", c.message),
                    delivery: c.delivery,
                };
                tracing::info!(rule = %n.rule_id, ?n.delivery, "nudge fired");
                return vec![n];
            }
        }
        Vec::new()
    }
}

// ── Rules ────────────────────────────────────────────────────────────────

/// §2's trivial rule: the model ran a manual repo search via `shell_run`
/// (`grep`/`rg`/`find`/…) — steer it toward the indexed search surface. A
/// behavioral trigger (inspects the dispatched call), delivered as a rider on
/// the shell result so it's contextual to what just happened.
struct ShellGrepHook;

impl ShellGrepHook {
    /// True when the first token of a shell command is a known repo-search tool.
    fn is_manual_search(command: &str) -> bool {
        // Look past leading env assignments / `time` etc. only minimally: take
        // the first bare word. Conservative on purpose — false positives are
        // worse than misses for a nudge.
        let first = command.split_whitespace().next().unwrap_or("");
        // Strip a leading path (e.g. /usr/bin/grep) to the basename.
        let base = first.rsplit('/').next().unwrap_or(first);
        matches!(
            base,
            "grep" | "rg" | "egrep" | "fgrep" | "ag" | "ack" | "find"
        )
    }
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
            message: "You searched the repo with a shell tool. For indexed, \
                      gitignore-aware code/graph search prefer `bbox_code_query` or \
                      `bbox_hybrid_search` (load via `tool_search` if not yet available)."
                .into(),
            delivery: Delivery::Rider,
            kind: NudgeKind::Periodic { cooldown: 6 },
            priority: 10,
        }]
    }
}

/// Collapse runs of whitespace to single spaces — so a verbatim paste still
/// matches across reflowed indentation / line endings.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Behavioral copy-paste detector: the model read content, then wrote a
/// *verbatim* copy of it somewhere else — the bytes round-trip through context
/// twice. Steer toward the server-side structural move. Verbatim containment
/// above a length floor is deliberately conservative: low false-positive, and
/// it only flags the exact "context-copy-paste" the slice tools eliminate.
///
/// Stateful: remembers recent read outputs for this dispatch (interior
/// mutability; not persisted — a per-dispatch heuristic).
#[derive(Default)]
struct CopyPasteHook {
    reads: Mutex<VecDeque<String>>,
}

/// Min normalized-char overlap to call a write a paste of a read.
const MIN_PASTE_OVERLAP: usize = 120;
/// How many recent reads to remember, and the per-entry byte cap.
const COPY_PASTE_RECENT_READS: usize = 8;
const COPY_PASTE_ENTRY_CAP: usize = 16 * 1024;

impl Hook for CopyPasteHook {
    fn on_tool_result(&self, call: &ToolCall, result: &ToolResult) -> Vec<Candidate> {
        match call.name.as_str() {
            // Producers: remember what was read.
            "file_read" | "smart_read" => {
                if !result.is_error {
                    let mut text = result.content.clone();
                    text.truncate(COPY_PASTE_ENTRY_CAP);
                    if let Ok(mut q) = self.reads.lock() {
                        q.push_back(text);
                        while q.len() > COPY_PASTE_RECENT_READS {
                            q.pop_front();
                        }
                    }
                }
                Vec::new()
            }
            // Consumers: did the written text reproduce a recent read verbatim?
            "file_write" | "file_edit" => {
                let written = call
                    .args
                    .get("content")
                    .or_else(|| call.args.get("new_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let nw = normalize_ws(written);
                if nw.len() < MIN_PASTE_OVERLAP {
                    return Vec::new();
                }
                let hit = self
                    .reads
                    .lock()
                    .map(|q| {
                        q.iter().any(|r| {
                            let nr = normalize_ws(r);
                            let (short, long) = if nw.len() <= nr.len() {
                                (&nw, &nr)
                            } else {
                                (&nr, &nw)
                            };
                            short.len() >= MIN_PASTE_OVERLAP && long.contains(short.as_str())
                        })
                    })
                    .unwrap_or(false);
                if !hit {
                    return Vec::new();
                }
                vec![Candidate {
                    rule_id: "copy-paste-to-slice".into(),
                    message: "You wrote a verbatim copy of content you just read — that \
                              round-trips the bytes through context twice. `bbox_slice_copy` / \
                              `bbox_slice_move` perform the source→target move server-side \
                              without inlining the content (sha-guarded, dry-runnable)."
                        .into(),
                    delivery: Delivery::Rider,
                    kind: NudgeKind::Periodic { cooldown: 8 },
                    priority: 20,
                }]
            }
            _ => Vec::new(),
        }
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
            message: "This looks like structured refactor work. Check `sm-refactor` first via \
                      `bbox_knowledge` — the `bbox_refactor_*` / `bbox_slice_*` tools do guarded, \
                      semantics-aware structural edits (rename, move item, organize imports) that \
                      beat hand-editing."
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
        ] {
            assert_eq!(
                h.on_tool_result(&shell_call(cmd), &ok_result()).len(),
                1,
                "{cmd}"
            );
        }
        for cmd in ["cargo build", "ls -la", "echo grep"] {
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

    fn read_result(content: &str) -> ToolResult {
        ToolResult {
            id: "r".into(),
            content: content.into(),
            is_error: false,
        }
    }
    fn write_call(tool: &str, field: &str, text: &str) -> ToolCall {
        ToolCall {
            id: "w".into(),
            name: tool.into(),
            args: json!({ field: text }),
        }
    }
    fn read_call(tool: &str) -> ToolCall {
        ToolCall {
            id: "rd".into(),
            name: tool.into(),
            args: json!({ "file_path": "f" }),
        }
    }

    #[test]
    fn copy_paste_fires_on_verbatim_reproduction() {
        // A chunk well over the overlap floor, read then written verbatim.
        let chunk = "fn parse(input: &str) -> Result<Ast> { let tokens = lex(input)?; \
                     let mut p = Parser::new(tokens); p.parse_program().map_err(Into::into) } \
                     // end of the parser entrypoint, copied wholesale";
        let h = CopyPasteHook::default();
        // Read it (producer) — no nudge yet.
        assert!(
            h.on_tool_result(&read_call("file_read"), &read_result(chunk))
                .is_empty()
        );
        // Write the same content elsewhere (consumer) — nudge fires.
        let fired = h.on_tool_result(
            &write_call("file_write", "content", chunk),
            &read_result("ok"),
        );
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].rule_id, "copy-paste-to-slice");
        assert_eq!(fired[0].delivery, Delivery::Rider);
    }

    #[test]
    fn copy_paste_ignores_short_or_novel_writes() {
        let h = CopyPasteHook::default();
        h.on_tool_result(
            &read_call("file_read"),
            &read_result("some long original file body that is read"),
        );
        // Short write — under the overlap floor.
        assert!(
            h.on_tool_result(
                &write_call("file_edit", "new_string", "x = 1"),
                &read_result("ok")
            )
            .is_empty()
        );
        // Long but novel write — not a reproduction of anything read.
        let novel = "completely unrelated content authored fresh, ".repeat(6);
        assert!(
            h.on_tool_result(
                &write_call("file_write", "content", &novel),
                &read_result("ok")
            )
            .is_empty()
        );
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
    fn delivered_nudges_carry_adopt_or_explain_directive() {
        // Every delivered nudge — regardless of rule — gets the gap-note
        // directive appended once at the engine choke point.
        let mut eng = HookEngine::new(vec![Box::new(ShellGrepHook)], NudgeLedger::default());
        let out = eng.on_tool_result(&shell_call("grep -r x ."), &ok_result());
        assert_eq!(out.len(), 1);
        let msg = &out[0].message;
        assert!(
            msg.contains("bbox_note"),
            "directive names the gap-note tool"
        );
        assert!(
            msg.contains("gap note"),
            "directive frames it as a gap note"
        );
        // The rule's own body is still there ahead of the directive.
        assert!(msg.contains("indexed"));
        // And it's inside the rider envelope when delivered as a rider.
        assert!(out[0].rider_block().contains("<harness-note>"));
    }
}
