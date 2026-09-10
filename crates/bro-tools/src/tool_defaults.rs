use crate::tool::ToolResult;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Default,
    Pin,
    Grant,
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
    value: Value,
}

/// Host-supplied per-(tool,param) default and pin table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolArgDefaults {
    rules: Vec<Rule>,
}

/// Structured observation of default/pin handling for a single tool call.
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
        ToolResult::Error(format!(
            "pin conflict for tool '{tool_name}' param '{}': expected {}, got {}; nothing executed",
            self.param, self.expected, self.actual,
        ))
    }
}

impl ToolArgDefaults {
    /// Compatibility input: every existing host string remains a JSON string.
    pub fn parse_map(raw: BTreeMap<String, String>) -> Result<Self, String> {
        Self::parse_values(
            raw.into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect(),
        )
    }

    pub fn parse_values(raw: BTreeMap<String, Value>) -> Result<Self, String> {
        Ok(Self {
            rules: raw
                .into_iter()
                .map(|(key, value)| parse_rule(key, value))
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// String-only compatibility lookup for ordinary wrapper arguments.
    pub fn lookup(&self, tool_name: &str, param: &str) -> Option<&str> {
        self.lookup_value(tool_name, param).and_then(Value::as_str)
    }

    pub fn lookup_value(&self, tool_name: &str, param: &str) -> Option<&Value> {
        self.selected_rules(tool_name, Flavor::Default)
            .into_iter()
            .find(|rule| rule.param == param)
            .map(|rule| &rule.value)
    }

    /// Host-only operator grant lookup. Legacy default keys are supported for
    /// declared grant parameters; pins never grant authority. Ordinary JSON
    /// strings are not coerced by this API; the binding interprets its grant.
    pub fn lookup_grant(&self, tool_name: &str, param: &str) -> Option<&Value> {
        self.selected_rules(tool_name, Flavor::Grant)
            .into_iter()
            .find(|rule| rule.param == param)
            .map(|rule| &rule.value)
            .or_else(|| self.lookup_value(tool_name, param))
    }

    pub fn apply_schema(
        &self,
        tool_name: &str,
        input: Value,
        schema: &Value,
        grants: &[&str],
    ) -> Result<(Value, ToolArgRider), ToolPolicyError> {
        let mut object = match input {
            Value::Object(object) => object,
            other => return Ok((other, ToolArgRider::default())),
        };
        for grant in grants {
            if object.contains_key(*grant) {
                return Err(ToolPolicyError::Invalid(format!(
                    "'{grant}' is host-only authority and cannot be authored in tool arguments"
                )));
            }
        }
        let mut rider = ToolArgRider::default();
        for flavor in [Flavor::Grant, Flavor::Default, Flavor::Pin] {
            for rule in self.selected_rules(tool_name, flavor) {
                if grants.contains(&rule.param.as_str()) {
                    if flavor == Flavor::Pin {
                        return Err(ToolPolicyError::Invalid(format!(
                            "{}: pins cannot grant host authority",
                            rule.key
                        )));
                    }
                    let grant = self
                        .lookup_grant(tool_name, &rule.param)
                        .expect("selected host grant");
                    if !(grant.is_boolean()
                        || grant.as_str().is_some_and(|value| {
                            value.eq_ignore_ascii_case("true")
                                || value.eq_ignore_ascii_case("false")
                        }))
                    {
                        return Err(ToolPolicyError::Invalid(format!(
                            "{}: host boolean grant expects true or false",
                            rule.key
                        )));
                    }
                    continue;
                }
                if flavor == Flavor::Grant {
                    if rule.pattern.is_glob() {
                        continue;
                    }
                    return Err(ToolPolicyError::Invalid(format!(
                        "{}: tool '{tool_name}' does not declare this host authority grant",
                        rule.key
                    )));
                }
                if !schema_has_param(schema, &rule.param) {
                    if rule.pattern.is_glob() {
                        continue;
                    }
                    return Err(ToolPolicyError::Invalid(format!(
                        "{}: unknown parameter '{}' on tool '{tool_name}'",
                        rule.key, rule.param
                    )));
                }
                validate_rule_value(schema, rule)?;
                match flavor {
                    Flavor::Default if !object.contains_key(&rule.param) => {
                        object.insert(rule.param.clone(), rule.value.clone());
                        rider
                            .defaults_applied
                            .insert(rule.param.clone(), rule.value.clone());
                    }
                    Flavor::Pin => {
                        if let Some(actual) = object.get(&rule.param) {
                            if actual != &rule.value {
                                return Err(ToolPolicyError::Pin(PinConflict {
                                    param: rule.param.clone(),
                                    expected: rule.value.clone(),
                                    actual: actual.clone(),
                                }));
                            }
                            rider
                                .pin_enforced
                                .insert(rule.param.clone(), rule.value.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok((Value::Object(object), rider))
    }

    // Existing pure selection tests use permissive property schemas. Runtime
    // admission always supplies the actual tool schema through apply_schema.
    #[cfg(test)]
    fn apply(&self, tool_name: &str, input: Value) -> Result<(Value, ToolArgRider), PinConflict> {
        let properties: Map<String, Value> = self
            .rules
            .iter()
            .map(|rule| (rule.param.clone(), json!({})))
            .collect();
        self.apply_schema(tool_name, input, &json!({"properties":properties}), &[])
            .map_err(|error| match error {
                ToolPolicyError::Pin(conflict) => conflict,
                ToolPolicyError::Invalid(message) => panic!("unexpected schema failure: {message}"),
            })
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

#[derive(Debug, Clone)]
pub enum ToolPolicyError {
    Pin(PinConflict),
    Invalid(String),
}

impl ToolPolicyError {
    pub fn observation(&self) -> Value {
        match self {
            Self::Pin(conflict) => conflict.rider().to_value(),
            Self::Invalid(message) => json!({"policy_error": message}),
        }
    }
    pub fn into_tool_result(self, tool: &str) -> ToolResult {
        match self {
            Self::Pin(conflict) => conflict.into_tool_result(tool),
            Self::Invalid(message) => ToolResult::Error(format!(
                "invalid host tool policy for '{tool}': {message}; nothing executed"
            )),
        }
    }
}

fn validate_rule_value(schema: &Value, rule: &Rule) -> Result<(), ToolPolicyError> {
    // Validate a one-property object while retaining local reference targets.
    // Other required parameters are supplied by the actual invocation, not by
    // this host rule. Real JSON typing is preserved, including null and arrays.
    let mut check =
        json!({"type":"object", "properties":schema["properties"], "required":[rule.param]});
    for key in ["$defs", "definitions"] {
        if let Some(value) = schema.get(key) {
            check[key] = value.clone();
        }
    }
    let validator = jsonschema::JSONSchema::compile(&check).map_err(|error| {
        ToolPolicyError::Invalid(format!(
            "{}: cannot validate tool schema: {error}",
            rule.key
        ))
    })?;
    let mut instance = Map::new();
    instance.insert(rule.param.clone(), rule.value.clone());
    if !validator.is_valid(&Value::Object(instance)) {
        return Err(ToolPolicyError::Invalid(format!(
            "{}: configured value does not satisfy the declared parameter schema",
            rule.key
        )));
    }
    Ok(())
}

fn parse_rule(key: String, value: Value) -> Result<Rule, String> {
    let (flavor_raw, rest) = key
        .split_once(':')
        .ok_or_else(|| format!("tool arg default key '{key}' is missing '<flavor>:'"))?;
    let flavor = match flavor_raw {
        "default" => Flavor::Default,
        "pin" => Flavor::Pin,
        "grant" => Flavor::Grant,
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
        assert_eq!(
            defaults.lookup("rust.moveStructFields", "acknowledge_repr"),
            None
        );
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
    #[test]
    fn typed_values_are_preserved_and_validated_against_real_properties() {
        let schema = json!({"type":"object", "properties":{
            "limit":{"type":"integer", "minimum":1}, "enabled":{"type":"boolean"},
            "items":{"type":"array", "items":{"type":"string"}}, "maybe":{"type":["string","null"]},
            "label":{"type":"string"}
        }});
        let defaults = ToolArgDefaults::parse_values(BTreeMap::from([
            ("default:fixture.limit".into(), json!(10)),
            ("pin:fixture.limit".into(), json!(10)),
            ("default:fixture.enabled".into(), json!(true)),
            ("default:fixture.items".into(), json!(["a", "b"])),
            ("default:fixture.maybe".into(), Value::Null),
            ("default:fixture.label".into(), json!("false")),
        ]))
        .unwrap();
        let (result, observation) = defaults
            .apply_schema("fixture", json!({}), &schema, &[])
            .unwrap();
        assert_eq!(
            result,
            json!({"limit":10,"enabled":true,"items":["a","b"],"maybe":null,"label":"false"})
        );
        assert_eq!(observation.pin_enforced["limit"], 10);
        let legacy = table(&[("default:fixture.enabled", "true")]);
        assert!(
            legacy
                .apply_schema("fixture", json!({}), &schema, &[])
                .is_err()
        );
        let bad = ToolArgDefaults::parse_values(BTreeMap::from([(
            "default:fixture.limit".into(),
            json!(0),
        )]))
        .unwrap();
        assert!(
            bad.apply_schema("fixture", json!({}), &schema, &[])
                .is_err()
        );
    }

    #[test]
    fn schema_absent_wildcards_skip_but_exact_rules_fail_closed() {
        let glob = table(&[("default:*.cwd", "/fixture"), ("pin:*.cwd", "/fixture")]);
        let (result, observation) = glob
            .apply_schema(
                "fixture",
                json!({"query":"text"}),
                &json!({"properties":{"query":{"type":"string"}}}),
                &[],
            )
            .unwrap();
        assert_eq!(result, json!({"query":"text"}));
        assert!(observation.is_empty());
        let exact = table(&[("default:fixture.cwd", "/fixture")]);
        assert!(
            exact
                .apply_schema("fixture", json!({}), &json!({"properties":{}}), &[])
                .is_err()
        );
    }

    #[test]
    fn local_schema_refs_validate_defaults_and_typed_pins() {
        let schema = json!({"$defs":{"Kind":{"type":"string","enum":["one","two"]}}, "properties":{"kind":{"$ref":"#/$defs/Kind"}}});
        for (value, allowed) in [
            (json!("one"), true),
            (json!("three"), false),
            (json!(1), false),
        ] {
            let defaults = ToolArgDefaults::parse_values(BTreeMap::from([(
                "default:fixture.kind".into(),
                value,
            )]))
            .unwrap();
            assert_eq!(
                defaults
                    .apply_schema("fixture", json!({}), &schema, &[])
                    .is_ok(),
                allowed
            );
        }
    }

    #[test]
    fn declared_grants_stay_out_of_inputs_and_observations() {
        for value in [json!(true), json!("true")] {
            let defaults = ToolArgDefaults::parse_values(BTreeMap::from([(
                "default:fixture.acknowledge".into(),
                value.clone(),
            )]))
            .unwrap();
            let schema = json!({"properties":{"file":{"type":"string"}}});
            let (result, observation) = defaults
                .apply_schema("fixture", json!({"file":"a.rs"}), &schema, &["acknowledge"])
                .unwrap();
            assert_eq!(result, json!({"file":"a.rs"}));
            assert!(observation.is_empty());
            assert_eq!(
                defaults.lookup_grant("fixture", "acknowledge"),
                Some(&value)
            );
            assert!(
                defaults
                    .apply_schema(
                        "fixture",
                        json!({"file":"a.rs", "acknowledge":true}),
                        &schema,
                        &["acknowledge"]
                    )
                    .is_err()
            );
        }
        let explicit = ToolArgDefaults::parse_values(BTreeMap::from([(
            "grant:fixture.acknowledge".into(),
            json!(true),
        )]))
        .unwrap();
        assert!(
            explicit
                .apply_schema(
                    "fixture",
                    json!({}),
                    &json!({"properties":{}}),
                    &["acknowledge"]
                )
                .is_ok()
        );
        assert!(
            explicit
                .apply_schema("fixture", json!({}), &json!({"properties":{}}), &[])
                .is_err()
        );
    }
}
