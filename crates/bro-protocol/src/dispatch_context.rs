//! Typed dispatch-context payload: the `--dispatch-context <json>` boundary
//! surface (design/bro-harness/dispatch-prompt-slots.md §4).
//!
//! The daemon owns CONTENT SELECTION: the persona resolved from the brofile
//! and the pre-bound scope IDs. The harness owns COMPOSITION: where each
//! ingredient lands per transport (system stable slot, marker-demarcated
//! contextual user fragments, or the vibe-shaped leading system block). This
//! DTO is the ingredients list that crosses that boundary: typed values,
//! never composed prose.
//!
//! Parsing is deliberately strict (`deny_unknown_fields`, exact version
//! match): the payload is daemon-authored, so garbage is a bug to surface,
//! not input to tolerate. The allowances are the shapes older daemons and
//! persisted harness side-state carry: a `pins` block and a `directives`
//! array are accepted and dropped.

use serde::{Deserialize, Serialize};

/// The only payload version this revision understands.
pub const DISPATCH_CONTEXT_VERSION: u32 = 1;

/// Typed ingredients for one dispatch. See module docs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "WireDispatchContext")]
pub struct DispatchContext {
    /// Payload version; must equal [`DISPATCH_CONTEXT_VERSION`].
    pub v: u32,
    /// Brofile lens (persona / role system-prompt), verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Pre-bound scoping IDs. Typed key->value fields, NOT pre-rendered lines;
    /// the harness renders (and re-renders) them. NEVER restored from session
    /// side-state: `task` is per-dispatch correlation data for thread and gap
    /// records, and a stale value would mis-correlate them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DispatchScope>,
}

/// Accepted input shape for [`DispatchContext`]: its fields plus the `pins`
/// block and `directives` array older payloads carry, both dropped on
/// conversion.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDispatchContext {
    v: u32,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    scope: Option<DispatchScope>,
    #[serde(default, rename = "directives")]
    _directives: Vec<serde::de::IgnoredAny>,
    #[serde(default, rename = "pins")]
    _pins: Option<serde::de::IgnoredAny>,
}

impl From<WireDispatchContext> for DispatchContext {
    fn from(wire: WireDispatchContext) -> Self {
        Self {
            v: wire.v,
            persona: wire.persona,
            scope: wire.scope,
        }
    }
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
        self.persona.is_none() && self.scope.is_none()
    }
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

    /// Ordered (key, value) pairs for rendering. Task first: it is the
    /// stable correlation key.
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
            scope: Some(DispatchScope {
                task: Some("task-1".into()),
                session: Some("sess-1".into()),
                ..Default::default()
            }),
        };
        let raw = serde_json::to_string(&ctx).unwrap();
        assert_eq!(DispatchContext::parse(&raw).unwrap(), ctx);
    }

    #[test]
    fn parse_accepts_and_drops_legacy_pins() {
        let ctx = DispatchContext::parse(
            r#"{"v":1,"persona":"p","scope":{"task":"t"},"pins":"- [bro:x] Active arc"}"#,
        )
        .unwrap();
        assert_eq!(ctx.persona.as_deref(), Some("p"));
        assert_eq!(
            ctx.scope.as_ref().and_then(|s| s.task.as_deref()),
            Some("t")
        );
        let reserialized = serde_json::to_value(&ctx).unwrap();
        assert!(reserialized.get("pins").is_none(), "{reserialized}");
    }

    #[test]
    fn parse_accepts_and_drops_legacy_directives_of_any_cadence() {
        let ctx = DispatchContext::parse(
            r#"{"v":1,"persona":"p","directives":[
                {"id":"recall","cadence":"per_turn","text":"r"},
                {"id":"contract","cadence":"standing","needs_scope":true,"text":"c"}
            ],"scope":{"task":"t"}}"#,
        )
        .unwrap();
        assert_eq!(
            ctx,
            DispatchContext {
                v: 1,
                persona: Some("p".into()),
                scope: Some(DispatchScope {
                    task: Some("t".into()),
                    ..Default::default()
                }),
            }
        );
        let reserialized = serde_json::to_value(&ctx).unwrap();
        assert_eq!(
            reserialized,
            serde_json::json!({"v": 1, "persona": "p", "scope": {"task": "t"}})
        );
        assert_eq!(
            DispatchContext::parse(&reserialized.to_string()).unwrap(),
            ctx
        );
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
        let err = DispatchContext::parse(r#"{"v":1,"directives":"standing"}"#).unwrap_err();
        assert!(err.contains("invalid dispatch context"), "{err}");
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
