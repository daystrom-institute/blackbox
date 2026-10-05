//! The accepted publication's content for one queued checkout mutation path,
//! read the way the lane that queued the mutation reads it. An operator
//! requeue compares it with the base the mutation was computed from, so a
//! redelivery never overwrites content that moved underneath the row.

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;

use super::state::SharedState;

/// What the accepted publication holds at a queued path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublishedPathContent {
    /// The lane is known; `None` means the publication has no file there.
    Known(Option<String>),
    /// No lane of this daemon queues mutations at the path, so nothing can
    /// say what the publication holds for it.
    UnknownLane,
}

impl SharedState {
    /// The accepted publication's committed bytes at `relative_path` for a
    /// project whose accepted scope is `scope`. Knowledge and gap paths read
    /// the normalized committed form of the published record, as their edit
    /// lanes do; a configuration path reads the accepted bytes exactly, as
    /// the guarded lane does.
    pub(crate) fn published_path_content(
        &self,
        project_id: &ProjectId,
        scope: &PublishedScope,
        relative_path: &str,
    ) -> anyhow::Result<PublishedPathContent> {
        use bbox_code_source::ProjectConfigTargetV1;

        if let Some(target) = ProjectConfigTargetV1::from_relative_path(relative_path) {
            let accepted = self
                .load_accepted_project_config(project_id.as_str())
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            anyhow::ensure!(
                &accepted.scope == scope,
                "error.checkout_mutation_scope_changed: the accepted publication is at another scope"
            );
            return Ok(PublishedPathContent::Known(
                accepted.snapshot.accepted_bytes(&target).map(str::to_owned),
            ));
        }
        // `.bbox/<lane>/<id>.json`, one directory deep.
        let Some((lane, id)) = relative_path
            .strip_suffix(".json")
            .and_then(|path| path.rsplit_once('/'))
            .and_then(|(directory, id)| directory.strip_prefix(".bbox/").map(|lane| (lane, id)))
            .filter(|(lane, _)| matches!(*lane, "knowledge" | "gaps"))
        else {
            return Ok(PublishedPathContent::UnknownLane);
        };
        let runtime = self.accepted_publications.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "error.project_catalog_inactive: the accepted publication runtime is unavailable"
            )
        })?;
        let verified = runtime
            .load_verified(project_id)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        anyhow::ensure!(
            verified.content_stamp().accepted_scope() == scope,
            "error.checkout_mutation_scope_changed: the accepted publication is at another scope"
        );
        let bytes = if lane == "knowledge" {
            super::knowledge_view::published_knowledge_from_accepted(&verified)
                .entries
                .get(id)
                .map(|entry| {
                    bbox_knowledge::knowledge::committed_knowledge_entry_bytes(&entry.entry)
                })
                .transpose()?
        } else {
            super::gap_view::published_gaps_from_accepted(&verified)
                .gaps
                .get(id)
                .map(|entry| bbox_gaps::gaps::committed_gap_note_bytes(&entry.gap))
                .transpose()?
        };
        Ok(PublishedPathContent::Known(
            bytes.map(String::from_utf8).transpose()?,
        ))
    }
}
