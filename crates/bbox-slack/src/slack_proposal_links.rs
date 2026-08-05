// SlackProposalLinks — persistent (team_id, channel_id, msg_ts) →
// (proposal_id, authoring_session_id, version, posted_at) map.
//
// One record per proposal posted into Slack by the daily triage brief.
// Three consumers:
//
//   1. Daily-triage tool (writer) — records a link the moment it
//      successfully chat.postMessages a proposal so the inbound side
//      can resolve back.
//
//   2. Reaction handler — resolves item_ts (the message a user
//      reacted to) into proposal_id, then routes to the existing
//      BadgeyProposalStore apply path.
//
//   3. Thread-reply handler — resolves thread_ts (the reply's parent)
//      into authoring_session_id, then bro_resumes that session with
//      the reply text so the agent that wrote the proposal is the
//      one that defends or refines it.
//
// Persistence is JSON at <store_dir>/slack-proposal-links.json with
// atomic-rename writes (mirror of slack_channel_bindings::save).
//
// Capped at MAX_ENTRIES with simple insertion-order eviction so a
// long-running daemon doesn't grow unbounded from old proposals
// (proposals are typically actioned within a day or two; older links
// rarely matter).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

const STORE_FILE: &str = "slack-proposal-links.json";
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackProposalLink {
    pub team_id: String,
    pub channel_id: String,
    /// Slack message ts of the posted proposal. Doubles as the
    /// thread root — replies in the thread arrive with `thread_ts`
    /// equal to this value.
    pub msg_ts: String,
    /// BadgeyProposalStore id (the canonical proposal identity).
    /// One Slack message ↔ one proposal.
    pub proposal_id: String,
    /// BadgeyInstance id (`bg-<8hex>-<8hex>`) that owns this proposal.
    /// `badgey_apply_proposal_internal` requires `(BadgeyId, proposal_id)`
    /// — recording the instance here keeps the future apply hook a
    /// trivial swap-in. None when the proposal was emitted by the
    /// simplified triage path that doesn't spawn sub-bros (the
    /// design's §6.3 sub-bro fanout is future work).
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Bro/Claude session id of the agent that authored the proposal,
    /// for thread-reply refinement loops. May be empty when the
    /// proposal was emitted by the simplified triage path that
    /// doesn't spawn sub-bros (the design's §6.3 sub-bro fanout is
    /// future work).
    #[serde(default)]
    pub authoring_session_id: Option<String>,
    /// Bumps on every refinement-driven chat.update. v1 on first
    /// post.
    pub version: u32,
    /// Project this proposal scopes to. Populated from the
    /// channel-binding at post time so consumers don't need a second
    /// lookup.
    pub project_dir: String,
    /// Resolving authority's project id, stamped on write. Absent on rows
    /// written before the catalog cut: those stay on the path lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// ISO8601 UTC post time.
    pub posted_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    /// Insertion order — used for cap eviction.
    /// Each entry is the composite key `<team_id>:<channel_id>:<msg_ts>`.
    order: Vec<String>,
    links: HashMap<String, SlackProposalLink>,
    /// Reverse index `<team_id>:<channel_id>:<proposal_id>` → composite
    /// forward key. Lets the apply handler resolve a (channel-scoped)
    /// proposal id back to its Slack message. Scoping by channel is
    /// load-bearing: in the multi-binding daily-fanout case, the
    /// simplified triage path emits non-unique synthesized ids like
    /// `triage-1` per channel run, and Badgey-store proposal ids are
    /// per-instance (not globally unique) anyway. A flat
    /// `proposal_id → key` map would cross-collide between channels.
    by_proposal: HashMap<String, String>,
}

/// Capture Slack proposal links that retain the legacy project directory.
pub fn capture_project_catalog_owner_snapshot(
    store_dir: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner, sha256_hex,
    };

    capture_json_owner(
        &store_dir.join(STORE_FILE),
        "slack_binding_proposal_link",
        "slack_binding_proposal_link:central-json",
        limits,
        |bytes| {
            let store: StoreData = serde_json::from_slice(bytes).map_err(|_| ())?;
            Ok(store
                .links
                .into_values()
                .flat_map(|link| {
                    let mut rows = Vec::new();
                    if let Some(project_id) = link
                        .project_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|project_id| !project_id.is_empty())
                    {
                        rows.push(OwnerSnapshotRowV1::inventory_target(
                            format!(
                                "{}:{}:{}:target",
                                link.team_id, link.channel_id, link.msg_ts
                            ),
                            project_id,
                            sha256_hex(bytes),
                        ));
                    }
                    let selector = link.project_dir.trim().to_string();
                    if link.project_id.is_none() && !selector.is_empty() {
                        rows.push(OwnerSnapshotRowV1::legacy_selector(
                            format!("{}:{}:{}", link.team_id, link.channel_id, link.msg_ts),
                            LegacyProjectSelectorKindV1::Project,
                            selector,
                        ));
                    }
                    rows
                })
                .collect())
        },
    )
}

/// Stamp one Slack proposal link with its stable project id, the write-back
/// inverse of [`capture_project_catalog_owner_snapshot`].
pub fn stamp_project_catalog_owner_row(
    store_dir: &Path,
    source_row_id: &str,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{stamp_json_map_row, stamp_json_owner_row};

    stamp_json_owner_row(
        &store_dir.join(STORE_FILE),
        "slack_binding_proposal_link",
        "slack_binding_proposal_link:central-json",
        limits,
        |bytes| {
            stamp_json_map_row(bytes, "links", source_row_id, project_id, |row| {
                let team_id = row.get("team_id")?.as_str()?;
                let channel_id = row.get("channel_id")?.as_str()?;
                let msg_ts = row.get("msg_ts")?.as_str()?;
                Some(format!("{team_id}:{channel_id}:{msg_ts}"))
            })
        },
    )
}

/// Read the stable project ids of MANY Slack proposal link rows, the VERIFY
/// half of [`stamp_project_catalog_owner_row`]. Locates the records exactly as
/// the stamper does, so the two agree on row identity by construction.
pub fn read_project_catalog_owner_rows(
    store_dir: &Path,
    source_row_ids: &std::collections::BTreeSet<String>,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        read_json_map_rows_project_id, read_json_owner_rows,
    };

    read_json_owner_rows(
        &store_dir.join(STORE_FILE),
        "slack_binding_proposal_link",
        "slack_binding_proposal_link:central-json",
        limits,
        |bytes| {
            read_json_map_rows_project_id(bytes, "links", source_row_ids, |row| {
                let team_id = row.get("team_id")?.as_str()?;
                let channel_id = row.get("channel_id")?.as_str()?;
                let msg_ts = row.get("msg_ts")?.as_str()?;
                Some(format!("{team_id}:{channel_id}:{msg_ts}"))
            })
        },
    )
}

#[derive(Debug)]
pub struct SlackProposalLinks {
    path: PathBuf,
    inner: RwLock<StoreData>,
    /// Serializes save() across concurrent writers. Without this two
    /// concurrent record/bump/forget/rename calls can race on the
    /// shared `<path>.json.tmp` temp file: one save's in-progress
    /// write gets stomped by another's truncating create, and a
    /// subsequent rename can publish a partially-written file.
    save_lock: Mutex<()>,
}

impl SlackProposalLinks {
    pub fn open(store_dir: &Path) -> Result<Self> {
        let path = store_dir.join(STORE_FILE);
        let data = if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            StoreData::default()
        };
        Ok(Self {
            path,
            inner: RwLock::new(data),
            save_lock: Mutex::new(()),
        })
    }

    pub fn lookup_by_msg(
        &self,
        team_id: &str,
        channel_id: &str,
        msg_ts: &str,
    ) -> Option<SlackProposalLink> {
        let key = compose_key(team_id, channel_id, msg_ts);
        self.inner.read().links.get(&key).cloned()
    }

    /// Look up a proposal scoped to a channel. Use this from any
    /// reverse-lookup path (apply hook, refine hook). Returns None
    /// when the channel has no record of this proposal_id — even if
    /// another channel happens to have a proposal with the same id.
    #[allow(dead_code)] // used by tests in same file
    pub fn lookup_by_proposal(
        &self,
        team_id: &str,
        channel_id: &str,
        proposal_id: &str,
    ) -> Option<SlackProposalLink> {
        let proposal_key = compose_proposal_key(team_id, channel_id, proposal_id);
        let data = self.inner.read();
        data.by_proposal
            .get(&proposal_key)
            .and_then(|key| data.links.get(key))
            .cloned()
    }

    /// Record a fresh link from a triage post. v1 on insert; if the
    /// (team, channel, msg_ts) composite already exists, the new
    /// record replaces it (no version bump — versioning is for
    /// in-place chat.update via `bump_version`, not re-recording).
    /// Re-recording the same (team, channel, proposal_id) under a
    /// different msg_ts moves the reverse arrow but does NOT delete
    /// the prior message link — concurrent posts of the same
    /// proposal_id across channels stay independent.
    pub fn record(&self, mut link: SlackProposalLink) -> Result<()> {
        if link.version == 0 {
            link.version = 1;
        }
        let key = compose_key(&link.team_id, &link.channel_id, &link.msg_ts);
        let proposal_key = compose_proposal_key(&link.team_id, &link.channel_id, &link.proposal_id);
        {
            let mut data = self.inner.write();
            data.by_proposal.remove(&proposal_key);
            data.order.retain(|k| k != &key);
            data.order.push(key.clone());
            data.by_proposal.insert(proposal_key, key.clone());
            data.links.insert(key, link);
            evict_under_cap(&mut data);
        }
        self.save()
    }

    /// Bump the version on a refined-in-place message (chat.update).
    /// Returns the new version. Returns None if the link doesn't
    /// exist.
    pub fn bump_version(
        &self,
        team_id: &str,
        channel_id: &str,
        msg_ts: &str,
    ) -> Result<Option<u32>> {
        let key = compose_key(team_id, channel_id, msg_ts);
        let next = {
            let mut data = self.inner.write();
            match data.links.get_mut(&key) {
                Some(link) => {
                    link.version = link.version.saturating_add(1);
                    Some(link.version)
                }
                None => None,
            }
        };
        if next.is_some() {
            self.save()?;
        }
        Ok(next)
    }

    /// Drop a link. Removes both the forward and reverse index
    /// entries. Returns the removed record, if any.
    #[allow(dead_code)] // used by tests in same file
    pub fn forget(
        &self,
        team_id: &str,
        channel_id: &str,
        msg_ts: &str,
    ) -> Result<Option<SlackProposalLink>> {
        let key = compose_key(team_id, channel_id, msg_ts);
        let removed = {
            let mut data = self.inner.write();
            let removed = data.links.remove(&key);
            if let Some(ref link) = removed {
                let proposal_key =
                    compose_proposal_key(&link.team_id, &link.channel_id, &link.proposal_id);
                data.by_proposal.remove(&proposal_key);
                data.order.retain(|k| k != &key);
            }
            removed
        };
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    /// Count records whose project_dir matches. Used by the project-rename
    /// pre-flight to surface the blast radius before mutating.
    pub fn project_ref_count(&self, project: &str) -> usize {
        self.inner
            .read()
            .links
            .values()
            .filter(|link| link.project_dir == project)
            .count()
    }

    /// Rewrite project_dir on every link that matched the old path.
    /// Returns the number of records updated. Persists once at the end
    /// if anything changed.
    pub fn rename_project_refs(&self, old_project: &str, new_project: &str) -> Result<usize> {
        let mut updated = 0usize;
        {
            let mut data = self.inner.write();
            for link in data.links.values_mut() {
                if link.project_dir == old_project {
                    link.project_dir = new_project.to_string();
                    updated += 1;
                }
            }
        }
        if updated > 0 {
            self.save()?;
        }
        Ok(updated)
    }

    /// Delete every proposal link owned by the project id or one of its
    /// recorded legacy directory selectors. Persists once and is idempotent.
    pub fn discharge_project_refs(&self, project_id: &str, selectors: &[String]) -> Result<usize> {
        let removed = {
            let mut data = self.inner.write();
            let keys = data
                .links
                .iter()
                .filter(|(_, link)| match link.project_id.as_deref() {
                    Some(owner) => owner == project_id,
                    None => selectors
                        .iter()
                        .any(|selector| selector == &link.project_dir),
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in &keys {
                if let Some(link) = data.links.remove(key) {
                    data.by_proposal.remove(&compose_proposal_key(
                        &link.team_id,
                        &link.channel_id,
                        &link.proposal_id,
                    ));
                }
            }
            data.order.retain(|key| !keys.contains(key));
            keys.len()
        };
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        // Hold save_lock for the full read-snapshot → write-tmp →
        // rename sequence. This blocks concurrent saves and ensures
        // the snapshot we serialize matches what we publish.
        let _guard = self.save_lock.lock();
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = {
            let data = self.inner.read();
            serde_json::to_string_pretty(&*data)?
        };
        let tmp = self.path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path)?;
        if let Some(parent) = self.path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

fn evict_under_cap(data: &mut StoreData) {
    while data.order.len() > MAX_ENTRIES {
        if let Some(oldest) = data.order.first().cloned() {
            data.order.remove(0);
            if let Some(removed) = data.links.remove(&oldest) {
                let proposal_key = compose_proposal_key(
                    &removed.team_id,
                    &removed.channel_id,
                    &removed.proposal_id,
                );
                data.by_proposal.remove(&proposal_key);
            }
        } else {
            break;
        }
    }
}

fn compose_key(team_id: &str, channel_id: &str, msg_ts: &str) -> String {
    format!("{team_id}:{channel_id}:{msg_ts}")
}

fn compose_proposal_key(team_id: &str, channel_id: &str, proposal_id: &str) -> String {
    format!("{team_id}:{channel_id}:{proposal_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: &str, proposal: &str, session: Option<&str>) -> SlackProposalLink {
        SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C01".into(),
            msg_ts: ts.into(),
            proposal_id: proposal.into(),
            instance_id: None,
            authoring_session_id: session.map(str::to_string),
            version: 1,
            project_dir: "/repo/x".into(),
            project_id: None,
            posted_at: "2026-05-07T06:00:00Z".into(),
        }
    }

    #[test]
    fn round_trip_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        assert!(store.lookup_by_msg("T01", "C01", "ts1").is_none());
        store
            .record(sample("ts1", "prop-abc", Some("session-1")))
            .unwrap();
        let got = store.lookup_by_msg("T01", "C01", "ts1").unwrap();
        assert_eq!(got.proposal_id, "prop-abc");
        assert_eq!(got.version, 1);
        assert_eq!(
            store
                .lookup_by_proposal("T01", "C01", "prop-abc")
                .unwrap()
                .msg_ts,
            "ts1"
        );
        // Reopen — survives.
        drop(store);
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        assert_eq!(
            store
                .lookup_by_proposal("T01", "C01", "prop-abc")
                .unwrap()
                .msg_ts,
            "ts1"
        );
    }

    #[test]
    fn version_bump_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        assert!(store.bump_version("T01", "C01", "ts1").unwrap().is_none());
        store.record(sample("ts1", "prop-abc", None)).unwrap();
        assert_eq!(store.bump_version("T01", "C01", "ts1").unwrap(), Some(2));
        assert_eq!(store.bump_version("T01", "C01", "ts1").unwrap(), Some(3));
        assert_eq!(store.lookup_by_msg("T01", "C01", "ts1").unwrap().version, 3);
    }

    #[test]
    fn re_recording_same_proposal_keeps_both_messages() {
        // Refinements post a NEW message in-thread with the SAME
        // proposal_id (chat.update happens elsewhere). Both message
        // links must survive so reactions/replies on either resolve.
        // The reverse arrow points to whichever was recorded most
        // recently.
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        store.record(sample("ts1", "prop-abc", None)).unwrap();
        store.record(sample("ts2", "prop-abc", None)).unwrap();
        assert_eq!(
            store
                .lookup_by_msg("T01", "C01", "ts1")
                .unwrap()
                .proposal_id,
            "prop-abc"
        );
        assert_eq!(
            store
                .lookup_by_msg("T01", "C01", "ts2")
                .unwrap()
                .proposal_id,
            "prop-abc"
        );
        assert_eq!(
            store
                .lookup_by_proposal("T01", "C01", "prop-abc")
                .unwrap()
                .msg_ts,
            "ts2"
        );
    }

    #[test]
    fn multi_binding_fanout_does_not_cross_collide() {
        // Regression for the daily-fanout case: simplified triage
        // emits non-unique synthesized proposal ids (`triage-1`,
        // `triage-2`, …) per channel run. Channel B's record must
        // not delete channel A's link — they're in different
        // channels, scoping by channel keeps them independent.
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        let chan_a_link = SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C-alpha".into(),
            msg_ts: "ts-a".into(),
            proposal_id: "triage-1".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/alpha".into(),
            project_id: None,
            posted_at: "2026-05-07T06:00:00Z".into(),
        };
        let chan_b_link = SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C-beta".into(),
            msg_ts: "ts-b".into(),
            proposal_id: "triage-1".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/beta".into(),
            project_id: None,
            posted_at: "2026-05-07T06:00:00Z".into(),
        };
        store.record(chan_a_link).unwrap();
        store.record(chan_b_link).unwrap();
        // Both forward links survive.
        assert_eq!(
            store
                .lookup_by_msg("T01", "C-alpha", "ts-a")
                .unwrap()
                .project_dir,
            "/repo/alpha"
        );
        assert_eq!(
            store
                .lookup_by_msg("T01", "C-beta", "ts-b")
                .unwrap()
                .project_dir,
            "/repo/beta"
        );
        // Reverse lookups are channel-scoped — each finds the right
        // message without cross-contamination.
        assert_eq!(
            store
                .lookup_by_proposal("T01", "C-alpha", "triage-1")
                .unwrap()
                .msg_ts,
            "ts-a"
        );
        assert_eq!(
            store
                .lookup_by_proposal("T01", "C-beta", "triage-1")
                .unwrap()
                .msg_ts,
            "ts-b"
        );
    }

    #[test]
    fn forget_drops_both_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        store.record(sample("ts1", "prop-abc", None)).unwrap();
        let removed = store.forget("T01", "C01", "ts1").unwrap();
        assert_eq!(removed.unwrap().proposal_id, "prop-abc");
        assert!(store.lookup_by_msg("T01", "C01", "ts1").is_none());
        assert!(store.lookup_by_proposal("T01", "C01", "prop-abc").is_none());
        // Idempotent.
        let again = store.forget("T01", "C01", "ts1").unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn rename_project_refs_updates_matching_records_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        store.record(sample("ts1", "p-a", None)).unwrap();
        store.record(sample("ts2", "p-b", None)).unwrap();
        // Different project — should not move.
        let other = SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C01".into(),
            msg_ts: "ts3".into(),
            proposal_id: "p-c".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/other".into(),
            project_id: None,
            posted_at: "2026-05-07T06:00:00Z".into(),
        };
        store.record(other).unwrap();
        assert_eq!(store.project_ref_count("/repo/x"), 2);
        assert_eq!(store.project_ref_count("/repo/other"), 1);
        let moved = store
            .rename_project_refs("/repo/x", "/repo/x-renamed")
            .unwrap();
        assert_eq!(moved, 2);
        assert_eq!(store.project_ref_count("/repo/x"), 0);
        assert_eq!(store.project_ref_count("/repo/x-renamed"), 2);
        assert_eq!(store.project_ref_count("/repo/other"), 1);
        // Persists.
        drop(store);
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        assert_eq!(store.project_ref_count("/repo/x-renamed"), 2);
    }

    #[test]
    fn distinct_keys_for_team_channel_msg() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        let a = SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C01".into(),
            msg_ts: "ts1".into(),
            proposal_id: "prop-a".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/a".into(),
            project_id: None,
            posted_at: "2026-05-07T06:00:00Z".into(),
        };
        let b = SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C02".into(),
            msg_ts: "ts1".into(),
            proposal_id: "prop-b".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/b".into(),
            project_id: None,
            posted_at: "2026-05-07T06:00:00Z".into(),
        };
        store.record(a).unwrap();
        store.record(b).unwrap();
        assert_eq!(
            store
                .lookup_by_msg("T01", "C01", "ts1")
                .unwrap()
                .proposal_id,
            "prop-a"
        );
        assert_eq!(
            store
                .lookup_by_msg("T01", "C02", "ts1")
                .unwrap()
                .proposal_id,
            "prop-b"
        );
    }

    #[test]
    fn proposal_link_without_project_id_decodes_and_round_trips() {
        let legacy = serde_json::json!({
            "team_id": "T01",
            "channel_id": "C01",
            "msg_ts": "ts-legacy",
            "proposal_id": "p1",
            "instance_id": "bg-1",
            "version": 1,
            "project_dir": "/repo/old",
            "posted_at": "2026-07-24T00:00:00Z"
        });
        let link: SlackProposalLink = serde_json::from_value(legacy).unwrap();
        assert_eq!(link.project_id, None);
        assert!(
            serde_json::to_value(&link)
                .unwrap()
                .get("project_id")
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        store.record(link).unwrap();
        let reopened = SlackProposalLinks::open(dir.path()).unwrap();
        let found = reopened.lookup_by_msg("T01", "C01", "ts-legacy").unwrap();
        assert_eq!(found.project_id, None);
        assert_eq!(found.project_dir, "/repo/old");
    }

    #[test]
    fn retirement_discharge_removes_owned_links_and_reverse_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackProposalLinks::open(dir.path()).unwrap();
        let mut owned = sample("ts-owned", "proposal-owned", None);
        owned.project_dir = "/repo/a".into();
        owned.project_id = Some("project-a".into());
        let mut other = sample("ts-other", "proposal-other", None);
        other.project_dir = "/repo/a".into();
        other.project_id = Some("project-b".into());
        store.record(owned).unwrap();
        store.record(other).unwrap();

        assert_eq!(
            store
                .discharge_project_refs("project-a", &["/repo/a".into()])
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .discharge_project_refs("project-a", &["/repo/a".into()])
                .unwrap(),
            0
        );
        assert!(
            store
                .lookup_by_proposal("T01", "C01", "proposal-owned")
                .is_none()
        );
        assert!(store.lookup_by_msg("T01", "C01", "ts-other").is_some());
    }
}

// ── Project-catalog row stamping (P6-B) ─────────────────────────

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MISSING,
        OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
    };

    const ROW_A: &str = "T1:C1:1.1";
    const ROW_B: &str = "T1:C2:2.2";

    fn write_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let store_dir = dir.path().canonicalize().unwrap();
        std::fs::write(
            store_dir.join(STORE_FILE),
            br#"{
  "order": ["k1", "k2"],
  "by_proposal": {},
  "links": {
    "k1": {"team_id": "T1", "channel_id": "C1", "msg_ts": "1.1", "project_dir": "/legacy/path/one", "future_field": {"kept": true}},
    "k2": {"team_id": "T1", "channel_id": "C2", "msg_ts": "2.2", "project_dir": "/legacy/path/two"}
  }
}
"#,
        )
        .unwrap();
        store_dir
    }

    fn read_bytes(store_dir: &std::path::Path) -> Vec<u8> {
        std::fs::read(store_dir.join(STORE_FILE)).unwrap()
    }

    fn read_row(store_dir: &std::path::Path, row: &str) -> serde_json::Value {
        let document: serde_json::Value = serde_json::from_slice(&read_bytes(store_dir)).unwrap();
        document["links"]
            .as_object()
            .unwrap()
            .values()
            .find(|value| {
                format!(
                    "{}:{}:{}",
                    value["team_id"].as_str().unwrap(),
                    value["channel_id"].as_str().unwrap(),
                    value["msg_ts"].as_str().unwrap()
                ) == row
            })
            .cloned()
            .unwrap()
    }

    fn stamp(
        store_dir: &std::path::Path,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            store_dir,
            row,
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = write_fixture(&dir);

        assert_eq!(
            stamp(&store_dir, ROW_A, "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&store_dir, ROW_A);
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED for dual-read.
        assert_eq!(row["project_dir"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one row must not touch its neighbours.
        assert!(read_row(&store_dir, ROW_B).get("project_id").is_none());
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = write_fixture(&dir);

        stamp(&store_dir, ROW_A, "a1b2c3d4").unwrap();
        let after_first = read_bytes(&store_dir);

        assert_eq!(
            stamp(&store_dir, ROW_A, "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        assert_eq!(read_bytes(&store_dir), after_first);
    }

    /// Never a silent overwrite.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = write_fixture(&dir);

        stamp(&store_dir, ROW_A, "a1b2c3d4").unwrap();
        let before = read_bytes(&store_dir);

        let error = stamp(&store_dir, ROW_A, "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&store_dir, ROW_A)["project_id"], "a1b2c3d4");
        assert_eq!(read_bytes(&store_dir), before);
    }

    /// Absence is a refusal, never a success.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = write_fixture(&dir);

        let error = stamp(&store_dir, "T9:C9:9.9", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create it.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().canonicalize().unwrap().join("absent");

        let error = stamp(&store_dir, ROW_A, "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_SOURCE_MISSING);
        assert!(!store_dir.join(STORE_FILE).exists());
    }
}
