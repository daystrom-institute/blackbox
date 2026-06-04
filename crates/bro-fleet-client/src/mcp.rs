//! Client-local MCP server config — the shape `fleet.json`'s `mcpServers` map
//! round-trips.
//!
//! Since 1b made the daemon own MCP injection, the fleet client only needs this
//! to PARSE and PRESERVE `fleet.json` (so the config panel's save doesn't drop a
//! user's `mcpServers`); it never resolves secrets or forwards these to a
//! provider. Header/env *values* are kept as opaque JSON so any form — a plain
//! string or a `{"$secret": "ENV"}` reference — round-trips byte-for-byte. This
//! is deliberately NOT a wire contract (it doesn't belong in `bro-protocol`);
//! it is a client-local view type (harness-daemon-boundary.md §7).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Transport-discriminated MCP server config, mirroring the daemon's Http/Sse/
/// Stdio shape but with opaque-JSON secret values (the client never resolves
/// them).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, Value>,
    },
}
