use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Bro roster endpoint — resolves selectors to concrete per-bro lane info
// (provider, session_id, transcript file path). Consumed by `bro tail`
// to know WHICH JSONL files to open and follow.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct RosterQuery {
    /// Comma-separated bro names (union of matches across all teams)
    #[serde(default)]
    pub(crate) bros: Option<String>,
    /// Comma-separated team names (each contributes all members). Accepts
    /// legacy `team=` singular form as an alias.
    #[serde(default, alias = "team")]
    pub(crate) teams: Option<String>,
    /// Comma-separated session IDs — synthetic adhoc lanes bypassing team membership.
    #[serde(default, alias = "session")]
    pub(crate) sessions: Option<String>,
    /// Comma-separated provider names (claude/codex/gemini/copilot/vibe) — final filter.
    #[serde(default, alias = "provider")]
    pub(crate) providers: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BroRosterEntry {
    pub(crate) bro: String,
    pub(crate) bro_selector: String,
    pub(crate) team: String,
    pub(crate) provider: String,
    pub(crate) account: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) jsonl_path: Option<String>,
    pub(crate) brofile: String,
    pub(crate) model: Option<String>,
}


