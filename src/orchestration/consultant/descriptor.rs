use std::str::FromStr;

use super::types::ConsultantId;

/// The configuration boundary between the generic consultant runtime and one
/// configured consumer (Badgey is the first; see
/// `orchestration::badgey::vocabulary`).
///
/// Descriptors are code-owned constants, never loaded from data: the intent
/// post-processor is the recursion-guard security boundary, so a consumer may
/// only *select* code-owned vocabulary and handlers, not define new ones
/// (design/orchestration/agents/consultant-runtime.md §4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerDescriptor {
    /// Catalog name of the consumer (e.g. `badgey`).
    pub name: &'static str,
    /// Instance-id prefix (e.g. `bg` → `bg-<8hex>-<8hex>`).
    pub id_prefix: &'static str,
    /// Prefix of the intent notes the post-turn processor consumes
    /// (e.g. `bg-action-`). Must match the grammar the consumer's persona
    /// lens instructs the model to emit.
    pub intent_note_prefix: &'static str,
    /// Brofile the consumer's persona dispatches with.
    pub brofile_ref: &'static str,
    /// Brofile for wrapper-mediated sub-bro (scout) dispatches.
    pub scout_brofile_ref: &'static str,
    /// Full intent-note kinds the consumer's code-owned handlers accept;
    /// the post-processor ignores any other note kind.
    pub action_kinds: &'static [&'static str],
    /// Proposal-kind vocabulary (serde snake_case strings) this consumer
    /// writes to the proposal store.
    pub proposal_kinds: &'static [&'static str],
}

impl ConsumerDescriptor {
    pub fn generate_id(&self) -> ConsultantId {
        ConsultantId::generate(self.id_prefix)
    }

    /// Parse an instance id and enforce this consumer's prefix.
    pub fn parse_id(&self, raw: &str) -> Result<ConsultantId, String> {
        let id = ConsultantId::from_str(raw).map_err(|_| self.bad_id(raw))?;
        if id.prefix() != self.id_prefix {
            return Err(self.bad_id(raw));
        }
        Ok(id)
    }

    pub fn is_action_kind(&self, kind: &str) -> bool {
        self.action_kinds.contains(&kind)
    }

    /// Compose a consumer-grammar note kind from a suffix, e.g.
    /// `action_note_kind("failed")` → `bg-action-failed` for Badgey.
    pub fn action_note_kind(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.intent_note_prefix)
    }

    pub fn is_proposal_kind(&self, kind: &str) -> bool {
        self.proposal_kinds.contains(&kind)
    }

    fn bad_id(&self, raw: &str) -> String {
        format!(
            "invalid {} id '{raw}', expected {}-<8hex>-<8hex>",
            self.name, self.id_prefix
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST: ConsumerDescriptor = ConsumerDescriptor {
        name: "testc",
        id_prefix: "tc",
        intent_note_prefix: "tc-action-",
        brofile_ref: "testc-persona",
        scout_brofile_ref: "testc-scout-persona",
        action_kinds: &["tc-action-emit-proposal"],
        proposal_kinds: &["packet", "agent"],
    };

    #[test]
    fn parse_id_enforces_consumer_prefix() {
        let id = TEST.generate_id();
        assert_eq!(id.prefix(), "tc");
        assert_eq!(TEST.parse_id(id.as_str()).unwrap(), id);
        let err = TEST.parse_id("bg-3f7a91c4-91ff04cc").unwrap_err();
        assert!(err.contains("invalid testc id"), "{err}");
        assert!(TEST.parse_id("tc-nope").is_err());
    }

    #[test]
    fn action_and_proposal_vocabulary_checks() {
        assert!(TEST.is_action_kind("tc-action-emit-proposal"));
        assert!(!TEST.is_action_kind("tc-action-unknown"));
        assert_eq!(TEST.action_note_kind("failed"), "tc-action-failed");
        assert!(TEST.is_proposal_kind("packet"));
        assert!(!TEST.is_proposal_kind("workflow"));
    }
}
