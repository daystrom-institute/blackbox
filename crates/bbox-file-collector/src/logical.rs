//! Collision-safe logical-path assignment (design section 12).
//!
//! Deriving one path is [`bbox_file_source::derive_logical_path`]'s job.
//! ASSIGNING paths across a whole source is this module's, and it is a
//! different problem: two remote entries can derive the same path, and which
//! one keeps the bare name must not depend on enumeration order or the corpus
//! churns paths for no reason.
//!
//! The rule: assignment runs over the UNION of the journal and the incoming
//! batch, groups by case-folded collision key, and suffixes EVERY member of
//! any group with more than one member. A group of one keeps its bare path.
//!
//! Why suffix everyone rather than let the first claimant keep the bare name:
//! "first claimant" depends on enumeration order, so the pretty name would
//! migrate between documents across scans. Suffixing the whole group makes
//! assignment a pure function of the group's membership, and once a group has
//! two members it stays suffixed no matter who joins or leaves. The cost is a
//! single one-time churn at the 1-to-2 transition, which is honest and
//! explainable; the alternative is unexplainable churn forever.
//!
//! Why the union rather than the batch: an incremental batch carrying one
//! member of a collision group cannot see the other member. Assigning from the
//! batch alone would un-suffix a path that must stay suffixed, and silently
//! shadow a document.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::connector::RemoteEntry;
use crate::journal::Journal;

/// One entry participating in assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentInput {
    pub remote_id: String,
    pub name_path: Vec<String>,
    /// `Some` for a provider-native export: the exported extension replaces
    /// whatever the remote name carried.
    pub export_extension: Option<String>,
}

impl AssignmentInput {
    pub fn from_remote(entry: &RemoteEntry, export_extension: Option<String>) -> Self {
        Self {
            remote_id: entry.remote_id.clone(),
            name_path: entry.name_path.clone(),
            export_extension,
        }
    }
}

/// Assign collision-free logical paths for the union of the journal's known
/// entries and this cycle's batch.
///
/// Returns `remote_id -> logical_path` covering every input. Batch entries
/// override journal rows with the same `remote_id`, which is how a rename
/// lands: same id, new name path, new logical path, same bytes.
pub fn assign_logical_paths(
    journal: &Journal,
    batch: &[AssignmentInput],
    removed: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut inputs: BTreeMap<String, AssignmentInput> = BTreeMap::new();
    for (remote_id, entry) in &journal.entries {
        inputs.insert(
            remote_id.clone(),
            AssignmentInput {
                remote_id: remote_id.clone(),
                name_path: entry.name_path.clone(),
                export_extension: entry.export_format.clone(),
            },
        );
    }
    for input in batch {
        inputs.insert(input.remote_id.clone(), input.clone());
    }
    for remote_id in removed {
        inputs.remove(remote_id);
    }

    // Pass one: derive the bare path for every participant.
    let mut bare: BTreeMap<String, String> = BTreeMap::new();
    for (remote_id, input) in &inputs {
        // A journal row written before this field existed carries an empty
        // name path. Skip it rather than deriving a wrong path from nothing:
        // it keeps whatever path it already has until the remote reports it
        // again, which the next complete enumeration does.
        if input.name_path.is_empty() {
            continue;
        }
        let path = bbox_file_source::derive_logical_path(
            &input.name_path,
            input.export_extension.as_deref(),
        )?;
        bare.insert(remote_id.clone(), path);
    }

    // Pass two: group by the case-folded collision key.
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (remote_id, path) in &bare {
        groups
            .entry(bbox_file_source::collision_key(path))
            .or_default()
            .push(remote_id.as_str());
    }

    // Pass three: a group of one keeps its bare path; every member of a
    // larger group is suffixed from its own remote_id.
    let mut assigned: BTreeMap<String, String> = BTreeMap::new();
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    for members in groups.values() {
        for remote_id in members {
            let path = &bare[*remote_id];
            let resolved = if members.len() == 1 {
                path.clone()
            } else {
                bbox_file_source::apply_collision_suffix(path, remote_id)
            };
            let key = bbox_file_source::collision_key(&resolved);
            if let Some(owner) = taken.get(&key)
                && owner.as_str() != *remote_id
            {
                // Two distinct remote ids whose SUFFIXED paths collide. The
                // suffix is a sha256 prefix over the remote id, so this is a
                // genuine digest collision or a connector reporting one id
                // twice. Either way, publishing would shadow a document.
                bail!(
                    "logical path assignment could not separate {owner} and {remote_id}: \
                     both resolve to {resolved}"
                );
            }
            taken.insert(key, (*remote_id).to_string());
            bbox_file_source::validate_logical_path(&resolved)?;
            assigned.insert((*remote_id).to_string(), resolved);
        }
    }
    Ok(assigned)
}

/// The faithful remote leaf name, retained for producer-side status output.
pub fn display_name(name_path: &[String]) -> String {
    name_path.last().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JournalEntry, JournalState};

    fn input(remote_id: &str, path: &[&str]) -> AssignmentInput {
        AssignmentInput {
            remote_id: remote_id.into(),
            name_path: path.iter().map(|part| (*part).to_string()).collect(),
            export_extension: None,
        }
    }

    fn journal_row(remote_id: &str, path: &[&str], logical: &str) -> JournalEntry {
        JournalEntry {
            remote_id: remote_id.into(),
            remote_version: "v1".into(),
            logical_path: logical.into(),
            name_path: path.iter().map(|part| (*part).to_string()).collect(),
            display_name: display_name(
                &path
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect::<Vec<_>>(),
            ),
            export_format: None,
            remote_url: None,
            state: JournalState::Published {
                content_hash: "a".repeat(64),
                size: 4,
            },
        }
    }

    #[test]
    fn a_lone_entry_keeps_its_bare_derived_path() {
        let assigned = assign_logical_paths(
            &Journal::default(),
            &[input("r1", &["Ops", "Report.md"])],
            &[],
        )
        .unwrap();
        assert_eq!(assigned["r1"], "Ops/Report.md");
    }

    #[test]
    fn assignment_does_not_depend_on_enumeration_order() {
        let forward = assign_logical_paths(
            &Journal::default(),
            &[
                input("r1", &["Ops", "Report.md"]),
                input("r2", &["Ops", "report.md"]),
            ],
            &[],
        )
        .unwrap();
        let reversed = assign_logical_paths(
            &Journal::default(),
            &[
                input("r2", &["Ops", "report.md"]),
                input("r1", &["Ops", "Report.md"]),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            forward, reversed,
            "which document keeps which path must not depend on walk order"
        );
    }

    #[test]
    fn every_member_of_a_collision_group_is_suffixed_and_both_stay_reachable() {
        // A vendor duplicate-sibling pair and a case-collision pair are the
        // same problem to this code.
        let assigned = assign_logical_paths(
            &Journal::default(),
            &[
                input("r1", &["Ops", "Report.md"]),
                input("r2", &["Ops", "report.md"]),
                input("r3", &["Ops", "Other.md"]),
            ],
            &[],
        )
        .unwrap();
        assert_ne!(assigned["r1"], assigned["r2"]);
        assert_ne!(
            bbox_file_source::collision_key(&assigned["r1"]),
            bbox_file_source::collision_key(&assigned["r2"]),
            "both entries must be distinctly reachable, not merely distinct strings"
        );
        assert!(assigned["r1"].ends_with(".md") && assigned["r2"].ends_with(".md"));
        assert_eq!(
            assigned["r3"], "Ops/Other.md",
            "an uncontested path is untouched by someone else's collision"
        );
    }

    #[test]
    fn a_collision_group_stays_suffixed_across_an_incremental_batch() {
        // The journal holds both members; the delta carries only one. Without
        // the union, the lone batch member would look uncontested and get
        // un-suffixed, silently shadowing its partner.
        let mut journal = Journal::default();
        journal.upsert(journal_row(
            "r1",
            &["Ops", "Report.md"],
            "Ops/Report-aaaaaaaa.md",
        ));
        journal.upsert(journal_row(
            "r2",
            &["Ops", "report.md"],
            "Ops/report-bbbbbbbb.md",
        ));

        let assigned =
            assign_logical_paths(&journal, &[input("r2", &["Ops", "report.md"])], &[]).unwrap();
        assert_ne!(
            bbox_file_source::collision_key(&assigned["r1"]),
            bbox_file_source::collision_key(&assigned["r2"])
        );
        assert!(
            assigned["r2"] != "Ops/report.md",
            "the batch member must stay suffixed: its partner is still in the journal"
        );
    }

    #[test]
    fn removing_one_member_returns_the_survivor_to_its_bare_path() {
        let mut journal = Journal::default();
        journal.upsert(journal_row(
            "r1",
            &["Ops", "Report.md"],
            "Ops/Report-aaaaaaaa.md",
        ));
        journal.upsert(journal_row(
            "r2",
            &["Ops", "report.md"],
            "Ops/report-bbbbbbbb.md",
        ));

        let assigned = assign_logical_paths(&journal, &[], &["r2".to_string()]).unwrap();
        assert_eq!(assigned.len(), 1);
        assert_eq!(
            assigned["r1"], "Ops/Report.md",
            "a group that shrinks to one is no longer a collision"
        );
    }

    #[test]
    fn a_rename_moves_the_path_under_a_stable_remote_id() {
        let mut journal = Journal::default();
        journal.upsert(journal_row("r1", &["Ops", "old.md"], "Ops/old.md"));
        let assigned =
            assign_logical_paths(&journal, &[input("r1", &["Ops", "new.md"])], &[]).unwrap();
        assert_eq!(assigned.len(), 1, "a rename is one entry, not two");
        assert_eq!(assigned["r1"], "Ops/new.md");
    }

    #[test]
    fn a_native_export_extension_participates_in_assignment() {
        let mut docx = input("r1", &["Ops", "Runbook"]);
        docx.export_extension = Some("docx".into());
        let assigned = assign_logical_paths(&Journal::default(), &[docx], &[]).unwrap();
        assert_eq!(assigned["r1"], "Ops/Runbook.docx");
    }

    #[test]
    fn a_journal_row_predating_the_name_path_field_is_left_alone() {
        let mut journal = Journal::default();
        let mut legacy = journal_row("r1", &[], "Ops/legacy.md");
        legacy.name_path = Vec::new();
        journal.upsert(legacy);
        let assigned = assign_logical_paths(&journal, &[], &[]).unwrap();
        assert!(
            !assigned.contains_key("r1"),
            "a row with no recorded name path keeps whatever path it has \
             rather than deriving a wrong one from nothing"
        );
    }
}
