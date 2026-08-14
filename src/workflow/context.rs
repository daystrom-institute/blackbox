//! Arc execution context — the unified entity carried through every
//! node, hook, gate, and packet evaluation in a workflow.
//!
//! Three-layer namespace:
//! - `vars`         — mutable, hook-writable, schema-validated
//! - `outputs`      — per-node return values (immutable once written)
//! - `meta`         — arc-intrinsic metadata (id, names, worktree, …)
//! - `last_signal`  — most-recent Wait resolution payload (cleared on
//!                    next Wait dispatch, not on node boundary; the
//!                    immediate-successor gate needs it)
//!
//! Templates use `${vars.x}`, `${outputs.NodeName}`,
//! `${outputs.NodeName.field.sub}`, `${meta.field}`,
//! `${last_signal.name}`, `${last_signal.payload.field}`.
//!
//! Packet entity construction flattens this whole structure into one
//! JSON value so any predicate AST evaluator (gate, policy, hook
//! `when`, shell policy, webhook routing) sees the same shape.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Per-arc runtime state shared by every node, hook, and packet
/// evaluation. Owned by `WorkflowRunner` and threaded through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArcContext {
    pub vars: Map<String, Value>,
    pub outputs: HashMap<String, Value>,
    /// Per-node actor task envelopes: `task_result_json` for each actor
    /// node that has dispatched. Populated regardless of terminal
    /// outcome (success / failed / cancelled / timed_out) when the
    /// node is configured `actor_failure: continue`. Lets downstream
    /// nodes inspect `taskId`, `status`, `costUsd`, `result`, etc.
    /// for branching, summary rendering, and state-machine bookkeeping.
    #[serde(default)]
    pub actor_results: HashMap<String, Value>,
    pub meta: ArcMeta,
    /// Most recent Wait resolution. Cleared when the next Wait
    /// dispatches (not on node boundary) so the immediate-successor
    /// gate can branch on which signal arrived.
    pub last_signal: Option<SignalRef>,
    /// Append-only signal history for arcs that need to look back.
    /// Hooks read this; gates typically only need `last_signal`.
    #[serde(default)]
    pub signal_history: Vec<SignalRef>,
    /// System-event ids this arc has already consumed through the
    /// Wait-registration ledger catch-up. Prevents a wait node revisited
    /// in a loop (or re-entered on daemon-restart rehydration) from
    /// resolving against the same persisted event twice. Bounded FIFO;
    /// see `record_consumed_signal_event`.
    #[serde(default)]
    pub consumed_signal_event_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArcMeta {
    pub arc_id: String,
    pub workflow_name: String,
    pub workflow_version: u32,
    pub started_at: String,
    /// Project directory the arc was launched against. Inherited by
    /// every dispatch unless overridden.
    pub project_dir: Option<String>,
    /// Per-arc worktree root. Hook ops set this; subsequent dispatches
    /// run with `cwd = meta.worktree` automatically.
    pub worktree: Option<String>,
    /// Set by the engine at terminal state: success / failed /
    /// cancelled / timeout. Used by `on_arc_exit` hook gates.
    pub arc_outcome: Option<String>,
    /// Parent arc id for sub-workflows. Set by sub-runner construction.
    pub parent_arc_id: Option<String>,
    /// Composition depth (top-level = 0, sub-of-top = 1, …).
    pub composition_depth: u32,
}

/// One Wait resolution. Carried in `last_signal` and `signal_history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRef {
    pub name: String,
    pub payload: Value,
    pub correlation: Map<String, Value>,
    pub received_at: String,
}

/// Schema entry for a single arc variable. `kind` is enforced when a
/// hook (or initial_vars seeding) writes to that key — type-mismatched
/// writes return `Err`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarSchemaEntry {
    pub kind: VarKind,
    /// When true, the variable must be present at terminal state. Used
    /// by `exports` validation in subworkflows.
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VarKind {
    /// Any JSON value. Use sparingly — prefer concrete kinds.
    Any,
    Bool,
    Int,
    Float,
    String,
    Array,
    Object,
}

impl VarKind {
    pub fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (VarKind::Any, _) => true,
            (VarKind::Bool, Value::Bool(_)) => true,
            (VarKind::Int, Value::Number(n)) => n.is_i64() || n.is_u64(),
            (VarKind::Float, Value::Number(_)) => true,
            (VarKind::String, Value::String(_)) => true,
            (VarKind::Array, Value::Array(_)) => true,
            (VarKind::Object, Value::Object(_)) => true,
            // Treat null as "not yet set" — schema match is permissive
            // here so initialization doesn't have to write a typed
            // sentinel before a real value lands.
            (_, Value::Null) => true,
            _ => false,
        }
    }
}

pub type VarsSchema = HashMap<String, VarSchemaEntry>;

impl ArcContext {
    pub fn new(meta: ArcMeta) -> Self {
        Self {
            vars: Map::new(),
            outputs: HashMap::new(),
            actor_results: HashMap::new(),
            meta,
            last_signal: None,
            signal_history: Vec::new(),
            consumed_signal_event_ids: Vec::new(),
        }
    }

    /// Record the full task envelope returned by an actor node. Called
    /// by the engine after every actor dispatch, regardless of terminal
    /// outcome. Downstream nodes (and gate packets) read fields off
    /// `actor_results.<NodeId>`: `taskId`, `status`, `costUsd`,
    /// `result`, `sessionId`, etc. See `orch::task_result_json` for
    /// the canonical shape.
    pub fn record_actor_result(&mut self, node_id: &str, value: Value) {
        self.actor_results.insert(node_id.to_string(), value);
    }

    /// Validate-and-write a single var. Returns Err if a schema is
    /// provided and the value's kind doesn't match the schema entry.
    pub fn set_var(&mut self, key: &str, value: Value, schema: Option<&VarsSchema>) -> Result<()> {
        if let Some(s) = schema {
            if let Some(entry) = s.get(key) {
                if !entry.kind.matches(&value) {
                    bail!(
                        "vars write rejected: '{key}' expects {:?}, got {}",
                        entry.kind,
                        value_kind_name(&value)
                    );
                }
            }
        }
        self.vars.insert(key.to_string(), value);
        Ok(())
    }

    /// Bulk seed vars from initial_vars passed at arc start. Validates
    /// every entry against schema.
    pub fn seed_vars(
        &mut self,
        initial: Map<String, Value>,
        schema: Option<&VarsSchema>,
    ) -> Result<()> {
        for (k, v) in initial {
            self.set_var(&k, v, schema)?;
        }
        Ok(())
    }

    /// Required-vars check. Used at terminal state to verify that
    /// declared exports / required vars are populated. Returns the
    /// list of missing keys (empty = OK).
    pub fn missing_required_vars(&self, schema: &VarsSchema) -> Vec<String> {
        schema
            .iter()
            .filter(|(_, e)| e.required)
            .filter(|(k, _)| self.vars.get(*k).is_none_or(|v| matches!(v, Value::Null)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Record a node output. LLM string outputs come through here as
    /// `Value::String`; structured-output nodes write the parsed JSON
    /// directly.
    pub fn set_output(&mut self, node_id: &str, value: Value) {
        self.outputs.insert(node_id.to_string(), value);
    }

    /// Record a Wait resolution. Updates `last_signal` AND appends to
    /// `signal_history`.
    pub fn record_signal(&mut self, sig: SignalRef) {
        self.last_signal = Some(sig.clone());
        self.signal_history.push(sig);
    }

    /// Clear `last_signal` (called when the next Wait dispatches so
    /// stale signal references can't leak into a subsequent gate). The
    /// history is preserved.
    pub fn clear_last_signal(&mut self) {
        self.last_signal = None;
    }

    /// Ceiling for `consumed_signal_event_ids`. Old entries age out
    /// FIFO; an arc that loops through more distinct ledger events than
    /// this between two visits of the same wait correlation would be
    /// pathological.
    const CONSUMED_SIGNAL_EVENT_CAP: usize = 256;

    /// Remember that a persisted system event resolved one of this
    /// arc's waits, so ledger catch-up never consumes it again.
    pub fn record_consumed_signal_event(&mut self, event_id: &str) {
        if self.signal_event_consumed(event_id) {
            return;
        }
        self.consumed_signal_event_ids.push(event_id.to_string());
        if self.consumed_signal_event_ids.len() > Self::CONSUMED_SIGNAL_EVENT_CAP {
            let excess = self.consumed_signal_event_ids.len() - Self::CONSUMED_SIGNAL_EVENT_CAP;
            self.consumed_signal_event_ids.drain(..excess);
        }
    }

    pub fn signal_event_consumed(&self, event_id: &str) -> bool {
        self.consumed_signal_event_ids
            .iter()
            .any(|id| id == event_id)
    }

    /// Flatten the full context into one JSON Value. Used as the
    /// entity for packet evaluation — gate, policy, hook `when`, shell
    /// policy, webhook routing all see the same shape:
    ///
    /// ```text
    /// {
    ///   "vars":    { ... },
    ///   "outputs": { node_id: <value>, ... },
    ///   "meta":    { arc_id, workflow_name, ... },
    ///   "last_signal": { name, payload, correlation, received_at }?,
    ///   "signals_count": N
    /// }
    /// ```
    ///
    /// Per-node gate evaluation calls `flatten_for_gate` which adds
    /// `node_output` (the dispatching node's output) at the top level
    /// for backward-compatible rule writing. Policy packets see the
    /// raw flatten without node_output.
    pub fn flatten(&self) -> Value {
        json!({
            "vars": self.vars,
            "outputs": self.outputs,
            "actor_results": self.actor_results,
            "meta": self.meta,
            "last_signal": self.last_signal,
            "signals_count": self.signal_history.len(),
        })
    }

    /// Flatten variant for node-gate evaluation: same as `flatten()`
    /// but exposes the just-completed node's output at the top level
    /// as `node_output` (string coerced for legacy rules) and as
    /// `node_output_json` (raw value).
    pub fn flatten_for_gate(&self, node_id: &str) -> Value {
        let raw = self.outputs.get(node_id).cloned().unwrap_or(Value::Null);
        let str_form = match &raw {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        json!({
            "vars": self.vars,
            "outputs": self.outputs,
            "actor_results": self.actor_results,
            "meta": self.meta,
            "last_signal": self.last_signal,
            "node_output": str_form,
            "node_output_json": raw,
            "node_id": node_id,
        })
    }

    /// Render a `${path.to.value}` template against this context.
    ///
    /// Resolution order for `${head.tail…}`:
    /// 1. `head == "vars"`     → walk into `vars`
    /// 2. `head == "outputs"`  → walk into `outputs`
    /// 3. `head == "meta"`     → walk into `meta`
    /// 4. `head == "last_signal"` → walk into `last_signal`
    /// 5. `head == "env"`      → process env var lookup (`${env.NAME}`)
    /// 6. legacy `${NodeName.output}` form → equivalent to
    ///    `${outputs.NodeName}` when `tail == "output"`; this preserves
    ///    every existing template in the codebase.
    ///
    /// Unresolved expressions are left in place verbatim so misspelled
    /// references are visible in the dispatched prompt rather than
    /// silently turning into the empty string.
    pub fn render_template(&self, template: &str) -> String {
        let mut roots = serde_json::Map::new();
        roots.insert("vars".to_string(), json!(self.vars));
        roots.insert("outputs".to_string(), json!(self.outputs));
        roots.insert("actor_results".to_string(), json!(self.actor_results));
        roots.insert(
            "meta".to_string(),
            serde_json::to_value(&self.meta).unwrap_or_default(),
        );
        if let Some(ref sig) = self.last_signal {
            roots.insert(
                "last_signal".to_string(),
                serde_json::to_value(sig).unwrap_or_default(),
            );
        }
        crate::system_events::template::render_template_with_roots(
            template,
            &roots,
            crate::system_events::template::UnresolvedPolicy::LeaveVerbatim,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("template render error (LeaveVerbatim should not error): {e}");
            template.to_string()
        })
    }

    /// Resolve one path expression to a value. Public so hook arg
    /// rendering can resolve into `Value` instead of going through
    /// stringification.
    pub fn resolve(&self, expr: &str) -> Option<Value> {
        let parts: Vec<&str> = expr.split('.').collect();
        if parts.is_empty() {
            return None;
        }
        match parts[0] {
            "vars" => walk_value(&Value::Object(self.vars.clone()), &parts[1..]),
            "outputs" => {
                if parts.len() == 1 {
                    return Some(json!(self.outputs));
                }
                let node = parts[1];
                let raw = self.outputs.get(node)?.clone();
                walk_value(&raw, &parts[2..])
            }
            "actor_results" => {
                if parts.len() == 1 {
                    return Some(json!(self.actor_results));
                }
                let node = parts[1];
                let raw = self.actor_results.get(node)?.clone();
                walk_value(&raw, &parts[2..])
            }
            "meta" => {
                let m = serde_json::to_value(&self.meta).ok()?;
                walk_value(&m, &parts[1..])
            }
            "last_signal" => {
                let s = serde_json::to_value(self.last_signal.as_ref()?).ok()?;
                walk_value(&s, &parts[1..])
            }
            "env" => {
                // ${env.NAME} — single-segment env lookup. Always
                // returns Value::String. Missing env yields None so
                // the templater leaves `${env.NAME}` verbatim — caller
                // sees the unresolved reference instead of a silent
                // empty substitution.
                if parts.len() != 2 {
                    return None;
                }
                std::env::var(parts[1]).ok().map(Value::String)
            }
            // Legacy ${NodeName.output} → outputs.NodeName
            other if parts.len() == 2 && parts[1] == "output" => self.outputs.get(other).cloned(),
            _ => None,
        }
    }
}

fn walk_value(root: &Value, path: &[&str]) -> Option<Value> {
    let mut cur = root.clone();
    for seg in path {
        cur = match &cur {
            Value::Object(m) => m.get(*seg).cloned()?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx).cloned()?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn find_close_brace(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn value_kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn resolve_arg_value(ctx: &ArcContext, raw: &Value) -> Result<Value> {
    match raw {
        Value::String(s) => {
            let trimmed = s.trim();
            let is_whole_string_template = trimmed.starts_with("${")
                && trimmed.ends_with('}')
                && find_close_brace(trimmed, 2) == Some(trimmed.len() - 1);
            if is_whole_string_template {
                let expr = &trimmed[2..trimmed.len() - 1];
                ctx.resolve(expr)
                    .ok_or_else(|| anyhow!("template expr '{expr}' did not resolve"))
            } else {
                Ok(Value::String(ctx.render_template(s)))
            }
        }
        Value::Array(items) => {
            let resolved: Result<Vec<_>> =
                items.iter().map(|i| resolve_arg_value(ctx, i)).collect();
            Ok(Value::Array(resolved?))
        }
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), resolve_arg_value(ctx, v)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ArcContext {
        let mut c = ArcContext::new(ArcMeta {
            arc_id: "arc-test".into(),
            workflow_name: "wf".into(),
            workflow_version: 1,
            started_at: "2026-04-26T00:00:00Z".into(),
            project_dir: Some("/repo/x".into()),
            worktree: Some("/tmp/wt-1".into()),
            arc_outcome: None,
            parent_arc_id: None,
            composition_depth: 0,
        });
        c.vars.insert("issue".into(), json!(42));
        c.vars.insert("repo".into(), json!("foo/bar"));
        c.vars.insert("labels".into(), json!(["bug", "urgent"]));
        c.outputs.insert(
            "Plan".into(),
            json!({"branch_name": "fix/issue-42", "summary": "do the thing"}),
        );
        c.outputs
            .insert("Implement".into(), json!("PR #117 opened"));
        c.last_signal = Some(SignalRef {
            name: "pr-merged".into(),
            payload: json!({"pr": 117, "merged_by": "alice"}),
            correlation: serde_json::Map::from_iter([("pr".to_string(), json!(117))]),
            received_at: "2026-04-26T00:01:00Z".into(),
        });
        c
    }

    #[test]
    fn resolves_vars_path() {
        let c = ctx();
        assert_eq!(c.resolve("vars.issue"), Some(json!(42)));
        assert_eq!(c.resolve("vars.repo"), Some(json!("foo/bar")));
    }

    #[test]
    fn resolves_array_index() {
        let c = ctx();
        assert_eq!(c.resolve("vars.labels.0"), Some(json!("bug")));
        assert_eq!(c.resolve("vars.labels.1"), Some(json!("urgent")));
        assert_eq!(c.resolve("vars.labels.5"), None);
    }

    #[test]
    fn resolves_outputs_field() {
        let c = ctx();
        assert_eq!(
            c.resolve("outputs.Plan.branch_name"),
            Some(json!("fix/issue-42"))
        );
    }

    #[test]
    fn legacy_node_output_form() {
        let c = ctx();
        assert_eq!(c.resolve("Implement.output"), Some(json!("PR #117 opened")));
    }

    #[test]
    fn meta_path() {
        let c = ctx();
        assert_eq!(c.resolve("meta.arc_id"), Some(json!("arc-test")));
        assert_eq!(c.resolve("meta.worktree"), Some(json!("/tmp/wt-1")));
    }

    #[test]
    fn signal_path() {
        let c = ctx();
        assert_eq!(c.resolve("last_signal.name"), Some(json!("pr-merged")));
        assert_eq!(
            c.resolve("last_signal.payload.merged_by"),
            Some(json!("alice"))
        );
    }

    #[test]
    fn render_template_substitutes() {
        let c = ctx();
        let rendered = c.render_template(
            "Issue ${vars.issue} in ${vars.repo}: branch=${outputs.Plan.branch_name}",
        );
        assert_eq!(rendered, "Issue 42 in foo/bar: branch=fix/issue-42");
    }

    #[test]
    fn render_template_preserves_unresolved() {
        let c = ctx();
        let rendered = c.render_template("hello ${vars.does_not_exist} world");
        assert_eq!(rendered, "hello ${vars.does_not_exist} world");
    }

    #[test]
    fn render_template_legacy_node_output() {
        let c = ctx();
        let rendered = c.render_template("prior: ${Implement.output}");
        assert_eq!(rendered, "prior: PR #117 opened");
    }

    #[test]
    fn render_template_resolves_env_head() {
        let _env = crate::util::test_env_lock();
        let c = ctx();
        unsafe {
            std::env::set_var("CTX_TEST_HEAD", "hello");
        }
        let rendered = c.render_template("${env.CTX_TEST_HEAD} world");
        assert_eq!(rendered, "hello world");
    }

    #[test]
    fn resolve_arg_value_whole_string_keeps_typing() {
        // Single ${vars.n} returns Value::Number, not stringified.
        let c = ctx();
        let raw = json!("${vars.issue}");
        let resolved = resolve_arg_value(&c, &raw).unwrap();
        assert_eq!(resolved, json!(42));
    }

    #[test]
    fn resolve_arg_value_multi_interpolation_renders_as_string() {
        let _env = crate::util::test_env_lock();
        // The bug: a URL with two interpolations starts with `${` and
        // ends with `}` but is NOT a single whole-string template.
        // Must render as text via the templater, not parsed as one
        // expression — that produced the broken
        // `'env.X}/path/${vars.y' did not resolve` error.
        let c = ctx();
        unsafe {
            std::env::set_var("CTX_TEST_BASE", "http://host:3000");
        }
        let raw = json!("${env.CTX_TEST_BASE}/repos/${vars.repo}/issues/${vars.issue}");
        let resolved = resolve_arg_value(&c, &raw).unwrap();
        assert_eq!(resolved, json!("http://host:3000/repos/foo/bar/issues/42"));
    }

    #[test]
    fn schema_validates_writes() {
        let mut c = ArcContext::new(ArcMeta::default());
        let mut schema = HashMap::new();
        schema.insert(
            "issue".to_string(),
            VarSchemaEntry {
                kind: VarKind::Int,
                required: false,
            },
        );
        // Correct kind — accepted.
        c.set_var("issue", json!(42), Some(&schema)).unwrap();
        // Wrong kind — rejected.
        let err = c.set_var("issue", json!("not an int"), Some(&schema));
        assert!(err.is_err());
        // Unknown key — accepted (schema is open by design; declare
        // explicit required fields to lock down).
        c.set_var("misc", json!("ok"), Some(&schema)).unwrap();
    }

    #[test]
    fn missing_required_vars() {
        let c = ArcContext::new(ArcMeta::default());
        let mut schema = HashMap::new();
        schema.insert(
            "pr_number".to_string(),
            VarSchemaEntry {
                kind: VarKind::Int,
                required: true,
            },
        );
        let missing = c.missing_required_vars(&schema);
        assert_eq!(missing, vec!["pr_number".to_string()]);
    }

    #[test]
    fn flatten_for_gate_includes_node_output() {
        let c = ctx();
        let entity = c.flatten_for_gate("Implement");
        assert_eq!(entity["node_id"], json!("Implement"));
        assert_eq!(entity["node_output"], json!("PR #117 opened"));
        assert_eq!(entity["vars"]["issue"], json!(42));
    }

    #[test]
    fn resolve_arg_value_typed_passthrough() {
        let c = ctx();
        // Whole-string template gives back the typed value (int).
        let v = resolve_arg_value(&c, &json!("${vars.issue}")).unwrap();
        assert_eq!(v, json!(42));
        // Mixed-string template stringifies.
        let v = resolve_arg_value(&c, &json!("issue-${vars.issue}")).unwrap();
        assert_eq!(v, json!("issue-42"));
        // Nested object resolves recursively.
        let v = resolve_arg_value(&c, &json!({"branch": "${vars.repo}", "n": "${vars.issue}"}))
            .unwrap();
        assert_eq!(v, json!({"branch": "foo/bar", "n": 42}));
    }

    #[test]
    fn record_and_clear_signal() {
        let mut c = ArcContext::new(ArcMeta::default());
        let s = SignalRef {
            name: "x".into(),
            payload: json!({}),
            correlation: Map::new(),
            received_at: "ts".into(),
        };
        c.record_signal(s.clone());
        assert!(c.last_signal.is_some());
        assert_eq!(c.signal_history.len(), 1);
        c.clear_last_signal();
        assert!(c.last_signal.is_none());
        // History preserved.
        assert_eq!(c.signal_history.len(), 1);
    }
}
