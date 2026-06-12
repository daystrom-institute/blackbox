//! Harness-side dispatch-context state: resolution of the `--dispatch-context`
//! boundary flag, the per-transport composition strategy seam, scope/pins
//! fragments, and the session side-state cells
//! (design/bro-harness/dispatch-prompt-slots.md §4/§5/§7).

use serde_json::{Value, json};

pub use bro_protocol::{DirectiveCadence, DispatchContext, DispatchDirective, DispatchScope};

use super::{ContextualUserFragment, FragmentRole};
use crate::transport::TransportKind;

/// Per-transport composition strategy — the harness analog of opencode's
/// `provider(model)` + delivery branch and codex's PromptSlot router. One
/// routing point decides which slot each semantic class (persona, standing
/// directives, per-turn directives, memory, scope, pins, environment, task)
/// lands in; a per-provider fix becomes a strategy-arm change, not preamble
/// surgery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositionStrategy {
    /// anthropic + openai-responses: persona/standing directives in the
    /// stable system slot; per-turn directives in the volatile tail;
    /// memory/scope/pins/environment as marker-demarcated contextual USER
    /// fragments with change/compaction re-emit; the task is its own user
    /// item, verbatim, last.
    CodexShaped,
    /// openai-chat (the Mistral lane): everything — persona, standing
    /// directives, memory (AGENTS.md), environment, scope, pins — folds into
    /// the leading system message, rebuilt in place per request (vibe's
    /// `update_system_prompt` shape); the task is the only initial user
    /// message. The observed failure mode this fixes: policy and memory text
    /// in the user lane competing with the task ("no task provided",
    /// gap-00efeb12).
    VibeShaped,
}

impl CompositionStrategy {
    pub fn for_transport(kind: TransportKind) -> Self {
        match kind {
            TransportKind::OpenAiChat => Self::VibeShaped,
            TransportKind::Anthropic | TransportKind::OpenAiResponses => Self::CodexShaped,
        }
    }

    /// Whether memory/environment/scope/pins ride the contextual-user lane.
    /// When false they resolve to the stable system slot and the
    /// initial-context emitter contributes NOTHING to the user lane.
    pub fn context_rides_user_lane(self) -> bool {
        matches!(self, Self::CodexShaped)
    }
}

/// How the `--dispatch-context` flag resolved for this session run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchContextArg {
    /// Flag present, non-empty: the payload replaces persisted
    /// persona/directives/pins wholesale and sets the current scope.
    /// Boxed: the payload dwarfs the unit variants.
    Provided(Box<DispatchContext>),
    /// Flag present but empty (`""`/`{}`): explicit clear.
    Clear,
    /// Flag absent: restore persona/pins/directives from session side-state;
    /// scope is NEVER restored (per-dispatch correlation data).
    Absent,
}

/// Resolve the flag (or its standalone-binary env fallback,
/// `BRO_HARNESS_DISPATCH_CONTEXT`). Strict parse: the payload is
/// daemon-authored, so garbage fails the session rather than degrading.
pub fn resolve_dispatch_context_arg(flag: Option<&str>) -> Result<DispatchContextArg, String> {
    let raw = match flag {
        Some(s) => Some(s.to_string()),
        None => std::env::var("BRO_HARNESS_DISPATCH_CONTEXT")
            .ok()
            .filter(|s| !s.is_empty()),
    };
    match raw.as_deref() {
        None => Ok(DispatchContextArg::Absent),
        Some(s) if s.trim().is_empty() || s.trim() == "{}" => Ok(DispatchContextArg::Clear),
        Some(s) => DispatchContext::parse(s).map(|ctx| DispatchContextArg::Provided(Box::new(ctx))),
    }
}

/// Session-lived dispatch-context state: the current in-memory context plus
/// the last-emitted user-lane baselines (codex-shaped strategies only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchState {
    /// Current context. `scope` is `Some` only when THIS run's flag supplied
    /// one — never restored.
    pub context: Option<DispatchContext>,
    /// Rendered `<bbox_scope>` fragment last emitted into the user lane.
    /// Survives restore so a resume that re-passes an identical scope emits
    /// nothing.
    pub emitted_scope: Option<String>,
    /// Rendered `<bbox_pins>` fragment last emitted into the user lane.
    pub emitted_pins: Option<String>,
}

impl DispatchState {
    /// Build from the resolved flag plus the persisted side cells.
    pub fn from_arg(arg: DispatchContextArg, prior_side: &Value) -> Self {
        let emitted = prior_side.get("dispatch_emitted").unwrap_or(&Value::Null);
        let emitted_scope = emitted["scope"].as_str().map(str::to_string);
        let emitted_pins = emitted["pins"].as_str().map(str::to_string);
        match arg {
            DispatchContextArg::Provided(ctx) => Self {
                context: Some(*ctx),
                emitted_scope,
                emitted_pins,
            },
            DispatchContextArg::Clear => Self::default(),
            DispatchContextArg::Absent => {
                // Side-cell convention: tolerant restore (absent/garbage →
                // empty). Scope is structurally absent from the persisted
                // form, but force-drop it anyway for defense in depth.
                let restored = prior_side
                    .get("dispatch_context")
                    .and_then(|v| serde_json::from_value::<DispatchContext>(v.clone()).ok())
                    .map(|mut ctx| {
                        ctx.scope = None;
                        ctx
                    })
                    .filter(|ctx| !ctx.is_empty());
                Self {
                    context: restored,
                    emitted_scope,
                    emitted_pins,
                }
            }
        }
    }

    /// Persisted form of the context cell: the context minus scope (NEVER
    /// restorable), `Null` when empty/cleared.
    pub fn context_to_side(&self) -> Value {
        match &self.context {
            Some(ctx) => {
                let mut persisted = ctx.clone();
                persisted.scope = None;
                if persisted.is_empty() {
                    Value::Null
                } else {
                    serde_json::to_value(&persisted).unwrap_or(Value::Null)
                }
            }
            None => Value::Null,
        }
    }

    /// Persisted form of the last-emitted user-lane baselines.
    pub fn emitted_to_side(&self) -> Value {
        if self.emitted_scope.is_none() && self.emitted_pins.is_none() {
            return Value::Null;
        }
        json!({
            "scope": self.emitted_scope,
            "pins": self.emitted_pins,
        })
    }

    pub fn persona(&self) -> Option<&str> {
        self.context.as_ref().and_then(|c| c.persona.as_deref())
    }

    fn directive_text(&self, cadence: DirectiveCadence) -> Option<String> {
        let ctx = self.context.as_ref()?;
        let parts: Vec<&str> = ctx
            .effective_directives()
            .filter(|d| d.cadence == cadence)
            .map(|d| d.text.as_str())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// Joined standing directives (system authority, once per request).
    pub fn standing_text(&self) -> Option<String> {
        self.directive_text(DirectiveCadence::Standing)
    }

    /// Joined per-turn directives (uncached volatile-lane reinforcement).
    pub fn per_turn_text(&self) -> Option<String> {
        self.directive_text(DirectiveCadence::PerTurn)
    }

    /// Rendered `<bbox_scope>` fragment for the current scope, if any.
    pub fn scope_render(&self) -> Option<String> {
        let scope = self.context.as_ref()?.scope.as_ref()?;
        if scope.is_empty() {
            return None;
        }
        Some(ScopeFragment { scope }.render())
    }

    /// Rendered `<bbox_pins>` fragment for the current pin block, if any.
    pub fn pins_render(&self) -> Option<String> {
        let pins = self.context.as_ref()?.pins.as_deref()?;
        if pins.trim().is_empty() {
            return None;
        }
        Some(PinsFragment { pins }.render())
    }
}

/// Pre-bound scoping IDs, demarcated so the completion contract's "copy
/// `task:` from the `bbox_scope` context block" reference resolves under both
/// the contextual-user and system-section renderings.
struct ScopeFragment<'a> {
    scope: &'a DispatchScope,
}

impl ContextualUserFragment for ScopeFragment<'_> {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("<bbox_scope>", "</bbox_scope>")
    }

    fn body(&self) -> String {
        let lines: Vec<String> = self
            .scope
            .fields()
            .into_iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        format!("\n{}\n", lines.join("\n"))
    }
}

/// Scoped active-arc pin block (bbox_pin), demarcated.
struct PinsFragment<'a> {
    pins: &'a str,
}

impl ContextualUserFragment for PinsFragment<'_> {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("<bbox_pins>", "</bbox_pins>")
    }

    fn body(&self) -> String {
        format!("\n{}\n", self.pins.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(scope: Option<DispatchScope>) -> DispatchContext {
        DispatchContext {
            v: 1,
            persona: Some("You are a reviewer".into()),
            directives: vec![
                DispatchDirective {
                    id: "recall".into(),
                    cadence: DirectiveCadence::PerTurn,
                    needs_scope: false,
                    text: "Recall directive".into(),
                },
                DispatchDirective {
                    id: "task_shape".into(),
                    cadence: DirectiveCadence::Standing,
                    needs_scope: false,
                    text: "Task-shape check".into(),
                },
                DispatchDirective {
                    id: "contract".into(),
                    cadence: DirectiveCadence::Standing,
                    needs_scope: true,
                    text: "Completion contract".into(),
                },
                DispatchDirective {
                    id: "milestone".into(),
                    cadence: DirectiveCadence::PerTurn,
                    needs_scope: true,
                    text: "Milestone reporting".into(),
                },
            ],
            scope,
            pins: Some("pin block".into()),
        }
    }

    fn full_scope() -> DispatchScope {
        DispatchScope {
            task: Some("task-1".into()),
            session: Some("sess-1".into()),
            project: Some("/repo".into()),
            ..Default::default()
        }
    }

    #[test]
    fn strategy_keyed_by_transport() {
        assert_eq!(
            CompositionStrategy::for_transport(TransportKind::Anthropic),
            CompositionStrategy::CodexShaped
        );
        assert_eq!(
            CompositionStrategy::for_transport(TransportKind::OpenAiResponses),
            CompositionStrategy::CodexShaped
        );
        assert_eq!(
            CompositionStrategy::for_transport(TransportKind::OpenAiChat),
            CompositionStrategy::VibeShaped
        );
    }

    #[test]
    fn resolve_arg_provided_clear_absent() {
        let ctx = ctx_with(Some(full_scope()));
        let raw = serde_json::to_string(&ctx).unwrap();
        assert_eq!(
            resolve_dispatch_context_arg(Some(&raw)).unwrap(),
            DispatchContextArg::Provided(Box::new(ctx))
        );
        assert_eq!(
            resolve_dispatch_context_arg(Some("")).unwrap(),
            DispatchContextArg::Clear
        );
        assert_eq!(
            resolve_dispatch_context_arg(Some("  {} ")).unwrap(),
            DispatchContextArg::Clear
        );
        assert_eq!(
            resolve_dispatch_context_arg(None).unwrap(),
            DispatchContextArg::Absent
        );
    }

    #[test]
    fn resolve_arg_rejects_garbage_strictly() {
        assert!(resolve_dispatch_context_arg(Some("not json")).is_err());
        assert!(resolve_dispatch_context_arg(Some(r#"{"v":1,"nope":2}"#)).is_err());
        assert!(resolve_dispatch_context_arg(Some(r#"{"v":9}"#)).is_err());
    }

    #[test]
    fn provided_replaces_and_sets_scope() {
        let state = DispatchState::from_arg(
            DispatchContextArg::Provided(Box::new(ctx_with(Some(full_scope())))),
            &Value::Null,
        );
        assert!(state.scope_render().is_some());
        assert_eq!(state.persona(), Some("You are a reviewer"));
        // All four directives effective with scope present.
        assert_eq!(
            state.standing_text().unwrap(),
            "Task-shape check\n\nCompletion contract"
        );
        assert_eq!(
            state.per_turn_text().unwrap(),
            "Recall directive\n\nMilestone reporting"
        );
    }

    #[test]
    fn restore_round_trip_excludes_scope_and_drops_needs_scope() {
        let provided = DispatchState::from_arg(
            DispatchContextArg::Provided(Box::new(ctx_with(Some(full_scope())))),
            &Value::Null,
        );
        let side = json!({
            "dispatch_context": provided.context_to_side(),
            "dispatch_emitted": provided.emitted_to_side(),
        });
        let restored = DispatchState::from_arg(DispatchContextArg::Absent, &side);
        let ctx = restored.context.as_ref().expect("context restored");
        assert_eq!(ctx.scope, None, "scope must NEVER be restored");
        assert_eq!(restored.persona(), Some("You are a reviewer"));
        assert_eq!(
            restored.context.as_ref().unwrap().pins.as_deref(),
            Some("pin block")
        );
        // needs_scope directives drop without a current scope.
        assert_eq!(restored.standing_text().unwrap(), "Task-shape check");
        assert_eq!(restored.per_turn_text().unwrap(), "Recall directive");
        assert_eq!(restored.scope_render(), None);
    }

    #[test]
    fn emitted_baselines_survive_restore() {
        let side = json!({
            "dispatch_emitted": {"scope": "<bbox_scope>\ntask: t\n</bbox_scope>", "pins": null},
        });
        let restored = DispatchState::from_arg(DispatchContextArg::Absent, &side);
        assert_eq!(
            restored.emitted_scope.as_deref(),
            Some("<bbox_scope>\ntask: t\n</bbox_scope>")
        );
        assert_eq!(restored.emitted_pins, None);
    }

    #[test]
    fn clear_wipes_context_and_baselines() {
        let side = json!({
            "dispatch_context": {"v": 1, "persona": "p"},
            "dispatch_emitted": {"scope": "s", "pins": "p"},
        });
        let state = DispatchState::from_arg(DispatchContextArg::Clear, &side);
        assert_eq!(state, DispatchState::default());
        assert_eq!(state.context_to_side(), Value::Null);
        assert_eq!(state.emitted_to_side(), Value::Null);
    }

    #[test]
    fn restore_is_tolerant_of_garbage() {
        let side = json!({"dispatch_context": {"v": 99, "bogus": true}});
        let state = DispatchState::from_arg(DispatchContextArg::Absent, &side);
        assert_eq!(state.context, None);
    }

    #[test]
    fn scope_fragment_renders_markers_and_ordered_fields() {
        let state = DispatchState::from_arg(
            DispatchContextArg::Provided(Box::new(ctx_with(Some(full_scope())))),
            &Value::Null,
        );
        let rendered = state.scope_render().unwrap();
        assert_eq!(
            rendered,
            "<bbox_scope>\ntask: task-1\nsession: sess-1\nproject: /repo\n</bbox_scope>"
        );
        let pins = state.pins_render().unwrap();
        assert_eq!(pins, "<bbox_pins>\npin block\n</bbox_pins>");
    }

    #[test]
    fn empty_scope_renders_nothing() {
        let state = DispatchState::from_arg(
            DispatchContextArg::Provided(Box::new(ctx_with(Some(DispatchScope::default())))),
            &Value::Null,
        );
        assert_eq!(state.scope_render(), None);
    }
}
