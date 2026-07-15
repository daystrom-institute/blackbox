//! Projected daemon-tool capability.
//!
//! The daemon exposes a curated set of its `bbox_*` MCP tools to a dispatched
//! harness session through one typed capability family rather than an MCP
//! loopback (harness-daemon-boundary.md §6 and
//! design/bro-harness/bbox-tool-projection.md). The tool name travels as the
//! capability operation and the tool arguments travel as a JSON payload; the
//! daemon staples the authenticated session's ambient scope
//! (task / session / project / provider / bro) into the call server-side, so a
//! dispatched agent cannot forget or forge its own identity.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CapabilityResult;

/// One projected daemon-tool invocation. `tool` is the `bbox_*` tool name;
/// `arguments` is the tool's argument object (schema-validated daemon-side by
/// deserializing into the tool's real parameter type).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedToolCall {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

/// A single server-authoritative field the daemon stapled over a
/// caller-supplied value. Surfaced as an audit annotation, not an error: the
/// ambient value wins and the conflict is reported rather than rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StapleOverride {
    pub field: String,
    pub authoritative: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supplied: Option<Value>,
}

/// Result of a projected daemon-tool call. `content` is the flattened text of
/// the tool result; `structured_content` carries the tool's structured JSON
/// when present; `staple_overrides` audits any ambient fields the daemon
/// overrode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedToolOutcome {
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staple_overrides: Vec<StapleOverride>,
}

/// Session-scoped access to the daemon's projected `bbox_*` tool catalog.
///
/// The daemon implements this over its live tool layer; the harness consumes it
/// through the worker capability RPC. Absence fails closed: a session with no
/// projected grant registers no projected tools.
#[async_trait]
pub trait BboxToolCapability: Send + Sync {
    /// Invoke a projected daemon tool under the stable identity of the
    /// originating provider tool call. The daemon staples ambient scope and
    /// schema-validates the arguments before dispatch.
    async fn call_bbox_tool(
        &self,
        invocation_id: &str,
        call: ProjectedToolCall,
    ) -> CapabilityResult<ProjectedToolOutcome>;
}
