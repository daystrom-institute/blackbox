use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStatus {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerResult {
    pub status: RunnerStatus,
    pub data: serde_json::Value,
    pub errors: Vec<String>,
    pub wall_time_ms: u64,
}

impl RunnerResult {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            status: RunnerStatus::Succeeded,
            data,
            errors: Vec::new(),
            wall_time_ms: 0,
        }
    }

    pub fn fail(errors: Vec<String>) -> Self {
        Self {
            status: RunnerStatus::Failed,
            data: serde_json::Value::Null,
            errors,
            wall_time_ms: 0,
        }
    }

    #[allow(dead_code)]
    pub fn with_wall_time(mut self, ms: u64) -> Self {
        self.wall_time_ms = ms;
        self
    }
}

pub trait AtomRunner: Send + Sync {
    fn run(&self, args: &serde_json::Value) -> RunnerResult;
}

pub struct RunnerRegistry {
    deterministic: HashMap<String, Box<dyn AtomRunner>>,
    adapters: HashMap<String, Box<dyn AtomRunner>>,
}

impl fmt::Debug for RunnerRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerRegistry")
            .field(
                "deterministic_runners",
                &self.deterministic.keys().collect::<Vec<_>>(),
            )
            .field("adapter_runners", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl RunnerRegistry {
    pub fn new() -> Self {
        Self {
            deterministic: HashMap::new(),
            adapters: HashMap::new(),
        }
    }

    pub fn register_deterministic(&mut self, name: impl Into<String>, runner: Box<dyn AtomRunner>) {
        self.deterministic.insert(name.into(), runner);
    }

    pub fn register_adapter(&mut self, name: impl Into<String>, runner: Box<dyn AtomRunner>) {
        self.adapters.insert(name.into(), runner);
    }

    pub fn has_deterministic(&self, name: &str) -> bool {
        self.deterministic.contains_key(name)
    }

    pub fn has_adapter(&self, name: &str) -> bool {
        self.adapters.contains_key(name)
    }

    #[allow(dead_code)]
    pub fn deterministic_runner_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.deterministic.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    #[allow(dead_code)]
    pub fn adapter_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.adapters.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn execute_deterministic(&self, runner: &str, args: &serde_json::Value) -> RunnerResult {
        let Some(r) = self.deterministic.get(runner) else {
            return RunnerResult::fail(vec![format!(
                "error.bad_input(code=unknown_runner): no deterministic runner registered as '{runner}'"
            )]);
        };
        let start = Instant::now();
        let mut result = r.run(args);
        result.wall_time_ms = start.elapsed().as_millis() as u64;
        result
    }

    pub fn execute_adapter(&self, adapter_name: &str, args: &serde_json::Value) -> RunnerResult {
        let Some(r) = self.adapters.get(adapter_name) else {
            return RunnerResult::fail(vec![format!(
                "error.bad_input(code=unknown_adapter): no adapter runner registered as '{adapter_name}'"
            )]);
        };
        let start = Instant::now();
        let mut result = r.run(args);
        result.wall_time_ms = start.elapsed().as_millis() as u64;
        result
    }
}

impl Default for RunnerRegistry {
    fn default() -> Self {
        default_registry()
    }
}

struct EchoRunner;

impl AtomRunner for EchoRunner {
    fn run(&self, args: &serde_json::Value) -> RunnerResult {
        RunnerResult::ok(serde_json::json!({
            "echo": args.clone(),
        }))
    }
}

struct NoopRunner;

impl AtomRunner for NoopRunner {
    fn run(&self, _args: &serde_json::Value) -> RunnerResult {
        RunnerResult::ok(serde_json::json!({
            "noop": true,
        }))
    }
}

struct ValidateSchemaRunner;

impl AtomRunner for ValidateSchemaRunner {
    fn run(&self, args: &serde_json::Value) -> RunnerResult {
        if args.is_null() {
            return RunnerResult::ok(serde_json::json!({
                "valid": true,
                "input": serde_json::Value::Null,
            }));
        }
        let is_object = args.is_object();
        RunnerResult::ok(serde_json::json!({
            "valid": is_object,
            "type": if is_object { "object" } else if args.is_array() { "array" } else if args.is_string() { "string" } else { "other" },
            "input": args.clone(),
        }))
    }
}

struct BadgeyAdapterRunner;

impl AtomRunner for BadgeyAdapterRunner {
    fn run(&self, args: &serde_json::Value) -> RunnerResult {
        RunnerResult::ok(serde_json::json!({
            "adapter": "badgey",
            "accepted": true,
            "input": args.clone(),
        }))
    }
}

// Registered so the atom artifact validates against the schema; the actual
// execution is intercepted by system_events_runtime::executors before this runner runs.
struct ForgejoEnsureUserRunner;

impl AtomRunner for ForgejoEnsureUserRunner {
    fn run(&self, _args: &serde_json::Value) -> RunnerResult {
        RunnerResult::fail(vec![
            "forgejo-ensure-user must be invoked via the system_events reaction path, not directly"
                .into(),
        ])
    }
}

pub fn default_registry() -> RunnerRegistry {
    let mut reg = RunnerRegistry::new();
    reg.register_deterministic("echo", Box::new(EchoRunner));
    reg.register_deterministic("noop", Box::new(NoopRunner));
    reg.register_deterministic("validate-schema", Box::new(ValidateSchemaRunner));
    reg.register_deterministic("refactor-plan-validate", Box::new(ValidateSchemaRunner));
    reg.register_deterministic("forgejo-ensure-user", Box::new(ForgejoEnsureUserRunner));
    reg.register_adapter("badgey", Box::new(BadgeyAdapterRunner));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_runner_returns_args_as_echo() {
        let reg = default_registry();
        let result = reg.execute_deterministic("echo", &serde_json::json!({"key": "value"}));
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert!(result.errors.is_empty());
        assert_eq!(result.data["echo"]["key"], "value");
    }

    #[test]
    fn echo_runner_with_null_args() {
        let reg = default_registry();
        let result = reg.execute_deterministic("echo", &serde_json::Value::Null);
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert!(result.data["echo"].is_null());
    }

    #[test]
    fn echo_runner_with_nested_json() {
        let reg = default_registry();
        let input = serde_json::json!({
            "nested": {"deep": [1, 2, 3]},
            "flag": true,
        });
        let result = reg.execute_deterministic("echo", &input);
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(
            result.data["echo"]["nested"]["deep"],
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn noop_runner_returns_success() {
        let reg = default_registry();
        let result = reg.execute_deterministic("noop", &serde_json::json!({"ignored": true}));
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(result.data["noop"], true);
    }

    #[test]
    fn noop_runner_ignores_null_args() {
        let reg = default_registry();
        let result = reg.execute_deterministic("noop", &serde_json::Value::Null);
        assert_eq!(result.status, RunnerStatus::Succeeded);
    }

    #[test]
    fn validate_schema_with_object() {
        let reg = default_registry();
        let result = reg.execute_deterministic("validate-schema", &serde_json::json!({"a": 1}));
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(result.data["valid"], true);
        assert_eq!(result.data["type"], "object");
    }

    #[test]
    fn validate_schema_with_non_object() {
        let reg = default_registry();
        let result = reg.execute_deterministic("validate-schema", &serde_json::json!("hello"));
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(result.data["valid"], false);
        assert_eq!(result.data["type"], "string");
    }

    #[test]
    fn validate_schema_with_null() {
        let reg = default_registry();
        let result = reg.execute_deterministic("validate-schema", &serde_json::Value::Null);
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(result.data["valid"], true);
    }

    #[test]
    fn unknown_deterministic_runner_fails() {
        let reg = default_registry();
        let result = reg.execute_deterministic("nonexistent", &serde_json::Value::Null);
        assert_eq!(result.status, RunnerStatus::Failed);
        assert!(result.errors[0].contains("unknown_runner"));
        assert!(result.errors[0].contains("nonexistent"));
    }

    #[test]
    fn unknown_adapter_fails() {
        let reg = default_registry();
        let result = reg.execute_adapter("nonexistent", &serde_json::Value::Null);
        assert_eq!(result.status, RunnerStatus::Failed);
        assert!(result.errors[0].contains("unknown_adapter"));
    }

    #[test]
    fn wall_time_is_recorded() {
        let reg = default_registry();
        let result = reg.execute_deterministic("echo", &serde_json::Value::Null);
        // Wall time should be a very small number (sub-millisecond for echo),
        // but it must be present (>= 0).
        assert!(result.wall_time_ms < 1000);
    }

    #[test]
    fn runner_result_ok_convenience() {
        let r = RunnerResult::ok(serde_json::json!({"a": 1}));
        assert_eq!(r.status, RunnerStatus::Succeeded);
        assert!(r.errors.is_empty());
        assert_eq!(r.data["a"], 1);
    }

    #[test]
    fn runner_result_fail_convenience() {
        let r = RunnerResult::fail(vec!["err1".into(), "err2".into()]);
        assert_eq!(r.status, RunnerStatus::Failed);
        assert_eq!(r.errors.len(), 2);
        assert!(r.data.is_null());
    }

    #[test]
    fn runner_result_with_wall_time() {
        let r = RunnerResult::ok(serde_json::Value::Null).with_wall_time(42);
        assert_eq!(r.wall_time_ms, 42);
    }

    #[test]
    fn has_deterministic_and_adapter_checks() {
        let reg = default_registry();
        assert!(reg.has_deterministic("echo"));
        assert!(reg.has_deterministic("noop"));
        assert!(reg.has_deterministic("validate-schema"));
        assert!(!reg.has_deterministic("nonexistent"));
        assert!(reg.has_adapter("badgey"));
        assert!(!reg.has_adapter("echo"));
    }

    #[test]
    fn runner_names_listing() {
        let reg = default_registry();
        let det = reg.deterministic_runner_names();
        assert!(det.contains(&"echo"));
        assert!(det.contains(&"noop"));
        assert!(det.contains(&"validate-schema"));
        let adapters = reg.adapter_names();
        assert!(adapters.contains(&"badgey"));
    }

    #[test]
    fn custom_runner_registration() {
        struct DoubleRunner;
        impl AtomRunner for DoubleRunner {
            fn run(&self, args: &serde_json::Value) -> RunnerResult {
                let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                RunnerResult::ok(serde_json::json!({"result": n * 2}))
            }
        }

        let mut reg = RunnerRegistry::new();
        reg.register_deterministic("double", Box::new(DoubleRunner));
        let result = reg.execute_deterministic("double", &serde_json::json!({"n": 21}));
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(result.data["result"], 42);
    }

    #[test]
    fn custom_adapter_registration() {
        struct PassthroughAdapter;
        impl AtomRunner for PassthroughAdapter {
            fn run(&self, args: &serde_json::Value) -> RunnerResult {
                RunnerResult::ok(args.clone())
            }
        }

        let mut reg = RunnerRegistry::new();
        reg.register_adapter("passthrough", Box::new(PassthroughAdapter));
        let result = reg.execute_adapter("passthrough", &serde_json::json!({"hello": "world"}));
        assert_eq!(result.status, RunnerStatus::Succeeded);
        assert_eq!(result.data["hello"], "world");
    }

    #[test]
    fn registry_debug_format() {
        let reg = default_registry();
        let debug_str = format!("{reg:?}");
        assert!(debug_str.contains("RunnerRegistry"));
        assert!(debug_str.contains("deterministic_runners"));
        assert!(debug_str.contains("adapter_runners"));
    }

    #[test]
    fn default_trait_gives_default_registry() {
        let reg = RunnerRegistry::default();
        assert!(reg.has_deterministic("echo"));
        assert!(reg.has_deterministic("noop"));
        assert!(reg.has_adapter("badgey"));
    }

    #[test]
    fn runner_status_serde_roundtrip() {
        let ok = RunnerStatus::Succeeded;
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("succeeded"));
        let parsed: RunnerStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ok);

        let fail = RunnerStatus::Failed;
        let json = serde_json::to_string(&fail).unwrap();
        assert!(json.contains("failed"));
        let parsed: RunnerStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, fail);
    }

    #[test]
    fn runner_result_serialization_shape() {
        let result = RunnerResult::ok(serde_json::json!({"key": "val"})).with_wall_time(5);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], "succeeded");
        assert_eq!(json["data"]["key"], "val");
        assert!(json["errors"].as_array().unwrap().is_empty());
        assert_eq!(json["wall_time_ms"], 5);
    }
}
