use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::types::ConsultantId;

/// Code-owned consumer hook selection. The runtime turn loop consults this
/// instead of comparing consumer names: a descriptor *selects* a compiled-in
/// hook set (wrapper-command grammar + intent post-processor), it can never
/// define one in data (concern 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerHooks {
    /// No wrapper commands, no intent post-processing.
    None,
    /// Badgey's wrapper-command grammar (`orchestration::badgey::commands`)
    /// and `bg-action-*` intent post-processor.
    Badgey,
}

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
    /// Catalog name of the consumer (e.g. `badgey`). Used as the bro label,
    /// thread-name prefix (`<name>:<id>`), session name, and scope-bind
    /// header (`[<name>-scope]`).
    pub name: &'static str,
    /// Human-facing display name (e.g. `Badgey`) for thread topics and
    /// operator-visible prose.
    pub display_name: &'static str,
    /// Agent catalog ref used as the dispatch label (e.g. `agent:badgey@v1`).
    pub agent_ref: &'static str,
    /// First-turn instruction appended after the scope-bind block on exec.
    pub exec_init_prompt: &'static str,
    /// Per-consultation soft token budget surfaced in the scope-bind block;
    /// budget-extend commands add this amount again.
    pub turn_budget_tokens: u64,
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
    /// Compiled-in hook set the turn loop runs for this consumer.
    pub hooks: ConsumerHooks,
    /// Legacy on-disk state subdirectory. Badgey keeps its pre-dissolution
    /// `state_dir/badgey/` layout permanently (migration judged riskier than
    /// the path asymmetry — design §4.1/§5 Phase 4); new consumers get
    /// `state_dir/consultant/<name>/`.
    pub legacy_state_subdir: Option<&'static str>,
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

    fn state_root(&self, state_dir: &Path) -> PathBuf {
        match self.legacy_state_subdir {
            Some(subdir) => state_dir.join(subdir),
            None => state_dir.join("consultant").join(self.name),
        }
    }

    /// Proposal-store root for this consumer under the daemon state dir.
    pub fn proposals_root(&self, state_dir: &Path) -> PathBuf {
        self.state_root(state_dir).join("proposals")
    }

    /// Action-journal root for this consumer under the daemon state dir.
    pub fn action_journal_root(&self, state_dir: &Path) -> PathBuf {
        self.state_root(state_dir).join("action_journal")
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
        display_name: "Testc",
        agent_ref: "agent:testc@v1",
        exec_init_prompt: "Initialize this Testc consultation.",
        turn_budget_tokens: 1_000,
        id_prefix: "tc",
        intent_note_prefix: "tc-action-",
        brofile_ref: "testc-persona",
        scout_brofile_ref: "testc-scout-persona",
        action_kinds: &["tc-action-emit-proposal"],
        proposal_kinds: &["packet", "agent"],
        hooks: ConsumerHooks::None,
        legacy_state_subdir: None,
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
    fn state_roots_follow_legacy_subdir_policy() {
        let dir = Path::new("/state");
        assert_eq!(
            TEST.proposals_root(dir),
            Path::new("/state/consultant/testc/proposals")
        );
        let legacy = ConsumerDescriptor {
            legacy_state_subdir: Some("testc-legacy"),
            ..TEST.clone()
        };
        assert_eq!(
            legacy.action_journal_root(dir),
            Path::new("/state/testc-legacy/action_journal")
        );
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
