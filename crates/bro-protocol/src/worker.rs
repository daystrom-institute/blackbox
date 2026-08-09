//! Fully-resolved worker spawn spec: the daemon composes everything it
//! materializes at spawn time into one serializable value, and an executor
//! (local today, `fleetd` later) turns it into a supervised child process.
//!
//! This is the contract the fleet-supervisor extraction (slice 5 of
//! `design/daemon-runtime/locality-first-decomposition.md`) hands across the
//! daemon -> fleetd seam. Everything here is pure serde with no I/O; final
//! binary resolution and process spawning stay executor-side.
//!
//! Two correctness invariants the type encodes:
//!
//! 1. **Prompts never ride argv.** The first user turn lives in
//!    [`WorkerSpawnSpec::initial_messages`], not in [`WorkerSpawnSpec::argv`],
//!    so a prompt can never leak into `ps`/process listings (the salvage
//!    post-mortem's hard rule).
//! 2. **Provider credentials never leak through Debug.** The environment map
//!    is a [`SecretEnv`] newtype whose `Debug`/`Display` render values as
//!    `REDACTED` while serde still round-trips the real values (it must cross
//!    a wire later).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use bro_core::{Provider, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Harness environment key carrying the opaque managed-workspace capability.
///
/// The value rides [`SecretEnv`], never argv. The harness may resolve it into
/// the daemon self-MCP header, and shell grandchildren must scrub it with the
/// rest of the session credentials.
pub const WORKSPACE_BINDING_ENV: &str = "BRO_WORKSPACE_BINDING_TOKEN";

/// Harness environment key carrying the daemon endpoint that owns provisional
/// source intake for the bound workspace. This is trusted spawn configuration,
/// not model input, and shell grandchildren scrub it with the session env.
pub const KNOWLEDGE_SOURCE_URL_ENV: &str = "BRO_KNOWLEDGE_SOURCE_URL";

/// Harness environment key carrying the serialized path-free published scope
/// authorized by the daemon when it minted [`WORKSPACE_BINDING_ENV`].
pub const WORKSPACE_SCOPE_ENV: &str = "BRO_WORKSPACE_PUBLISHED_SCOPE";

/// Private daemon self-MCP header carrying [`WORKSPACE_BINDING_ENV`].
///
/// Harness configuration stores only an environment reference for this
/// header. The secret itself is resolved inside the harness process.
pub const WORKSPACE_BINDING_HEADER: &str = "x-blackbox-workspace-binding";

/// Opaque capability binding one supervised session to one managed workspace.
///
/// The wire form is a 64-character lowercase hexadecimal secret. Debug and
/// Display are always redacted because fleetd retains this value for daemon
/// re-adoption after a daemon restart.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkspaceBindingToken(String);

impl WorkspaceBindingToken {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidWorkspaceBindingToken> {
        let value = value.into();
        if value.len() != 64
            || !value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(InvalidWorkspaceBindingToken);
        }
        Ok(Self(value))
    }

    /// Expose the credential only at the authentication/injection boundary.
    /// Never log or format the returned value.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkspaceBindingToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkspaceBindingToken")
            .field(&REDACTED)
            .finish()
    }
}

impl fmt::Display for WorkspaceBindingToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for WorkspaceBindingToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidWorkspaceBindingToken;

impl fmt::Display for InvalidWorkspaceBindingToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "workspace binding token must be exactly 64 lowercase hexadecimal characters",
        )
    }
}

impl std::error::Error for InvalidWorkspaceBindingToken {}

/// An environment map whose `Debug`/`Display` redact every value, but whose
/// serde representation is the plain map (it must cross a wire and reach the
/// executor intact). Provider credentials ride here, so it must never print
/// its values into a log line or an error.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretEnv(BTreeMap<String, String>);

impl SecretEnv {
    /// Wrap an environment map.
    pub fn new(env: BTreeMap<String, String>) -> Self {
        Self(env)
    }

    /// Borrow the underlying key/value map (the accessor the executor uses to
    /// apply the environment to a child process).
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// Consume the newtype and return the underlying map.
    pub fn into_map(self) -> BTreeMap<String, String> {
        self.0
    }

    /// Iterate the key/value pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    /// Insert one daemon-resolved secret before the spec crosses to its
    /// executor. The map remains uniformly redacted under Debug/Display.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }

    /// Number of variables.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map is empty (used as a serde skip predicate).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<BTreeMap<String, String>> for SecretEnv {
    fn from(env: BTreeMap<String, String>) -> Self {
        Self(env)
    }
}

/// The literal string every redacted value renders as.
pub const REDACTED: &str = "REDACTED";

impl fmt::Debug for SecretEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Keys stay visible (they are not secret and aid debugging); values
        // are uniformly redacted.
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, REDACTED)))
            .finish()
    }
}

impl fmt::Display for SecretEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        for (i, key) in self.0.keys().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{key}={REDACTED}")?;
        }
        write!(f, "}}")
    }
}

/// A fully-resolved specification for spawning one harness worker process.
///
/// The daemon composes this once; the executor consumes it verbatim. No field
/// is re-derived executor-side except the final `bin_override` -> absolute path
/// resolution, which depends on the executor's own login-shell `PATH`.
///
/// `Eq` is intentionally not derived: [`WorkerSpawnSpec::initial_messages`]
/// holds `serde_json::Value`, which is only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerSpawnSpec {
    /// Daemon task id (roster/store key).
    pub task_id: String,
    /// Provider session id to record. Harness-backed fresh dispatch pre-mints
    /// this before spawn; a placeholder such as `pending` is possible on legacy
    /// paths until the first stream event.
    pub session_id: String,
    /// Reuse-safe identity of the concrete workspace this child owns. Cwd is
    /// still the executor path; this id is the portable session/artifact key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Provider lane this worker runs.
    pub provider: Provider,
    /// Binary name/path from `BRO_HARNESS_BIN` or provider config. `None` lets
    /// the executor fall back to the provider default. Final login-shell
    /// resolution to an absolute path stays executor-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_override: Option<String>,
    /// Post-`prepare` argv, prompt-free. The first user turn lives in
    /// [`Self::initial_messages`], never here.
    pub argv: Vec<String>,
    /// Working directory for the child. `None` inherits the executor's cwd
    /// (dispatch composition always pins one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to apply to the child, values redacted from Debug/Display.
    /// Applied *after* [`Self::env_unset`] removal, so these win.
    #[serde(default, skip_serializing_if = "SecretEnv::is_empty")]
    pub env: SecretEnv,
    /// Keys the executor must remove from its inherited environment *before*
    /// applying [`Self::env`]. This carries the daemon's service-env scrub list
    /// (the vars a worker must never inherit) so the executor no longer relies
    /// on the daemon's own process environment being pre-scrubbed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_unset: Vec<String>,
    /// The first user turn(s), delivered over the child's stdin control lane as
    /// NDJSON. NEVER placed in argv (see the module invariants).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_messages: Vec<Value>,
    /// `BRO_HOME` for the child: the single pinned derivation the executor sets
    /// as the child's `BRO_HOME`, and the same value the daemon derives
    /// [`Self::event_log_path`] from.
    pub bro_home: PathBuf,
    /// The child's durable event-log JSONL path, pinned daemon-side so both
    /// sides of the historical dual derivation (daemon transcript location and
    /// child-side `BRO_HOME` resolution) flow from this spec.
    pub event_log_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_spec() -> WorkerSpawnSpec {
        let mut env = BTreeMap::new();
        env.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-super-secret".to_string(),
        );
        env.insert("BRO_HARNESS_PROVIDER".to_string(), "glm".to_string());
        WorkerSpawnSpec {
            task_id: "task-abc".to_string(),
            session_id: "sess-xyz".to_string(),
            workspace_id: Some(WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap()),
            provider: Provider::Glm,
            bin_override: Some("bro-harness".to_string()),
            argv: vec![
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--daemon-worker".to_string(),
            ],
            cwd: Some("/repo/x".to_string()),
            env: SecretEnv::new(env),
            env_unset: vec!["BLACKBOX_MCP_URL".to_string(), "BRO_HOME".to_string()],
            initial_messages: vec![json!({"type": "user", "message": {"role": "user"}})],
            bro_home: PathBuf::from("/state/bro"),
            event_log_path: PathBuf::from("/state/bro/harness-sessions/sess-xyz.events.jsonl"),
        }
    }

    #[test]
    fn spec_serde_round_trips() {
        let spec = sample_spec();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: WorkerSpawnSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    #[test]
    fn legacy_spec_without_workspace_id_decodes_unbound() {
        let mut value = serde_json::to_value(sample_spec()).unwrap();
        value.as_object_mut().unwrap().remove("workspace_id");
        let decoded: WorkerSpawnSpec = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.workspace_id, None);
    }

    #[test]
    fn secret_env_serializes_plain_values() {
        let spec = sample_spec();
        let value = serde_json::to_value(&spec).expect("serialize");
        // The wire form must carry the real secret so the executor can apply it.
        assert_eq!(
            value["env"]["ANTHROPIC_API_KEY"],
            json!("sk-super-secret"),
            "SecretEnv must serialize the real value"
        );
    }

    #[test]
    fn secret_env_debug_redacts_values_keeps_keys() {
        let spec = sample_spec();
        let debug = format!("{spec:?}");
        assert!(
            debug.contains("ANTHROPIC_API_KEY"),
            "keys stay visible: {debug}"
        );
        assert!(debug.contains("REDACTED"), "values are redacted: {debug}");
        assert!(
            !debug.contains("sk-super-secret"),
            "secret value must never appear in Debug: {debug}"
        );
    }

    #[test]
    fn workspace_binding_token_round_trips_and_redacts() {
        let token = WorkspaceBindingToken::parse("b".repeat(64)).unwrap();
        let encoded = serde_json::to_string(&token).unwrap();
        assert_eq!(encoded, format!("\"{}\"", "b".repeat(64)));
        assert_eq!(
            serde_json::from_str::<WorkspaceBindingToken>(&encoded).unwrap(),
            token
        );
        let debug = format!("{token:?}");
        assert!(debug.contains(REDACTED));
        assert!(!debug.contains(&"b".repeat(64)));
    }

    #[test]
    fn workspace_binding_token_decode_rejects_noncanonical_secrets() {
        for value in ["b".repeat(63), "B".repeat(64), "g".repeat(64)] {
            assert!(WorkspaceBindingToken::parse(value.clone()).is_err());
            assert!(serde_json::from_value::<WorkspaceBindingToken>(Value::String(value)).is_err());
        }
    }

    #[test]
    fn secret_env_display_redacts_values() {
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), "hunter2".to_string());
        let secret = SecretEnv::new(env);
        let shown = format!("{secret}");
        assert!(shown.contains("TOKEN=REDACTED"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }
}
