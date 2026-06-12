//! Typed dispatch-context payload — the `--dispatch-context <json>` boundary
//! surface (design/bro-harness/dispatch-prompt-slots.md §4).
//!
//! The daemon owns CONTENT SELECTION: which directives apply to a dispatch,
//! each directive's empirically-calibrated reinforcement cadence, the persona
//! resolved from the brofile, the pre-bound scope IDs, and the resolved pin
//! block. The harness owns COMPOSITION: where each ingredient lands per
//! transport (system stable slot, volatile tail, marker-demarcated contextual
//! user fragments, or the vibe-shaped leading system block). This DTO is the
//! ingredients list that crosses that boundary — typed values, never composed
//! prose.
//!
//! Parsing is deliberately strict (`deny_unknown_fields`, exact version
//! match): the payload is daemon-authored, so garbage is a bug to surface,
//! not input to tolerate.

use serde::{Deserialize, Serialize};

/// The only payload version this revision understands.
pub const DISPATCH_CONTEXT_VERSION: u32 = 1;

/// Typed ingredients for one dispatch. See module docs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchContext {
    /// Payload version; must equal [`DISPATCH_CONTEXT_VERSION`].
    pub v: u32,
    /// Brofile lens (persona / role system-prompt), verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// The ordered directive set the daemon selected for THIS dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directives: Vec<DispatchDirective>,
    /// Pre-bound scoping IDs. Typed key→value fields, NOT pre-rendered lines;
    /// the harness renders (and re-renders) them. NEVER restored from session
    /// side-state: `task` is per-dispatch correlation data, and a stale value
    /// would mis-route `bbox_note`/`bro_report` keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DispatchScope>,
    /// Resolved pin block text (bbox_pin), verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pins: Option<String>,
}

impl DispatchContext {
    pub fn new() -> Self {
        Self {
            v: DISPATCH_CONTEXT_VERSION,
            ..Self::default()
        }
    }

    /// Strict parse of a daemon-authored payload. Unknown fields and unknown
    /// versions are errors, not input to tolerate.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let ctx: Self =
            serde_json::from_str(raw).map_err(|e| format!("invalid dispatch context: {e}"))?;
        if ctx.v != DISPATCH_CONTEXT_VERSION {
            return Err(format!(
                "unsupported dispatch context version {} (expected {})",
                ctx.v, DISPATCH_CONTEXT_VERSION
            ));
        }
        Ok(ctx)
    }

    /// Whether the payload carries anything renderable at all.
    pub fn is_empty(&self) -> bool {
        self.persona.is_none()
            && self.directives.is_empty()
            && self.scope.is_none()
            && self.pins.is_none()
    }

    /// The directives that may render given the current scope state: when no
    /// scope exists, `needs_scope` directives are dropped — they instruct the
    /// model to copy correlation keys from a block that would not render.
    pub fn effective_directives(&self) -> impl Iterator<Item = &DispatchDirective> {
        let has_scope = self.scope.as_ref().is_some_and(|s| !s.is_empty());
        self.directives
            .iter()
            .filter(move |d| has_scope || !d.needs_scope)
    }
}

/// One daemon-selected directive plus its declared reinforcement need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchDirective {
    /// Stable label for diffing/debugging (e.g. `recall`, `contract`).
    pub id: String,
    /// Declared reinforcement cadence; placement is the harness's per-transport
    /// concern.
    pub cadence: DirectiveCadence,
    /// The text references the scope block's correlation keys; drop the
    /// directive whenever no current scope exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_scope: bool,
    pub text: String,
}

/// Reinforcement cadence the daemon declares per directive. The values carry
/// empirical calibration (e.g. session-start guidance attention-decays
/// within-session on some models while per-turn injection survives); the
/// harness honors them in each transport's native lane without interpreting
/// the directive text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveCadence {
    /// Deliver once per request at system authority (the default).
    Standing,
    /// Uncached per-request reinforcement in the transport's volatile lane.
    PerTurn,
}

/// Pre-bound scoping IDs, field-per-key. Rendering order is fixed: task first
/// (the stable correlation key), then session/project/bro/thread/work_item.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bro: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item: Option<String>,
}

impl DispatchScope {
    pub fn is_empty(&self) -> bool {
        self.fields().is_empty()
    }

    /// Ordered (key, value) pairs for rendering. Task first — it is the
    /// correlation key the completion contract tells the model to copy.
    pub fn fields(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        for (key, value) in [
            ("task", &self.task),
            ("session", &self.session),
            ("project", &self.project),
            ("bro", &self.bro),
            ("thread", &self.thread),
            ("work_item", &self.work_item),
        ] {
            if let Some(v) = value.as_deref()
                && !v.trim().is_empty()
            {
                out.push((key, v));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_full_payload() {
        let ctx = DispatchContext {
            v: 1,
            persona: Some("You are a reviewer".into()),
            directives: vec![
                DispatchDirective {
                    id: "recall".into(),
                    cadence: DirectiveCadence::PerTurn,
                    needs_scope: false,
                    text: "Recall: …".into(),
                },
                DispatchDirective {
                    id: "contract".into(),
                    cadence: DirectiveCadence::Standing,
                    needs_scope: true,
                    text: "If something notable…".into(),
                },
            ],
            scope: Some(DispatchScope {
                task: Some("task-1".into()),
                session: Some("sess-1".into()),
                ..Default::default()
            }),
            pins: Some("pin text".into()),
        };
        let raw = serde_json::to_string(&ctx).unwrap();
        assert_eq!(DispatchContext::parse(&raw).unwrap(), ctx);
    }

    #[test]
    fn parse_rejects_unknown_version() {
        let err = DispatchContext::parse(r#"{"v": 2}"#).unwrap_err();
        assert!(
            err.contains("unsupported dispatch context version 2"),
            "{err}"
        );
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let err = DispatchContext::parse(r#"{"v": 1, "extra": true}"#).unwrap_err();
        assert!(err.contains("invalid dispatch context"), "{err}");
        let err =
            DispatchContext::parse(r#"{"v":1,"scope":{"task":"t","bogus":"x"}}"#).unwrap_err();
        assert!(err.contains("invalid dispatch context"), "{err}");
        let err = DispatchContext::parse(
            r#"{"v":1,"directives":[{"id":"a","cadence":"standing","text":"t","priority":9}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("invalid dispatch context"), "{err}");
    }

    #[test]
    fn parse_rejects_unknown_cadence() {
        let err = DispatchContext::parse(
            r#"{"v":1,"directives":[{"id":"a","cadence":"hourly","text":"t"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("invalid dispatch context"), "{err}");
    }

    #[test]
    fn effective_directives_drop_needs_scope_without_scope() {
        let mut ctx = DispatchContext::new();
        ctx.directives = vec![
            DispatchDirective {
                id: "recall".into(),
                cadence: DirectiveCadence::PerTurn,
                needs_scope: false,
                text: "r".into(),
            },
            DispatchDirective {
                id: "contract".into(),
                cadence: DirectiveCadence::Standing,
                needs_scope: true,
                text: "c".into(),
            },
        ];
        let ids: Vec<_> = ctx.effective_directives().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["recall"]);

        ctx.scope = Some(DispatchScope {
            task: Some("t".into()),
            ..Default::default()
        });
        let ids: Vec<_> = ctx.effective_directives().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["recall", "contract"]);

        // An all-empty scope object counts as no scope.
        ctx.scope = Some(DispatchScope::default());
        let ids: Vec<_> = ctx.effective_directives().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["recall"]);
    }

    #[test]
    fn scope_fields_order_task_first_and_skip_blank() {
        let scope = DispatchScope {
            task: Some("t-1".into()),
            session: Some("  ".into()),
            project: Some("/repo".into()),
            bro: None,
            thread: Some("th-1".into()),
            work_item: None,
        };
        assert_eq!(
            scope.fields(),
            vec![("task", "t-1"), ("project", "/repo"), ("thread", "th-1")]
        );
    }
}
