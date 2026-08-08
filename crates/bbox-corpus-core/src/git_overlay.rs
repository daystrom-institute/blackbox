//! The typed Git current-file overlay selector (durable-project-catalog
//! governing section 11; Phase 3 plan section 10 item 1).
//!
//! Git history is an attachment-backed OVERLAY on an already-active code
//! generation, not a member of the code activation transaction. The selector
//! below is what makes that overlay identifiable rather than implicit: it
//! names the exact code generation the overlay's `COMMIT_TOUCHED_FILE` edges
//! target, the repo-history generation whose commit documents those edges
//! join against, and the attachment and head the walk actually read.
//!
//! Why every field is load-bearing:
//!
//! - `code_generation` is the anti-dangling field. `COMMIT_TOUCHED_FILE`
//!   targets embed a snapshot id and chunk hash, so an overlay built against
//!   generation A points at refs that do not exist under generation B.
//!   Comparing this field against the active code generation is how a reader
//!   decides the overlay is still joinable instead of discovering dangling
//!   targets at query time.
//! - `repo_history_generation` is the GC reference. A retained overlay is one
//!   of the durable inputs to the history reference manifest, so a generation
//!   named here can never be swept while the overlay is selected.
//! - `source` and `repo_head` are the freshness evidence. Attachment-backed
//!   overlays name the exact host-local attachment the walk read. Producer
//!   overlays instead name the authenticated producer and immutable accepted
//!   source generation; they deliberately carry no attachment sentinel.
//! - `overlay_generation` is a monotonic per-project counter, NOT an identity
//!   hash. It exists so a reader can tell two overlays apart when every other
//!   field happens to agree (a rebuild at the same head against the same
//!   attachment), which is what makes "did the overlay actually swap?"
//!   answerable from the manifest alone.
//!
//! This type is deliberately host-local runtime state, never catalog state:
//! it names an attachment, and attachments are host-local by the Phase 2
//! model. It is serialized into the edge sidecar's workspace manifest entry,
//! which is the single durable authority for the live selector (Phase 3 plan
//! section 4.7).

use serde::{Deserialize, Deserializer, Serialize};

/// Durable provenance for one selected Git overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitOverlaySourceV1 {
    Attachment {
        attachment_id: String,
    },
    ProducerTransport {
        producer_id: String,
        source_generation_id: String,
    },
}

impl GitOverlaySourceV1 {
    pub fn attachment_id(&self) -> Option<&str> {
        match self {
            Self::Attachment { attachment_id } => Some(attachment_id),
            Self::ProducerTransport { .. } => None,
        }
    }

    pub fn producer_transport(&self) -> Option<(&str, &str)> {
        match self {
            Self::ProducerTransport {
                producer_id,
                source_generation_id,
            } => Some((producer_id, source_generation_id)),
            Self::Attachment { .. } => None,
        }
    }
}

/// One project's selected Git current-file overlay.
///
/// Serde is deliberately NON-STRICT (no `deny_unknown_fields`): this rides
/// inside the edge-sidecar workspace manifest entry, which a older daemon
/// may have written without these fields and a newer one may extend. An
/// unknown field must degrade to "ignore it", never to a manifest that
/// refuses to load and takes every project's edges down with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitOverlaySelector {
    pub project_id: String,
    /// The code generation id this overlay's file edges target. An overlay
    /// whose value differs from the active code generation is stale by
    /// construction and must not be loaded.
    pub code_generation: String,
    /// The `rhg_`/`rhq_` history generation whose commit documents the
    /// overlay's edges join against. A GC reference for as long as the
    /// overlay is selected or retained.
    pub repo_history_generation: String,
    /// Exact source provenance. New writers always emit this discriminant;
    /// the custom reader below accepts the retired flat `attachment_id` form
    /// only long enough for the manifest coordinator to rewrite it.
    pub source: GitOverlaySourceV1,
    /// The head the walk observed. Compared against the attachment's current
    /// head to separate `current` from `lagging`.
    pub repo_head: String,
    /// The commit namespace the overlay's commit refs are keyed under: the
    /// owning repo-history record's PRIMARY namespace, never a compatibility
    /// namespace (D-037: new materialization routes through the primary).
    pub commit_namespace: String,
    /// Monotonic per-project overlay counter. Not an identity hash; see the
    /// module docs for why a counter rather than a content address.
    pub overlay_generation: u64,
}

impl<'de> Deserialize<'de> for GitOverlaySelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct CompatibleSelector {
            project_id: String,
            code_generation: String,
            repo_history_generation: String,
            #[serde(default)]
            source: Option<GitOverlaySourceV1>,
            #[serde(default)]
            attachment_id: Option<String>,
            repo_head: String,
            commit_namespace: String,
            overlay_generation: u64,
        }

        let compatible = CompatibleSelector::deserialize(deserializer)?;
        let source = match (compatible.source, compatible.attachment_id) {
            (Some(source), None) => source,
            (None, Some(attachment_id)) => GitOverlaySourceV1::Attachment { attachment_id },
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "Git overlay selector carries both typed and legacy source provenance",
                ));
            }
            (None, None) => {
                return Err(serde::de::Error::missing_field("source"));
            }
        };
        Ok(Self {
            project_id: compatible.project_id,
            code_generation: compatible.code_generation,
            repo_history_generation: compatible.repo_history_generation,
            source,
            repo_head: compatible.repo_head,
            commit_namespace: compatible.commit_namespace,
            overlay_generation: compatible.overlay_generation,
        })
    }
}

impl GitOverlaySelector {
    /// Whether this overlay is joinable against `code_generation`.
    ///
    /// The single predicate every reader must use. Spelling the comparison
    /// out at each call site is how the dangling-target class comes back.
    pub fn matches_code_generation(&self, code_generation: &str) -> bool {
        self.code_generation == code_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> GitOverlaySelector {
        GitOverlaySelector {
            project_id: "p_1".to_string(),
            code_generation: "gen-a".to_string(),
            repo_history_generation: format!("rhg_{}", "a".repeat(64)),
            source: GitOverlaySourceV1::Attachment {
                attachment_id: "att_1".to_string(),
            },
            repo_head: "b".repeat(40),
            commit_namespace: "ns1".to_string(),
            overlay_generation: 3,
        }
    }

    #[test]
    fn matches_only_its_own_code_generation() {
        let selector = selector();
        assert!(selector.matches_code_generation("gen-a"));
        assert!(!selector.matches_code_generation("gen-b"));
    }

    #[test]
    fn unknown_fields_decode_rather_than_refuse() {
        // The manifest entry this rides in must survive a forward-version
        // write; a strict decode would take every project's edges down.
        let json = serde_json::json!({
            "project_id": "p_1",
            "code_generation": "gen-a",
            "repo_history_generation": format!("rhg_{}", "a".repeat(64)),
            "attachment_id": "att_1",
            "repo_head": "b".repeat(40),
            "commit_namespace": "ns1",
            "overlay_generation": 3,
            "field_from_a_newer_daemon": true,
        });
        let decoded: GitOverlaySelector = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, selector());
    }

    #[test]
    fn round_trips_through_json() {
        let encoded = serde_json::to_string(&selector()).unwrap();
        let decoded: GitOverlaySelector = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, selector());
    }

    #[test]
    fn typed_and_legacy_sources_cannot_be_combined() {
        let mut json = serde_json::to_value(selector()).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("attachment_id".to_string(), serde_json::json!("att_legacy"));
        assert!(serde_json::from_value::<GitOverlaySelector>(json).is_err());
    }

    #[test]
    fn producer_transport_round_trips_without_attachment_sentinel() {
        let mut expected = selector();
        expected.source = GitOverlaySourceV1::ProducerTransport {
            producer_id: "producer-a".to_string(),
            source_generation_id: format!("ghs_{}", "c".repeat(64)),
        };
        let encoded = serde_json::to_value(&expected).unwrap();
        assert!(encoded.get("attachment_id").is_none());
        assert_eq!(
            serde_json::from_value::<GitOverlaySelector>(encoded).unwrap(),
            expected
        );
    }
}
