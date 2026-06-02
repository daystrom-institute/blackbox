//! Model-keyed compaction thresholds.
//!
//! Compaction is triggered when the prompt the model just processed
//! (`Usage::total_input_tokens` — cache-inclusive, the real window occupancy)
//! crosses a fraction of the model's context window. The window is a property
//! of the **model**, so the policy keys on the `--model` string the harness
//! already has — no provider env needed. The same normalized id keeps its
//! family prefix (`glm-…`, `deepseek-…`, `claude-…`) even after the daemon
//! strips the registry namespace, so a glob key like `glm-*` doubles as the
//! provider-level bucket.
//!
//! Lookup is a three-step fallback that needs nothing but the model string:
//!
//! ```text
//! exact "glm-4.6"  →  glob "glm-*" (longest match)  →  "default"
//! ```
//!
//! `compact_at` (window fraction) and `context_window` each resolve
//! independently through that chain, so a glob can set the window while
//! inheriting the default ratio.
//!
//! Config source: a JSON file pointed to by `BRO_HARNESS_COMPACTION_CONFIG`,
//! falling back to the built-in table below. Shape:
//!
//! ```json
//! {
//!   "default":           { "context_window": 200000, "compact_at": 0.75 },
//!   "claude-*":          { "context_window": 200000 },
//!   "glm-*":             { "context_window": 200000 },
//!   "deepseek-*":        { "context_window": 128000 },
//!   "deepseek-reasoner": { "context_window": 128000, "compact_at": 0.6 }
//! }
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;

const DEFAULT_WINDOW: u64 = 200_000;
const DEFAULT_RATIO: f64 = 0.75;
const DEFAULT_KEEP_TAIL: usize = 6;
/// Inline-summary output-token cap. Codex's inline summary is a full turn; the
/// old hardcoded 2048 was far too small for a long thread. Generous default,
/// env-overridable via `BRO_HARNESS_COMPACTION_SUMMARY_TOKENS`.
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;
/// Per-tool-result char cap when rendering the prefix transcript for the inline
/// summarizer. Bounds the summarization prompt; env-overridable via
/// `BRO_HARNESS_COMPACTION_TOOL_CAP`.
const DEFAULT_TOOL_RENDER_CAP: usize = 2000;

#[derive(Debug, Clone, Default, Deserialize)]
struct Entry {
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    compact_at: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    entries: BTreeMap<String, Entry>,
    keep_tail: usize,
    summary_max_tokens: u32,
    tool_render_cap: usize,
    enabled: bool,
}

impl CompactionPolicy {
    pub fn from_env() -> Self {
        let enabled = std::env::var("BRO_HARNESS_COMPACTION")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("off") && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        let keep_tail = std::env::var("BRO_HARNESS_COMPACTION_KEEP_TAIL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_KEEP_TAIL);
        let summary_max_tokens = std::env::var("BRO_HARNESS_COMPACTION_SUMMARY_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SUMMARY_MAX_TOKENS);
        let tool_render_cap = std::env::var("BRO_HARNESS_COMPACTION_TOOL_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TOOL_RENDER_CAP);
        let entries = std::env::var("BRO_HARNESS_COMPACTION_CONFIG")
            .ok()
            .and_then(|p| match std::fs::read_to_string(&p) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("compaction config {p} unreadable: {e}; using defaults");
                    None
                }
            })
            .and_then(
                |s| match serde_json::from_str::<BTreeMap<String, Entry>>(&s) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::warn!("compaction config parse error: {e}; using defaults");
                        None
                    }
                },
            )
            .unwrap_or_else(default_entries);
        Self {
            entries,
            keep_tail,
            summary_max_tokens,
            tool_render_cap,
            enabled,
        }
    }

    /// Resolve `(context_window, compact_at)` for a model, each field walking
    /// the exact → longest-glob → default chain independently.
    fn resolve(&self, model: &str) -> (u64, f64) {
        let exact = self.entries.get(model);
        // Longest matching glob prefix wins (e.g. "deepseek-r*" beats "deepseek-*").
        let glob = self
            .entries
            .iter()
            .filter(|(k, _)| {
                k.strip_suffix('*')
                    .is_some_and(|prefix| model.starts_with(prefix))
            })
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| v);
        let default = self.entries.get("default");

        let field = |f: &dyn Fn(&Entry) -> Option<f64>| -> Option<f64> {
            exact
                .and_then(f)
                .or_else(|| glob.and_then(f))
                .or_else(|| default.and_then(f))
        };
        let window =
            field(&|e| e.context_window.map(|w| w as f64)).unwrap_or(DEFAULT_WINDOW as f64);
        let ratio = field(&|e| e.compact_at).unwrap_or(DEFAULT_RATIO);
        (window as u64, ratio)
    }

    /// Token count above which the prefix should be compacted, or `None` when
    /// compaction is disabled or the window is unknown/zero.
    pub fn threshold(&self, model: &str) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        let (window, ratio) = self.resolve(model);
        if window == 0 {
            return None;
        }
        Some((window as f64 * ratio) as u64)
    }

    /// Bundle the per-pass tuning knobs for `Transport::compact`.
    pub fn params(&self) -> crate::transport::CompactionParams {
        crate::transport::CompactionParams {
            keep_tail: self.keep_tail,
            summary_max_tokens: self.summary_max_tokens,
            tool_render_cap: self.tool_render_cap,
        }
    }
}

fn default_entries() -> BTreeMap<String, Entry> {
    let mut m = BTreeMap::new();
    m.insert(
        "default".into(),
        Entry {
            context_window: Some(DEFAULT_WINDOW),
            compact_at: Some(DEFAULT_RATIO),
        },
    );
    for (k, w) in [
        ("claude-*", 200_000),
        ("glm-*", 200_000),
        ("deepseek-*", 128_000),
    ] {
        m.insert(
            k.into(),
            Entry {
                context_window: Some(w),
                compact_at: None,
            },
        );
    }
    m
}

/// The summarization directive sent to the model when compacting.
///
/// Structured after the canonical coding-agent compaction prompt (see
/// `design/bro-harness/compaction-canonical-anthropic.md` §3): an `<analysis>`
/// scratchpad to force a chronological pass, then a durable `<summary>` block
/// with fixed sections. Only the `<summary>` block is retained — the transports'
/// `summarize_text` runs the result through [`crate::transport::extract_summary`]
/// to drop the scratchpad. Verbatim preservation of security-relevant
/// instructions is a correctness requirement: a "never touch X" rule the user
/// gave must survive compaction or it silently stops applying.
pub const COMPACTION_INSTRUCTION: &str = "The conversation above is being compacted to free up context. \
It will be replaced by your summary, and the session will continue with new messages appended after it — \
so capture everything needed to continue the work without the original transcript.\n\n\
First, think in an <analysis> block: go through the conversation chronologically and note the user's \
explicit requests, your approach, key decisions and their rationale, files/paths/symbols touched (with the \
important code), commands run and their outcomes, errors and how they were resolved (including any user \
feedback), and what remains open. Pay special attention to the most recent messages. Preserve any \
security-relevant instructions or constraints the user gave (files or data to avoid, operations that must \
not be performed, secret-handling rules) VERBATIM so they still apply after compaction.\n\n\
Then write the durable summary inside a single <summary> block with these sections:\n\
1. Primary request and intent — the user's explicit goals and constraints, in detail.\n\
2. Key technical concepts — technologies, frameworks, and patterns in play.\n\
3. Files and code — specific files/symbols examined or changed, why each matters, with important snippets.\n\
4. Errors and fixes — what went wrong and how it was resolved, plus any user feedback.\n\
5. Problem solving — what is solved and what troubleshooting is ongoing.\n\
6. All user messages — every non-tool-result user message, so intent and feedback are not lost; keep any \
security-relevant instructions verbatim.\n\
7. Pending tasks — what remains to do.\n\
8. Current work — precisely what was being done immediately before this point, with file names and snippets.\n\
9. Next step — the immediate next step, only if it directly continues the most recent work; quote the \
relevant request or task text so the intent does not drift.\n\n\
Output the durable result in the <summary> block. Be specific and omit pleasantries.";

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[(&str, Option<u64>, Option<f64>)]) -> CompactionPolicy {
        let mut m = BTreeMap::new();
        for (k, w, r) in entries {
            m.insert(
                (*k).to_string(),
                Entry {
                    context_window: *w,
                    compact_at: *r,
                },
            );
        }
        CompactionPolicy {
            entries: m,
            keep_tail: DEFAULT_KEEP_TAIL,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            tool_render_cap: DEFAULT_TOOL_RENDER_CAP,
            enabled: true,
        }
    }

    #[test]
    fn exact_beats_glob_beats_default() {
        let p = policy(&[
            ("default", Some(100_000), Some(0.5)),
            ("glm-*", Some(200_000), None),
            ("glm-4.6", Some(210_000), Some(0.9)),
        ]);
        // exact wins both fields
        assert_eq!(p.resolve("glm-4.6"), (210_000, 0.9));
        // glob sets window, inherits default ratio
        assert_eq!(p.resolve("glm-4.5-air"), (200_000, 0.5));
        // neither → default
        assert_eq!(p.resolve("mystery-model"), (100_000, 0.5));
    }

    #[test]
    fn longest_glob_wins() {
        let p = policy(&[
            ("default", Some(100_000), Some(0.5)),
            ("deepseek-*", Some(128_000), None),
            ("deepseek-r*", Some(64_000), Some(0.6)),
        ]);
        assert_eq!(p.resolve("deepseek-reasoner"), (64_000, 0.6));
        assert_eq!(p.resolve("deepseek-chat"), (128_000, 0.5));
    }

    #[test]
    fn threshold_applies_ratio_and_respects_disable() {
        let p = policy(&[("default", Some(200_000), Some(0.75))]);
        assert_eq!(p.threshold("anything"), Some(150_000));
        let mut off = p.clone();
        off.enabled = false;
        assert_eq!(off.threshold("anything"), None);
    }

    #[test]
    fn builtin_defaults_cover_families() {
        let p = CompactionPolicy {
            entries: default_entries(),
            keep_tail: DEFAULT_KEEP_TAIL,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            tool_render_cap: DEFAULT_TOOL_RENDER_CAP,
            enabled: true,
        };
        assert_eq!(p.resolve("glm-4.6").0, 200_000);
        assert_eq!(p.resolve("deepseek-chat").0, 128_000);
        assert_eq!(p.resolve("claude-opus-4-8").0, 200_000);
        // ratio inherits from default everywhere
        assert_eq!(p.resolve("glm-4.6").1, DEFAULT_RATIO);
    }

    #[test]
    fn params_carry_tuning_knobs() {
        let p = CompactionPolicy {
            entries: default_entries(),
            keep_tail: 9,
            summary_max_tokens: 12_345,
            tool_render_cap: 4_096,
            enabled: true,
        };
        let params = p.params();
        assert_eq!(params.keep_tail, 9);
        assert_eq!(params.summary_max_tokens, 12_345);
        assert_eq!(params.tool_render_cap, 4_096);
    }

    #[test]
    fn defaults_lift_the_summary_cap_above_the_old_2048() {
        // Regression guard: the inline summary budget must not regress to the
        // old hardcoded 2048 that squeezed long threads.
        assert!(DEFAULT_SUMMARY_MAX_TOKENS > 2048);
    }
}
