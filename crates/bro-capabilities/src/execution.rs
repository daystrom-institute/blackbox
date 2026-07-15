//! Durable operational-intent to live-attempt capability values.

use std::collections::BTreeMap;

use bro_core::{AtomRef, AttemptId, OperationId, Provider, SessionId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ExecutionKind {
    Fresh {
        prompt: String,
    },
    Resume {
        session_id: SessionId,
        prompt: String,
    },
    /// Resume a durable harness session whose first input is supplied by the
    /// typed agent mailbox worker command. This variant deliberately carries
    /// no prompt, so a followup body has exactly one model-visible owner.
    MailboxResume {
        session_id: SessionId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionServiceTier {
    #[default]
    Default,
    Priority,
    Flex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCodeMode {
    Off,
    Optional,
    Only,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionDirectiveCadence {
    OncePerSession,
    OncePerScope,
    EveryTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionDirective {
    pub text: String,
    pub cadence: ExecutionDirectiveCadence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionScope {
    pub project: Option<String>,
    pub bro: Option<String>,
    pub thread: Option<String>,
    pub work_item: Option<String>,
    pub root_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionDispatchContext {
    pub persona: Option<String>,
    #[serde(default)]
    pub directives: Vec<ExecutionDirective>,
    pub scope: ExecutionScope,
    pub pins: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum WorkingSetIntent {
    Scratch,
    Existing {
        cwd: String,
        #[serde(default)]
        managed_worktree: bool,
    },
    CreateManagedWorktree {
        base_repo: String,
        requested_path: Option<String>,
        #[serde(default)]
        seed_dirs: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionToolPolicy {
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub deny_tools: Vec<String>,
    #[serde(default)]
    pub tool_placement: BTreeMap<String, String>,
    #[serde(default)]
    pub tool_defaults: BTreeMap<String, String>,
    /// Exact remote operations requested for this session, keyed by the
    /// capability name carried on worker RPC. An empty map grants no remote
    /// authority, independently of which tool definitions are visible.
    #[serde(default)]
    pub allowed_remote_operations: BTreeMap<String, Vec<String>>,
    /// Exact versioned atom references this session may invoke. Granting the
    /// `atom/invoke_atom` operation without naming a ref still grants no atom.
    #[serde(default)]
    pub allowed_atom_refs: Vec<AtomRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub operation_id: OperationId,
    pub idempotency_key: String,
    pub kind: ExecutionKind,
    pub provider: Provider,
    pub model: String,
    pub effort: Option<String>,
    #[serde(default)]
    pub service_tier: ExecutionServiceTier,
    pub code_mode: Option<ExecutionCodeMode>,
    pub dispatch_context: Option<ExecutionDispatchContext>,
    pub working_set: WorkingSetIntent,
    #[serde(default)]
    pub shell_env: BTreeMap<String, String>,
    #[serde(default)]
    pub tool_policy: ExecutionToolPolicy,
    pub system_prompt: Option<String>,
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAccepted {
    pub operation_id: OperationId,
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    /// True when this response was reconstructed from a prior accepted request
    /// with the same idempotency key.
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Accepted,
    Running,
    Completed,
    Failed,
    Interrupted,
    Lost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptOutcome {
    pub attempt_id: AttemptId,
    pub state: AttemptState,
    #[serde(default)]
    pub result: Value,
}
