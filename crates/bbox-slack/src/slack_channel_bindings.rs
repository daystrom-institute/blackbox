// SlackChannelBindings — persistent (team_id, channel_id) → project map.
//
// Bindings tie a Slack channel to a single bbox project so any badgey
// activity originating in (or destined for) that channel resolves to a
// stable scope. Two consumers:
//
//   1. Inbound — webhook handlers resolve a channel-bound project at
//      ingress so app_mention / reaction / message events know which
//      project's threads, proposals, and bro sessions they belong to.
//
//   2. Outbound — the daily-triage cron fans out per binding (one
//      triage run per bound channel, scoped to that project, posting
//      proposals into that channel). Zero bindings = no-op fanout.
//
// Persistence is JSON at <store_dir>/slack-channel-bindings.json with
// atomic-rename writes (same shape as slack_thread_store::save).
//
// Composite key is `<team_id>:<channel_id>` so a single bbox host can
// serve multiple Slack workspaces without channel-id collisions.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

const STORE_FILE: &str = "slack-channel-bindings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelBinding {
    pub team_id: String,
    pub channel_id: String,
    /// Display-only channel name (e.g. `transcript-search`). Slack
    /// channel renames are id-stable, so this is for human readability
    /// only — never used as a lookup key.
    #[serde(default)]
    pub channel_name: Option<String>,
    /// Absolute project path. The canonical lookup token for
    /// downstream consumers (cron fanout, badgey_triage_inbox scope).
    pub project_dir: String,
    /// Resolved 8-hex project id from ProjectRegistry, when the path
    /// is registered. Captured at bind time as a convenience; consumers
    /// that need a stable id should re-resolve through the registry to
    /// pick up rename / re-registration.
    #[serde(default)]
    pub project_id: Option<String>,
    /// ISO8601 UTC bind time.
    pub registered_at: String,
    /// Optional bbox_user that performed the bind (from the MCP
    /// caller's identity context, when available).
    #[serde(default)]
    pub registered_by: Option<String>,
    /// Badgey instance id (`bg-<8hex>-<8hex>`) that serves this
    /// channel as the triage / brief authoring agent. Populated
    /// lazily on first triage cycle (see `post_triage_brief_with_state`).
    /// Stored here so subsequent ticks resume the same instance for
    /// continuity, and so unbind can dismiss the instance cleanly.
    #[serde(default)]
    pub badgey_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    bindings: HashMap<String, ChannelBinding>,
}

/// Capture channel bindings that retain the legacy project directory.
pub fn capture_project_catalog_owner_snapshot(
    store_dir: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, capture_json_owner,
    };

    capture_json_owner(
        &store_dir.join(STORE_FILE),
        "slack_binding",
        "slack_binding:channel-json",
        limits,
        |bytes| {
            let store: StoreData = serde_json::from_slice(bytes).map_err(|_| ())?;
            Ok(store
                .bindings
                .into_values()
                .filter_map(|binding| {
                    let selector = binding.project_dir.trim().to_string();
                    (!selector.is_empty()).then(|| {
                        OwnerSnapshotRowV1::legacy_selector(
                            format!("{}:{}", binding.team_id, binding.channel_id),
                            LegacyProjectSelectorKindV1::Project,
                            selector,
                        )
                    })
                })
                .collect())
        },
    )
}

#[derive(Debug)]
pub struct SlackChannelBindings {
    path: PathBuf,
    inner: RwLock<StoreData>,
    /// Serializes save() across concurrent writers. Without this two
    /// concurrent bind/unbind/rename calls can race on the shared
    /// `<path>.json.tmp` temp file: one save's in-progress write
    /// gets stomped by another's truncating create, and a subsequent
    /// rename can publish a partially-written file.
    save_lock: Mutex<()>,
}

impl SlackChannelBindings {
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

    pub fn lookup(&self, team_id: &str, channel_id: &str) -> Option<ChannelBinding> {
        let key = compose_key(team_id, channel_id);
        self.inner.read().bindings.get(&key).cloned()
    }

    /// Bind a channel to a project. Idempotent; re-binding the same
    /// channel updates project_dir / project_id / channel_name and
    /// refreshes registered_at.
    pub fn bind(&self, binding: ChannelBinding) -> Result<()> {
        let key = compose_key(&binding.team_id, &binding.channel_id);
        {
            let mut data = self.inner.write();
            data.bindings.insert(key, binding);
        }
        self.save()
    }

    /// Set / update the Badgey instance id for an existing binding.
    /// Returns Ok(true) if the binding existed and was updated, Ok(false)
    /// if no binding for that (team, channel) was found. Persists once
    /// on update.
    pub fn set_badgey_id(
        &self,
        team_id: &str,
        channel_id: &str,
        badgey_id: Option<String>,
    ) -> Result<bool> {
        let key = compose_key(team_id, channel_id);
        let updated = {
            let mut data = self.inner.write();
            if let Some(b) = data.bindings.get_mut(&key) {
                b.badgey_id = badgey_id;
                true
            } else {
                false
            }
        };
        if updated {
            self.save()?;
        }
        Ok(updated)
    }

    /// Remove a binding. Returns the previous record, if any.
    pub fn unbind(&self, team_id: &str, channel_id: &str) -> Result<Option<ChannelBinding>> {
        let key = compose_key(team_id, channel_id);
        let removed = {
            let mut data = self.inner.write();
            data.bindings.remove(&key)
        };
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    /// All bindings, optionally filtered by project_dir or team_id.
    pub fn list(&self, team_id: Option<&str>, project_dir: Option<&str>) -> Vec<ChannelBinding> {
        let data = self.inner.read();
        let mut out: Vec<ChannelBinding> = data
            .bindings
            .values()
            .filter(|b| team_id.is_none_or(|t| b.team_id == t))
            .filter(|b| project_dir.is_none_or(|p| b.project_dir == p))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.team_id
                .cmp(&b.team_id)
                .then_with(|| a.channel_id.cmp(&b.channel_id))
        });
        out
    }

    pub fn rename_project_refs(
        &self,
        old_project_dir: &str,
        new_project_dir: &str,
        project_id: Option<&str>,
    ) -> Result<usize> {
        let mut updated = 0usize;
        {
            let mut data = self.inner.write();
            for binding in data.bindings.values_mut() {
                if binding.project_dir == old_project_dir {
                    binding.project_dir = new_project_dir.to_string();
                    binding.project_id = project_id.map(str::to_string);
                    updated += 1;
                }
            }
        }
        if updated > 0 {
            self.save()?;
        }
        Ok(updated)
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
        Ok(())
    }
}

fn compose_key(team_id: &str, channel_id: &str) -> String {
    format!("{team_id}:{channel_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(team: &str, ch: &str, name: &str, project: &str) -> ChannelBinding {
        ChannelBinding {
            team_id: team.into(),
            channel_id: ch.into(),
            channel_name: Some(name.into()),
            project_dir: project.into(),
            project_id: None,
            registered_at: "2026-05-07T06:00:00Z".into(),
            registered_by: Some("mathieu".into()),
            badgey_id: None,
        }
    }

    #[test]
    fn set_badgey_id_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        assert!(
            !store
                .set_badgey_id("T01", "C01", Some("bg-deadbeef-cafef00d".into()))
                .unwrap()
        );
        store.bind(sample("T01", "C01", "ts", "/repo/x")).unwrap();
        assert!(
            store
                .set_badgey_id("T01", "C01", Some("bg-deadbeef-cafef00d".into()))
                .unwrap()
        );
        assert_eq!(
            store.lookup("T01", "C01").unwrap().badgey_id.as_deref(),
            Some("bg-deadbeef-cafef00d")
        );
        // Reopen — survives.
        drop(store);
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        assert_eq!(
            store.lookup("T01", "C01").unwrap().badgey_id.as_deref(),
            Some("bg-deadbeef-cafef00d")
        );
        // Clear.
        store.set_badgey_id("T01", "C01", None).unwrap();
        assert!(store.lookup("T01", "C01").unwrap().badgey_id.is_none());
    }

    #[test]
    fn round_trip_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        assert!(store.lookup("T01", "C01").is_none());
        store
            .bind(sample("T01", "C01", "transcript-search", "/repo/ts"))
            .unwrap();
        let got = store.lookup("T01", "C01").unwrap();
        assert_eq!(got.project_dir, "/repo/ts");
        assert_eq!(got.channel_name.as_deref(), Some("transcript-search"));
        // Reopen — survives.
        drop(store);
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        assert_eq!(store.lookup("T01", "C01").unwrap().project_dir, "/repo/ts");
    }

    #[test]
    fn rebind_is_idempotent_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        store
            .bind(sample("T01", "C01", "transcript-search", "/repo/ts"))
            .unwrap();
        store
            .bind(sample(
                "T01",
                "C01",
                "transcript-search-renamed",
                "/repo/ts2",
            ))
            .unwrap();
        assert_eq!(store.list(None, None).len(), 1);
        let got = store.lookup("T01", "C01").unwrap();
        assert_eq!(got.project_dir, "/repo/ts2");
        assert_eq!(
            got.channel_name.as_deref(),
            Some("transcript-search-renamed")
        );
    }

    #[test]
    fn unbind_returns_prior_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        store
            .bind(sample("T01", "C01", "transcript-search", "/repo/ts"))
            .unwrap();
        let prior = store.unbind("T01", "C01").unwrap();
        assert_eq!(prior.unwrap().project_dir, "/repo/ts");
        assert!(store.lookup("T01", "C01").is_none());
        // Idempotent unbind.
        let again = store.unbind("T01", "C01").unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn list_filters_by_team_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        store.bind(sample("T01", "C01", "ts", "/repo/a")).unwrap();
        store.bind(sample("T01", "C02", "el", "/repo/b")).unwrap();
        store.bind(sample("T02", "C03", "lt", "/repo/a")).unwrap();
        assert_eq!(store.list(None, None).len(), 3);
        assert_eq!(store.list(Some("T01"), None).len(), 2);
        assert_eq!(store.list(None, Some("/repo/a")).len(), 2);
        assert_eq!(store.list(Some("T01"), Some("/repo/a")).len(), 1);
    }

    #[test]
    fn distinct_keys_for_team_and_channel() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackChannelBindings::open(dir.path()).unwrap();
        store
            .bind(sample("T01", "C01", "alpha", "/repo/a"))
            .unwrap();
        store
            .bind(sample("T02", "C01", "alpha-other-ws", "/repo/b"))
            .unwrap();
        assert_eq!(store.lookup("T01", "C01").unwrap().project_dir, "/repo/a");
        assert_eq!(store.lookup("T02", "C01").unwrap().project_dir, "/repo/b");
    }

    #[test]
    fn migration_snapshot_is_read_only_and_canonical() {
        use bbox_corpus_core::project_catalog_snapshot::{
            OwnerSnapshotLimitsV1, OwnerSnapshotRowValueV1, OwnerSnapshotStateV1,
        };

        let dir = tempfile::tempdir().unwrap();
        let missing =
            capture_project_catalog_owner_snapshot(dir.path(), OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(!dir.path().join(STORE_FILE).exists());

        let store = SlackChannelBindings::open(dir.path()).unwrap();
        store
            .bind(sample("T01", "C01", "alpha", "/repo/a"))
            .unwrap();
        store.bind(sample("T02", "C02", "beta", "/repo/b")).unwrap();
        let first =
            capture_project_catalog_owner_snapshot(dir.path(), OwnerSnapshotLimitsV1::default())
                .unwrap();
        let second =
            capture_project_catalog_owner_snapshot(dir.path(), OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(first.canonical_sha256, second.canonical_sha256);
        assert_eq!(first.row_count, 2);
        assert!(first.rows.iter().any(|row| matches!(
            &row.value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector {
                literal_selector,
                ..
            } if literal_selector == "/repo/a"
        )));
    }
}
