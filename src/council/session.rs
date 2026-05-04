//! `CouncilSession` — top-level metadata for a single deliberation.
//! Persisted at `<store>/councils/<id>/session.json` via tmp+rename.
//!
//! Bound to a team for roster (members, brofile resolution, project_dir).
//! `member_sessions` is council-scoped: each bro gets a fresh provider
//! session on first turn (lazy via the drain worker), distinct from any
//! sessions the same brofile may hold via team broadcasts. Prevents
//! cross-feature session poisoning.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilSession {
    pub id: String,
    pub team_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub topic: String,
    pub charter: String,
    /// Council-scoped provider sessions, keyed by bro name. Populated
    /// lazily on the bro's first drain (`bro_exec`); subsequent turns
    /// resume against the recorded session id.
    #[serde(default)]
    pub member_sessions: HashMap<String, String>,
    pub config: CouncilConfig,
    pub status: CouncilStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilConfig {
    pub max_inbox_depth: usize,
    pub relay_depth_max: u32,
    pub mention_dedupe: bool,
    pub fanout_per_cascade: usize,
    pub low_signal_patterns: Vec<String>,
    pub lease_ttl_secs: u64,
    pub max_attempts: u32,
}

impl Default for CouncilConfig {
    fn default() -> Self {
        Self {
            max_inbox_depth: 3,
            relay_depth_max: 3,
            mention_dedupe: true,
            fanout_per_cascade: 5,
            low_signal_patterns: vec![
                "^pass$".to_string(),
                "^no comment$".to_string(),
                "^agreed\\.?$".to_string(),
                "^sounds good\\.?$".to_string(),
            ],
            lease_ttl_secs: 300,
            max_attempts: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CouncilStatus {
    Open,
    Closed,
}

impl CouncilSession {
    pub fn new(
        id: String,
        team_id: String,
        topic: String,
        charter: String,
        project: Option<String>,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id,
            team_id,
            project,
            topic,
            charter,
            member_sessions: HashMap::new(),
            config: CouncilConfig::default(),
            status: CouncilStatus::Open,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_safe_caps() {
        let c = CouncilConfig::default();
        assert!(c.max_inbox_depth >= 1);
        assert!(c.relay_depth_max >= 1 && c.relay_depth_max <= 10);
        assert!(c.fanout_per_cascade >= 1);
        assert!(!c.low_signal_patterns.is_empty());
    }

    #[test]
    fn session_roundtrip_json() {
        let s = CouncilSession::new(
            "council-abcdef12".into(),
            "team-x".into(),
            "should we ship?".into(),
            "default charter".into(),
            Some("/repo".into()),
        );
        let j = serde_json::to_string(&s).unwrap();
        let r: CouncilSession = serde_json::from_str(&j).unwrap();
        assert_eq!(r.id, s.id);
        assert_eq!(r.status, CouncilStatus::Open);
    }
}
