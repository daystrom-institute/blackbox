//! Shared kernel types for bro-facing crates.
//!
//! This crate is deliberately small: ids, refs, and lightweight error shapes
//! that both protocol DTOs and capability traits can name without depending on
//! either implementation crate.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

mod provider;
pub use provider::{Capability, EffortInfo, ModelInfo, Provider};

// ---------------------------------------------------------------------------
// Task origin
// ---------------------------------------------------------------------------
//
// Where a daemon task was spawned FROM. Read at creation time and persisted
// alongside the task so the fleet roster (design/fleet-tui/daemon-roster-and-
// tail-unification.md §4.1) can group/tab tasks by their source. The cockpit
// HUD and the fleet TUI both need this distinction:
//   - `AgentDispatch` covers the bro_* MCP tools invoked by another bro / by
//     the operator from the cockpit chat. The bulk of fleet traffic.
//   - `Cockpit` covers cockpit dispatches that bypass the bro_exec MCP
//     tool but still go through the same spawn funnel. Same intent as
//     AgentDispatch but with a separate UI tab so cockpit-launched tasks
//     are visually distinguishable from peer-bros-launched ones.
//   - `Workflow` covers team / workflow / advisor runtime
//     dispatches (orchestrate.rs, workflow_runtime.rs, roster.rs team advisor).
//   - `Atom` covers catalog atom invocations and resumes.
//   - `Cron` / `Webhook` cover the scheduled / HTTP-triggered ingress paths
//     (added in their respective slices; V1 records them as the canonical
//     labels so the roster can light up dedicated tabs as soon as the
//     ingress paths actually use them).
//   - `Unknown` is the conservative default for any creation site not
//     yet classified. The design spec (Slice 1b) audits every spawn site;
//     unclassified sites are gaps and should be re-graded.
//
// New variants: ADD, do not reorder. The serde representation is a string
// (lowercase variant name); old persisted records missing the field
// decode to `Unknown` (see TaskStore::load back-compat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    #[default]
    Unknown,
    AgentDispatch,
    Cockpit,
    Workflow,
    Atom,
    Cron,
    Webhook,
}

impl Origin {
    /// Returns the lowercase serde name (the wire form). Kept as an explicit
    /// method rather than a derived `as_str` so callers don't accidentally
    /// rely on the variant identifier spelling.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Origin::Unknown => "unknown",
            Origin::AgentDispatch => "agentdispatch",
            Origin::Cockpit => "cockpit",
            Origin::Workflow => "workflow",
            Origin::Atom => "atom",
            Origin::Cron => "cron",
            Origin::Webhook => "webhook",
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(BroId);
id_type!(SessionId);
id_type!(TaskId);
id_type!(AtomRef);

/// Stable identity of one concrete checkout/workspace.
///
/// The value is the existing `.bbox/local/checkout-id` marker: 128 bits of
/// lowercase hexadecimal randomness, minted once per concrete checkout. It is
/// deliberately not derived from a path, task, or session, so moving a
/// workspace preserves identity while replacing one at the same path does not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub const ENCODED_LEN: usize = 32;

    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidWorkspaceId> {
        let value = value.into();
        if value.len() != Self::ENCODED_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(InvalidWorkspaceId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for WorkspaceId {
    type Error = InvalidWorkspaceId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidWorkspaceId;

impl fmt::Display for InvalidWorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("workspace id must be 32 lowercase hexadecimal characters")
    }
}

impl std::error::Error for InvalidWorkspaceId {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroError {
    pub code: String,
    pub message: String,
}

impl BroError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_round_trips() {
        let id = WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(encoded, "\"0123456789abcdef0123456789abcdef\"");
        assert_eq!(serde_json::from_str::<WorkspaceId>(&encoded).unwrap(), id);
    }

    #[test]
    fn workspace_id_rejects_noncanonical_values() {
        for value in [
            "",
            "0123456789abcdef",
            "0123456789abcdef0123456789abcdeF",
            "g123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef00",
        ] {
            assert!(WorkspaceId::parse(value).is_err(), "accepted {value:?}");
            let encoded = serde_json::to_string(value).unwrap();
            assert!(serde_json::from_str::<WorkspaceId>(&encoded).is_err());
        }
    }
}
