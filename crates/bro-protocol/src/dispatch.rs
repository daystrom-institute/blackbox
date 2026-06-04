//! Dispatch command-plane DTOs.
//!
//! `DispatchSpec`/`ResumeSpec` are the shape of a "start / continue an entrypoint
//! agent" request the fleet cockpit builds and the daemon services over
//! `/control/{exec,resume}`. They live at the contract bottom (§2 control plane)
//! so a thin client can name them without linking the daemon; the byte transport
//! (today a hand-built JSON body) is a thin layer above this schema.

use std::collections::HashMap;

use bro_core::Provider;

/// What to dispatch as a new top-level entrypoint agent. The cockpit's
/// composer fills this in; cwd/model are optional and resolved per dispatch
/// (no stickiness on the agent itself — provider is fixed at spawn, §4).
#[derive(Debug, Clone)]
pub struct DispatchSpec {
    pub provider: Provider,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// Effort/thinking level passed to the provider CLI's `--effort`.
    pub effort: Option<String>,
    /// Extra env overrides for the child (e.g. MCP injection wiring). The
    /// cockpit's TUI-local config (§5.2) feeds this; `None` for a bare launch.
    pub env_overrides: Option<HashMap<String, String>>,
    /// Display name persisted with the task (stored in `bro_label`) so it
    /// survives a cockpit reload. Defaults to the initial prompt's head.
    pub name: Option<String>,
}

impl DispatchSpec {
    pub fn new(provider: Provider, prompt: impl Into<String>) -> Self {
        Self {
            provider,
            prompt: prompt.into(),
            cwd: None,
            model: None,
            effort: None,
            env_overrides: None,
            name: None,
        }
    }
}

/// Resume a prior (Interrupted / reloaded) session and continue it with a new
/// turn — `--resume <session_id> -p <prompt>` (§5: steering an Interrupted
/// session auto-resumes). The harness/Claude CLI reloads the on-disk transcript.
#[derive(Debug, Clone)]
pub struct ResumeSpec {
    pub provider: Provider,
    pub session_id: String,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub name: Option<String>,
    pub env_overrides: Option<HashMap<String, String>>,
}

impl ResumeSpec {
    pub fn new(
        provider: Provider,
        session_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
            prompt: prompt.into(),
            cwd: None,
            model: None,
            effort: None,
            name: None,
            env_overrides: None,
        }
    }
}
