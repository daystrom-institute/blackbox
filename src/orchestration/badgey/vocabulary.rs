//! Badgey's consumer vocabulary: the descriptor binding Badgey to the generic
//! consultant runtime, and the proposal-kind enum Badgey's handlers match on.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::orchestration::consultant::descriptor::ConsumerDescriptor;

pub const BADGEY: ConsumerDescriptor = ConsumerDescriptor {
    name: "badgey",
    id_prefix: "bg",
    intent_note_prefix: "bg-action-",
    brofile_ref: "badgey-persona",
    scout_brofile_ref: "badgey-scout-persona",
    action_kinds: &[
        "bg-action-emit-proposal",
        "bg-action-spawn-subbro",
        "bg-action-escalate-dispute",
        "bg-action-extend-budget",
    ],
    proposal_kinds: &[
        "workflow",
        "packet",
        "brofile",
        "lens",
        "agent",
        "redispatch_task",
        "artifact_promotion",
    ],
};

pub fn descriptor() -> &'static ConsumerDescriptor {
    &BADGEY
}

/// Badgey's proposal kinds. Serialized as snake_case strings; the generic
/// proposal store carries the string form (`ConsultantProposal::kind`), and
/// Badgey code parses back to this enum at the tools boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Workflow,
    Packet,
    Brofile,
    Lens,
    Agent,
    RedispatchTask,
    ArtifactPromotion,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Packet => "packet",
            Self::Brofile => "brofile",
            Self::Lens => "lens",
            Self::Agent => "agent",
            Self::RedispatchTask => "redispatch_task",
            Self::ArtifactPromotion => "artifact_promotion",
        }
    }
}

impl FromStr for ProposalKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "workflow" => Ok(Self::Workflow),
            "packet" => Ok(Self::Packet),
            "brofile" => Ok(Self::Brofile),
            "lens" => Ok(Self::Lens),
            "agent" => Ok(Self::Agent),
            "redispatch_task" => Ok(Self::RedispatchTask),
            "artifact_promotion" => Ok(Self::ArtifactPromotion),
            other => Err(format!("unknown proposal kind: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_kind_string_round_trip_matches_serde() {
        for kind in [
            ProposalKind::Workflow,
            ProposalKind::Packet,
            ProposalKind::Brofile,
            ProposalKind::Lens,
            ProposalKind::Agent,
            ProposalKind::RedispatchTask,
            ProposalKind::ArtifactPromotion,
        ] {
            let serde_str = serde_json::to_value(kind).unwrap();
            assert_eq!(serde_str.as_str().unwrap(), kind.as_str());
            assert_eq!(kind.as_str().parse::<ProposalKind>().unwrap(), kind);
            assert!(BADGEY.is_proposal_kind(kind.as_str()));
        }
    }

    #[test]
    fn descriptor_matches_legacy_badgey_grammar() {
        assert_eq!(BADGEY.parse_id("bg-3f7a91c4-91ff04cc").unwrap().prefix(), "bg");
        assert!(BADGEY.parse_id("xx-3f7a91c4-91ff04cc").is_err());
        assert!(BADGEY.is_action_kind("bg-action-emit-proposal"));
        assert_eq!(BADGEY.action_note_kind("failed"), "bg-action-failed");
    }
}
