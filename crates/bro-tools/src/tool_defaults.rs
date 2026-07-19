use crate::tool::ToolResult;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Default,
    Pin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    raw: String,
    glob_prefix: Option<String>,
}

impl Pattern {
    fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("tool pattern is empty".to_string());
        }
        let star_count = raw.matches('*').count();
        if star_count > 1 || (star_count == 1 && !raw.ends_with('*')) {
            return Err(format!(
                "tool pattern '{raw}' is invalid; only exact names or trailing '*' globs are allowed"
            ));
        }
        let glob_prefix = raw.strip_suffix('*').map(str::to_string);
        Ok(Self {
            raw: raw.to_string(),
            glob_prefix,
        })
    }

    fn is_glob(&self) -> bool {
        self.glob_prefix.is_some()
    }

    fn matches(&self, tool_name: &str) -> bool {
        tool_aliases(tool_name).iter().any(|alias| {
            if let Some(prefix) = &self.glob_prefix {
                alias.starts_with(prefix)
            } else {
                alias == &self.raw
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    key: String,
    flavor: Flavor,
    pattern: Pattern,
    param: String,
    value: String,
}

/// Host-supplied per-(tool,param) default and pin table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolArgDefaults {
    rules: Vec<Rule>,
}

/// Visible rider describing default/pin handling for a single tool call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolArgRider {
    pub defaults_applied: BTreeMap<String, Value>,
    pub pin_enforced: BTreeMap<String, Value>,
    pub pin_conflict: BTreeMap<String, Value>,
}

impl ToolArgRider {
    pub fn is_empty(&self) -> bool {
        self.defaults_applied.is_empty()
            && self.pin_enforced.is_empty()
            && self.pin_conflict.is_empty()
    }

    pub fn to_value(&self) -> Value {
        let mut obj = Map::new();
        if !self.defaults_applied.is_empty() {
            obj.insert(
                "defaults_applied".to_string(),
                Value::Object(self.defaults_applied.clone().into_iter().collect()),
            );
        }
        if !self.pin_enforced.is_empty() {
            obj.insert(
                "pin_enforced".to_string(),
                Value::Object(self.pin_enforced.clone().into_iter().collect()),
            );
        }
        if !self.pin_conflict.is_empty() {
            obj.insert(
                "pin_conflict".to_string(),
                Value::Object(self.pin_conflict.clone().into_iter().collect()),
            );
        }
        Value::Object(obj)
    }

    fn as_text_rider(&self) -> String {
        format!(
            "\n\ntool_arg_context: {}",
            serde_json::to_string(&self.to_value()).unwrap_or_else(|_| self.to_value().to_string())
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinConflict {
    pub param: String,
    pub expected: Value,
    pub actual: Value,
}

impl PinConflict {
    fn rider(&self) -> ToolArgRider {
        let mut rider = ToolArgRider::default();
        rider.pin_conflict.insert(
            self.param.clone(),
            json!({
                "expected": self.expected,
                "actual": self.actual,
            }),
        );
        rider
    }

    pub fn into_tool_result(self, tool_name: &str) -> ToolResult {
        let rider = self.rider();
        ToolResult::Error(format!(
            "pin conflict for tool '{tool_name}' param '{}': expected {}, got {}\n{}",
            self.param,
            self.expected,
            self.actual,
            serde_json::to_string(&rider.to_value())
                .unwrap_or_else(|_| rider.to_value().to_string())
        ))
    }
}

impl ToolArgDefaults {
    pub fn parse_map(raw: BTreeMap<String, String>) -> Result<Self, String> {
        let mut rules = Vec::new();
        for (key, value) in raw {
            rules.push(parse_rule(key, value)?);
        }
        Ok(Self { rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Host-side grant lookup for operator-authority flags (RX-V1): the value
    /// of the Default rule exactly matching (tool, param), if any. Bindings
    /// query this instead of reading merged tool input, so a cell-authored
    /// flag of the same name stays a schema error. Pin rules are enforcement,
    /// not grants, and are ignored here. Values are raw strings; bindings
    /// parse their own booleans.
    pub fn lookup(&self, tool_name: &str, param: &str) -> Option<&str> {
        self.selected_rules(tool_name, Flavor::Default)
            .into_iter()
            .find(|rule| rule.param == param)
            .map(|rule| rule.value.as_str())
    }

    pub fn apply(
        &self,
        tool_name: &str,
        input: Value,
    ) -> Result<(Value, ToolArgRider), PinConflict> {
        if self.rules.is_empty() {
            return Ok((input, ToolArgRider::default()));
        }

        let mut input_obj = match input {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let mut rider = ToolArgRider::default();

        for rule in self.selected_rules(tool_name, Flavor::Default) {
            if !input_obj.contains_key(&rule.param) {
                let value = Value::String(rule.value.clone());
                input_obj.insert(rule.param.clone(), value.clone());
                rider.defaults_applied.insert(rule.param.clone(), value);
            }
        }

        for rule in self.selected_rules(tool_name, Flavor::Pin) {
            // A pin only acts on a model-supplied value: mismatch refuses,
            // match records the enforcement. An absent param is a no-op and
            // MUST stay rider-silent — `pin:*` globs match every tool, so an
            // unconditional rider would stamp the pinned paths onto every
            // tool result in the session (observed: vibebh cockpit dispatch
            // drowning one-line results under two worktree paths per call).
            let expected = Value::String(rule.value.clone());
            if let Some(actual) = input_obj.get(&rule.param) {
                if actual != &expected {
                    return Err(PinConflict {
                        param: rule.param.clone(),
                        expected,
                        actual: actual.clone(),
                    });
                }
                rider.pin_enforced.insert(rule.param.clone(), expected);
            }
        }

        Ok((Value::Object(input_obj), rider))
    }

    pub fn validation_warnings<'a, I>(&self, schemas: I) -> Vec<String>
    where
        I: IntoIterator<Item = (&'a str, &'a Value)>,
    {
        let schemas: Vec<_> = schemas.into_iter().collect();
        let mut warnings = Vec::new();
        let mut emitted = BTreeSet::new();

        for rule in &self.rules {
            let mut matched_any_tool = false;
            for (tool_name, schema) in &schemas {
                if !rule.pattern.matches(tool_name) {
                    continue;
                }
                matched_any_tool = true;
                // Glob rules are "wherever the param exists" by design
                // (§3.1: the host writes them deliberately): a `pin:*.cwd`
                // worktree pin matching tools without a `cwd` param is the
                // expected steady state, not rot — only exact-name rules
                // warn on a missing param.
                if !rule.pattern.is_glob() && !schema_has_param(schema, &rule.param) {
                    let msg = format!(
                        "tool arg default key '{}' references unknown param '{}' on tool '{}'",
                        rule.key, rule.param, tool_name
                    );
                    if emitted.insert(msg.clone()) {
                        warnings.push(msg);
                    }
                }
            }
            if !matched_any_tool {
                let msg = format!(
                    "tool arg default key '{}' matched no loaded tool schemas",
                    rule.key
                );
                if emitted.insert(msg.clone()) {
                    warnings.push(msg);
                }
            }
        }

        warnings
    }

    fn selected_rules(&self, tool_name: &str, flavor: Flavor) -> Vec<&Rule> {
        let mut selected: BTreeMap<&str, &Rule> = BTreeMap::new();
        for exactness in [false, true] {
            for rule in self.rules.iter().filter(|rule| {
                rule.flavor == flavor
                    && rule.pattern.matches(tool_name)
                    && rule.pattern.is_glob() == exactness
            }) {
                selected.entry(&rule.param).or_insert(rule);
            }
        }
        selected.into_values().collect()
    }
}

pub fn apply_rider(result: ToolResult, rider: &ToolArgRider) -> ToolResult {
    if rider.is_empty() {
        return result;
    }
    match result {
        ToolResult::Json(Value::Object(mut obj)) => {
            if !rider.defaults_applied.is_empty() {
                obj.insert(
                    "defaults_applied".to_string(),
                    Value::Object(rider.defaults_applied.clone().into_iter().collect()),
                );
            }
            if !rider.pin_enforced.is_empty() {
                obj.insert(
                    "pin_enforced".to_string(),
                    Value::Object(rider.pin_enforced.clone().into_iter().collect()),
                );
            }
            if !rider.pin_conflict.is_empty() {
                obj.insert(
                    "pin_conflict".to_string(),
                    Value::Object(rider.pin_conflict.clone().into_iter().collect()),
                );
            }
            ToolResult::Json(Value::Object(obj))
        }
        ToolResult::Json(v) => ToolResult::Json(json!({
            "result": v,
            "tool_arg_context": rider.to_value(),
        })),
        ToolResult::Text(mut text) => {
            text.push_str(&rider.as_text_rider());
            ToolResult::Text(text)
        }
        ToolResult::Error(mut text) => {
            text.push_str(&rider.as_text_rider());
            ToolResult::Error(text)
        }
    }
}

fn parse_rule(key: String, value: String) -> Result<Rule, String> {
    let (flavor_raw, rest) = key
        .split_once(':')
        .ok_or_else(|| format!("tool arg default key '{key}' is missing '<flavor>:'"))?;
    let flavor = match flavor_raw {
        "default" => Flavor::Default,
        "pin" => Flavor::Pin,
        other => {
            return Err(format!(
                "tool arg default key '{key}' has unsupported flavor '{other}'"
            ));
        }
    };
    let Some(dot) = rest.rfind('.') else {
        return Err(format!(
            "tool arg default key '{key}' is missing '.<param>'"
        ));
    };
    let (pattern_raw, param_raw) = rest.split_at(dot);
    let param = &param_raw[1..];
    if param.is_empty() {
        return Err(format!("tool arg default key '{key}' has empty param"));
    }
    if param.contains('*') {
        return Err(format!(
            "tool arg default key '{key}' has invalid param '{param}'; params are exact"
        ));
    }
    let pattern = Pattern::parse(pattern_raw)?;
    let param = param.to_string();
    Ok(Rule {
        key,
        flavor,
        pattern,
        param,
        value,
    })
}

fn schema_has_param(schema: &Value, param: &str) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|props| props.contains_key(param))
}

fn tool_aliases(tool_name: &str) -> Vec<String> {
    let mut aliases = vec![tool_name.to_string()];
    if tool_name.contains("__") {
        aliases.push(tool_name.replace("__", "."));
    }
    if let Some(rest) = tool_name.strip_prefix("mcp__blackbox__") {
        aliases.push(format!("mcp.{rest}"));
    }
    aliases
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(&str, &str)]) -> ToolArgDefaults {
        ToolArgDefaults::parse_map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn lookup_returns_default_rule_value_for_exact_param() {
        let defaults = table(&[
            ("default:rust.moveStructFields.acknowledge_repr", "true"),
            ("default:rust.moveStructFields.other_param", "x"),
            ("pin:rust.moveStructFields.acknowledge_repr", "false"),
            ("default:rust.*.acknowledge_public_api_change", "true"),
        ]);
        assert_eq!(
            defaults.lookup("rust.moveStructFields", "acknowledge_repr"),
            Some("true"),
            "exact Default rule is a grant"
        );
        assert_eq!(
            defaults.lookup("rust.moveStructFields", "other_param"),
            Some("x")
        );
        assert_eq!(
            defaults.lookup("rust.migrateErrorType", "acknowledge_public_api_change"),
            Some("true"),
            "glob patterns match by the same alias logic as apply"
        );
        assert_eq!(
            defaults.lookup("rust.moveStructFields", "missing"),
            None,
            "no rule, no grant"
        );
        assert_eq!(
            defaults.lookup("java.moveStructFields", "acknowledge_public_api_change"),
            None,
            "glob prefix does not cross the language boundary"
        );
    }

    #[test]
    fn lookup_ignores_pin_rules() {
        // Pins are enforcement, not grants: a pinned value must never read
        // as operator authority for an RX-V1 flag.
        let defaults = table(&[("pin:rust.moveStructFields.acknowledge_repr", "true")]);
        assert_eq!(defaults.lookup("rust.moveStructFields", "acknowledge_repr"), None);
    }

    #[test]
    fn parses_flavors_globs_and_rejects_malformed_keys() {
        let defaults = table(&[
            ("default:mcp.bbox_note.session_id", "s1"),
            ("pin:*.cwd", "/tmp/wt"),
        ]);
        assert!(!defaults.is_empty());

        assert!(
            ToolArgDefaults::parse_map(BTreeMap::from([("mcp.x.y".into(), "v".into())])).is_err()
        );
        assert!(
            ToolArgDefaults::parse_map(BTreeMap::from([("maybe:mcp.x.y".into(), "v".into())]))
                .is_err()
        );
        assert!(
            ToolArgDefaults::parse_map(BTreeMap::from([("default:mcp*foo.y".into(), "v".into())]))
                .is_err()
        );
        assert!(
            ToolArgDefaults::parse_map(BTreeMap::from([("default:mcp.x.".into(), "v".into())]))
                .is_err()
        );
    }

    #[test]
    fn exact_rules_beat_globs_for_the_same_param() {
        let defaults = table(&[
            ("default:*.session_id", "glob"),
            ("default:mcp.bbox_note.session_id", "exact"),
        ]);
        let (input, rider) = defaults
            .apply("mcp__blackbox__bbox_note", json!({}))
            .unwrap();
        assert_eq!(input["session_id"], "exact");
        assert_eq!(rider.defaults_applied["session_id"], "exact");
    }

    #[test]
    fn default_fills_only_when_model_omits_param() {
        let defaults = table(&[("default:mcp.bbox_note.session_id", "host")]);
        let (input, rider) = defaults
            .apply("mcp__blackbox__bbox_note", json!({"kind": "done"}))
            .unwrap();
        assert_eq!(input["session_id"], "host");
        assert_eq!(rider.defaults_applied["session_id"], "host");

        let (input, rider) = defaults
            .apply("mcp__blackbox__bbox_note", json!({"session_id": "model"}))
            .unwrap();
        assert_eq!(input["session_id"], "model");
        assert!(rider.is_empty());
    }

    #[test]
    fn pin_conflict_errors_without_overriding() {
        let defaults = table(&[("pin:mcp.bbox_note.session_id", "host")]);
        let err = defaults
            .apply("mcp__blackbox__bbox_note", json!({"session_id": "model"}))
            .unwrap_err();
        assert_eq!(err.param, "session_id");
        assert_eq!(err.expected, "host");
        assert_eq!(err.actual, "model");
    }

    #[test]
    fn pin_rider_silent_when_param_absent() {
        // `pin:*` globs match every tool, so a pin checked against an ABSENT
        // param must be a complete no-op: no fill, no rider. Otherwise every
        // tool result in a worktree dispatch carries the pinned paths as
        // noise (regression observed live on a vibebh cockpit dispatch).
        let defaults = table(&[("pin:*.cwd", "/repo/wt"), ("pin:*.project_dir", "/repo/wt")]);

        let (out, rider) = defaults
            .apply(
                "mcp__blackbox__bbox_thread_list",
                json!({"project": "/repo/base", "status": "open"}),
            )
            .unwrap();
        assert!(
            rider.is_empty(),
            "absent pinned params must stay rider-silent"
        );
        assert_eq!(out.get("cwd"), None, "pins never fill");
        assert_eq!(out.get("project_dir"), None, "pins never fill");

        // A present, matching value IS an enforcement and is disclosed.
        let (_, rider) = defaults
            .apply(
                "mcp__blackbox__bro_exec",
                json!({"prompt": "x", "cwd": "/repo/wt"}),
            )
            .unwrap();
        assert_eq!(rider.pin_enforced.get("cwd"), Some(&json!("/repo/wt")));
        assert!(!rider.pin_enforced.contains_key("project_dir"));
    }

    #[test]
    fn riders_are_inserted_into_json_results() {
        let mut rider = ToolArgRider::default();
        rider
            .defaults_applied
            .insert("session_id".into(), json!("host"));
        rider.pin_enforced.insert("cwd".into(), json!("/repo/wt"));

        let result = apply_rider(ToolResult::Json(json!({"ok": true})), &rider);
        let ToolResult::Json(v) = result else {
            panic!("expected json result");
        };
        assert_eq!(v["defaults_applied"]["session_id"], "host");
        assert_eq!(v["pin_enforced"]["cwd"], "/repo/wt");
    }

    #[test]
    fn worktree_pin_covers_both_cwd_and_project_dir_spellings() {
        // The pin guards by the literal param key in the tool input, and the
        // table applies BEFORE the daemon's serde alias normalization
        // (dispatch tools advertise `cwd`, accept `project_dir` as a
        // deprecated alias — gap-6366c92d). The daemon therefore emits pins
        // for BOTH names; either spelling of a wrong tree must refuse.
        let defaults = table(&[("pin:*.cwd", "/repo/wt"), ("pin:*.project_dir", "/repo/wt")]);

        // New canonical name, wrong tree: refused.
        let err = defaults
            .apply(
                "mcp__blackbox__bro_exec",
                json!({"prompt": "x", "cwd": "/repo/primary"}),
            )
            .unwrap_err();
        assert_eq!(err.param, "cwd");
        assert_eq!(err.expected, "/repo/wt");
        assert_eq!(err.actual, "/repo/primary");

        // Old alias name, wrong tree: still refused.
        let err = defaults
            .apply(
                "mcp__blackbox__bro_exec",
                json!({"prompt": "x", "project_dir": "/repo/primary"}),
            )
            .unwrap_err();
        assert_eq!(err.param, "project_dir");
        assert_eq!(err.actual, "/repo/primary");

        // Correct tree passes under either spelling.
        for key in ["cwd", "project_dir"] {
            let (_, rider) = defaults
                .apply(
                    "mcp__blackbox__bro_exec",
                    json!({"prompt": "x", (key): "/repo/wt"}),
                )
                .unwrap();
            assert_eq!(rider.pin_enforced[key], "/repo/wt");
            assert!(rider.pin_conflict.is_empty());
        }
    }

    #[test]
    fn glob_rules_do_not_warn_on_tools_missing_the_param() {
        // Daemon-shaped table on a standard dispatch profile: glob pins for
        // both dispatch-cwd spellings plus an exact retrieval-read default. Tools
        // without the pinned params are the expected steady state for glob
        // rules — session-start validation must stay quiet.
        let defaults = table(&[
            ("pin:*.cwd", "/repo/wt"),
            ("pin:*.project_dir", "/repo/wt"),
            ("default:mcp.bbox_hybrid_search.project", "/repo/wt"),
        ]);
        let dispatch_schema = json!({
            "type": "object",
            "properties": {"prompt": {"type": "string"}, "cwd": {"type": "string"}}
        });
        let retrieval_schema = json!({
            "type": "object",
            "properties": {"query": {"type": "string"}, "project": {"type": "string"}}
        });
        let note_schema = json!({
            "type": "object",
            "properties": {"kind": {"type": "string"}, "session_id": {"type": "string"}}
        });
        let warnings = defaults.validation_warnings([
            ("mcp__blackbox__bro_exec", &dispatch_schema),
            ("mcp__blackbox__bbox_hybrid_search", &retrieval_schema),
            ("mcp__blackbox__bbox_note", &note_schema),
        ]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        // Exact rules still warn on a missing param (rot detection).
        let stale = table(&[("default:mcp.bbox_note.nope", "x")]);
        let warnings = stale.validation_warnings([("mcp__blackbox__bbox_note", &note_schema)]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown param 'nope'"));

        // Glob rules that match no loaded tool at all still warn.
        let dead = table(&[("pin:mcp.bbox_zzz_*.cwd", "/repo/wt")]);
        let warnings = dead.validation_warnings([("mcp__blackbox__bbox_note", &note_schema)]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("matched no loaded tool"));
    }

    #[test]
    fn validation_warns_for_unknown_tool_or_param() {
        let defaults = table(&[
            ("default:mcp.bbox_note.session_id", "host"),
            ("default:mcp.bbox_note.nope", "host"),
            ("pin:mcp.unknown.session_id", "host"),
        ]);
        let schema = json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"}
            }
        });
        let warnings =
            defaults.validation_warnings([("mcp__blackbox__bbox_note", &schema)].into_iter());
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| w.contains("unknown param 'nope'")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("matched no loaded tool"))
        );
    }
}
