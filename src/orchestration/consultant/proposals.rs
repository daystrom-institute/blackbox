use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use fs2::FileExt;
use serde::Serialize;
use uuid::Uuid;

use super::types::{ConsultantId, ConsultantProposal, ProposalEvent, ProposalState, now_rfc3339};

#[derive(Debug)]
pub enum ProposalStoreError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    InvalidInstance(String),
    InvalidProposalId(String),
    NotFound {
        instance_id: ConsultantId,
        proposal_id: String,
    },
    Conflict {
        proposal_id: String,
        expected: ProposalState,
        actual: ProposalState,
    },
    InvalidTransition {
        proposal_id: String,
        from: ProposalState,
        to: ProposalState,
    },
    IdempotencyConflict {
        key: String,
        existing_id: String,
    },
}

impl std::fmt::Display for ProposalStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Serde(e) => write!(f, "json error: {e}"),
            Self::InvalidInstance(id) => write!(f, "invalid consultant instance id: {id}"),
            Self::InvalidProposalId(id) => write!(f, "invalid proposal id: {id}"),
            Self::NotFound {
                instance_id,
                proposal_id,
            } => write!(f, "proposal {proposal_id} not found for {instance_id}"),
            Self::Conflict {
                proposal_id,
                expected,
                actual,
            } => write!(
                f,
                "proposal {proposal_id} state conflict: expected {expected:?}, found {actual:?}"
            ),
            Self::InvalidTransition {
                proposal_id,
                from,
                to,
            } => write!(
                f,
                "invalid proposal {proposal_id} transition: {from:?} -> {to:?}"
            ),
            Self::IdempotencyConflict { key, existing_id } => write!(
                f,
                "idempotency key {key} already belongs to incompatible proposal {existing_id}"
            ),
        }
    }
}

impl std::error::Error for ProposalStoreError {}

impl From<std::io::Error> for ProposalStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ProposalStoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

pub type Result<T> = std::result::Result<T, ProposalStoreError>;

/// Read-only MCP projection options, shared by consumer-neutral and pinned adapters.
#[derive(Debug, Default)]
pub struct ProposalReadOptions {
    pub since: Option<String>,
    pub only_pending: bool,
    pub limit: Option<usize>,
    pub after: Option<String>,
    pub through: Option<String>,
    pub proposal_id: Option<String>,
    pub include_events: bool,
}

impl ProposalReadOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.proposal_id.is_some()
            && (self.since.is_some()
                || self.only_pending
                || self.after.is_some()
                || self.through.is_some()
                || self.limit.is_some())
        {
            anyhow::bail!(
                "error.proposal_read_options: proposal_id cannot be combined with list filters, limit, or cursors"
            );
        }
        if self.after.is_some() && self.through.is_none() {
            anyhow::bail!(
                "error.proposal_cursor_invalid: after requires the through bound from the initial page"
            );
        }
        if self.include_events && self.proposal_id.is_none() {
            anyhow::bail!("error.proposal_read_options: include_events requires proposal_id");
        }
        Ok(())
    }
}

pub struct ProposalStore {
    root: PathBuf,
}

impl ProposalStore {
    /// Open a proposal store rooted at `root`. The caller owns path layout;
    /// the daemon passes the legacy `state_dir/badgey/proposals` for the
    /// Badgey consumer so on-disk state is unchanged by the extraction.
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(
        &self,
        instance_id: &ConsultantId,
        kind: &str,
        draft: serde_json::Value,
        idempotency_key: Option<String>,
    ) -> Result<ConsultantProposal> {
        let dir = self.instance_dir(instance_id)?;
        fs::create_dir_all(&dir)?;
        let _lock = lock_path(&dir.join(".create.lock"))?;

        if let Some(key) = idempotency_key.as_deref() {
            if let Some(existing) = self
                .list_by_instance(instance_id)?
                .into_iter()
                .find(|proposal| proposal.idempotency_key.as_deref() == Some(key))
            {
                if existing.kind != kind || existing.draft != draft {
                    return Err(ProposalStoreError::IdempotencyConflict {
                        key: key.to_string(),
                        existing_id: existing.id,
                    });
                }
                return Ok(existing);
            }
        }

        let id = next_proposal_id(&dir)?;
        let proposal = ConsultantProposal::new(
            id.clone(),
            instance_id.clone(),
            kind.to_string(),
            draft,
            idempotency_key,
        );
        atomic_write_json(&proposal_path(&dir, &id)?, &proposal)?;
        Ok(proposal)
    }

    pub fn get(
        &self,
        instance_id: &ConsultantId,
        proposal_id: &str,
    ) -> Result<Option<ConsultantProposal>> {
        let path = self.proposal_path(instance_id, proposal_id)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }

    pub fn transition(
        &self,
        instance_id: &ConsultantId,
        proposal_id: &str,
        from: ProposalState,
        to: ProposalState,
        note: Option<String>,
    ) -> Result<ConsultantProposal> {
        let path = self.proposal_path(instance_id, proposal_id)?;
        if !path.exists() {
            return Err(ProposalStoreError::NotFound {
                instance_id: instance_id.clone(),
                proposal_id: proposal_id.to_string(),
            });
        }
        let _lock = lock_path(&path.with_extension("lock"))?;
        let mut proposal: ConsultantProposal = read_json(&path)?;
        if proposal.state != from {
            return Err(ProposalStoreError::Conflict {
                proposal_id: proposal_id.to_string(),
                expected: from,
                actual: proposal.state,
            });
        }
        if !from.can_transition_to(to) {
            return Err(ProposalStoreError::InvalidTransition {
                proposal_id: proposal_id.to_string(),
                from,
                to,
            });
        }
        let now = now_rfc3339();
        proposal.state = to;
        proposal.updated_at = now.clone();
        proposal.events.push(ProposalEvent {
            at: now,
            from,
            to,
            note,
        });
        atomic_write_json(&path, &proposal)?;
        Ok(proposal)
    }

    pub fn set_applied_task_id(
        &self,
        instance_id: &ConsultantId,
        proposal_id: &str,
        task_id: String,
    ) -> Result<ConsultantProposal> {
        let path = self.proposal_path(instance_id, proposal_id)?;
        if !path.exists() {
            return Err(ProposalStoreError::NotFound {
                instance_id: instance_id.clone(),
                proposal_id: proposal_id.to_string(),
            });
        }
        let _lock = lock_path(&path.with_extension("lock"))?;
        let mut proposal: ConsultantProposal = read_json(&path)?;
        if proposal.state != ProposalState::Applying {
            return Err(ProposalStoreError::Conflict {
                proposal_id: proposal_id.to_string(),
                expected: ProposalState::Applying,
                actual: proposal.state,
            });
        }
        proposal.applied_task_id = Some(task_id);
        proposal.updated_at = now_rfc3339();
        atomic_write_json(&path, &proposal)?;
        Ok(proposal)
    }

    pub fn list_by_instance(&self, instance_id: &ConsultantId) -> Result<Vec<ConsultantProposal>> {
        let dir = self.instance_dir(instance_id)?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut proposals = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            proposals.push(read_json::<ConsultantProposal>(&path)?);
        }
        proposals.sort_by_key(|p| proposal_number(&p.id).unwrap_or(usize::MAX));
        Ok(proposals)
    }

    /// Exact source-owned proposal projection, before transport paging.
    pub fn exact_response_row(
        &self,
        instance: &ConsultantId,
        options: &ProposalReadOptions,
    ) -> anyhow::Result<serde_json::Value> {
        use serde_json::json;
        options.validate()?;
        let id = options
            .proposal_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("proposal_id is required for an exact read"))?;
        let proposal = self
            .get(instance, id)?
            .ok_or_else(|| anyhow::anyhow!("error.proposal_not_found: {id}"))?;
        let mut row = json!({"id": proposal.id, "kind": proposal.kind, "state": proposal.state,
                "draft": proposal.draft, "created_at": proposal.created_at, "updated_at": proposal.updated_at});
        if let Some(task) = proposal.applied_task_id {
            row["applied_task_id"] = json!(task);
        }
        if options.include_events {
            row["events"] = json!(proposal.events);
        }
        Ok(row)
    }

    /// Stable numeric-id continuation survives state changes in earlier proposals.
    /// `through` freezes the initial upper id bound, so new proposals do not extend
    /// a workflow's current synthesis window indefinitely.
    pub fn response_page(
        &self,
        instance: &ConsultantId,
        options: &ProposalReadOptions,
        mut envelope: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        use serde_json::json;
        options.validate()?;
        let (rows, through, limit, total) = if let Some(id) = options.proposal_id.as_deref() {
            let row = self.exact_response_row(instance, options)?;
            (vec![row], id.to_owned(), 1, 1)
        } else {
            if let Some(since) = options.since.as_deref() {
                chrono::DateTime::parse_from_rfc3339(since).map_err(|_| {
                    anyhow::anyhow!(
                        "error.proposal_since_invalid: since must be an RFC3339 timestamp"
                    )
                })?;
            }
            let proposals = self.list_by_instance(instance)?;
            let parse_cursor = |cursor: &str| {
                proposal_number(cursor).ok_or_else(|| {
                    anyhow::anyhow!("error.proposal_cursor_invalid: expected P-<number>")
                })
            };
            let after = options
                .after
                .as_deref()
                .map(parse_cursor)
                .transpose()?
                .unwrap_or(0);
            let ceiling = options
                .through
                .as_deref()
                .map(parse_cursor)
                .transpose()?
                .unwrap_or_else(|| {
                    proposals
                        .iter()
                        .filter_map(|proposal| proposal_number(&proposal.id))
                        .max()
                        .unwrap_or(0)
                });
            anyhow::ensure!(
                after <= ceiling,
                "error.proposal_cursor_invalid: after must not exceed through"
            );
            let since = options
                .since
                .as_deref()
                .map(chrono::DateTime::parse_from_rfc3339)
                .transpose()?;
            let filtered: Vec<_> = proposals
                .into_iter()
                .filter(|proposal| {
                    let number = proposal_number(&proposal.id).unwrap_or(0);
                    number > after
                        && number <= ceiling
                        && (!options.only_pending || !proposal.is_terminal())
                })
                .filter(|proposal| {
                    since.as_ref().is_none_or(|since| {
                        chrono::DateTime::parse_from_rfc3339(&proposal.created_at)
                            .is_ok_and(|created| created >= *since)
                    })
                })
                .collect();
            let total = filtered.len();
            let limit = options.limit.unwrap_or(20).clamp(1, 100);
            let rows = filtered.into_iter().take(limit).map(|proposal| {
                let mut row = json!({"id": proposal.id, "kind": proposal.kind, "state": proposal.state,
                    "created_at": proposal.created_at});
                if let Some(headline) = proposal.draft.get("headline").and_then(serde_json::Value::as_str) {
                    row["headline"] = json!(headline);
                    bbox_corpus_core::response_page::preview_field(&mut row, "headline", 512);
                }
                if let Some(task) = proposal.applied_task_id { row["applied_task_id"] = json!(task); }
                row
            }).collect();
            (rows, format!("P-{ceiling}"), limit, total)
        };
        envelope["proposals"] = json!(rows);
        envelope["total"] = json!(total);
        envelope["offset"] = json!(0);
        envelope["limit"] = json!(limit);
        envelope["count"] = json!(rows.len());
        envelope["through"] = json!(through);
        envelope["next_after"] = json!(through); // reserve cursor bytes before byte selection
        envelope["has_more"] = json!(false);
        let mut page = bbox_corpus_core::response_page::bound_page(envelope, "proposals")?;
        let has_more = !page["next_offset"].is_null();
        page["has_more"] = json!(has_more);
        page["next_after"] = if has_more {
            page["proposals"]
                .as_array()
                .and_then(|rows| rows.last())
                .map(|row| row["id"].clone())
                .unwrap_or_default()
        } else {
            serde_json::Value::Null
        };
        page.as_object_mut().unwrap().remove("offset");
        page.as_object_mut().unwrap().remove("next_offset");
        Ok(page)
    }

    pub fn list_non_terminal(&self) -> Result<Vec<ConsultantProposal>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut proposals = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let instance_id = match ConsultantId::from_str(&entry.file_name().to_string_lossy()) {
                Ok(id) => id,
                Err(_) => continue,
            };
            proposals.extend(
                self.list_by_instance(&instance_id)?
                    .into_iter()
                    .filter(|proposal| !proposal.is_terminal()),
            );
        }
        Ok(proposals)
    }

    fn instance_dir(&self, instance_id: &ConsultantId) -> Result<PathBuf> {
        if ConsultantId::from_str(instance_id.as_str()).is_err() {
            return Err(ProposalStoreError::InvalidInstance(instance_id.to_string()));
        }
        Ok(self.root.join(instance_id.as_str()))
    }

    fn proposal_path(&self, instance_id: &ConsultantId, proposal_id: &str) -> Result<PathBuf> {
        proposal_path(&self.instance_dir(instance_id)?, proposal_id)
    }
}

fn proposal_path(dir: &Path, proposal_id: &str) -> Result<PathBuf> {
    if proposal_number(proposal_id).is_none() {
        return Err(ProposalStoreError::InvalidProposalId(
            proposal_id.to_string(),
        ));
    }
    Ok(dir.join(format!("{proposal_id}.json")))
}

fn proposal_number(proposal_id: &str) -> Option<usize> {
    proposal_id.strip_prefix("P-")?.parse().ok()
}

fn next_proposal_id(dir: &Path) -> Result<String> {
    let mut max_id = 0usize;
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                max_id = max_id.max(proposal_number(stem).unwrap_or(0));
            }
        }
    }
    Ok(format!("P-{}", max_id + 1))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let raw = serde_json::to_string_pretty(value)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("proposal"),
        Uuid::new_v4()
    ));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn lock_path(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    fn instance() -> ConsultantId {
        ConsultantId::from_str("bg-3f7a91c4-91ff04cc").unwrap()
    }

    #[test]
    fn proposal_pages_keep_tails_when_states_change_and_new_records_arrive() {
        use serde_json::json;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let store = ProposalStore::new(root).unwrap();
        let id = instance();
        for _ in 0..45 {
            store.create(&id, "packet", json!({"headline": "界".repeat(300), "private_body": "large-draft-secret".repeat(3000)}), None).unwrap();
        }
        let first = store
            .response_page(
                &id,
                &ProposalReadOptions {
                    only_pending: true,
                    ..Default::default()
                },
                json!({}),
            )
            .unwrap();
        assert_eq!(first["count"], 20);
        assert_eq!(first["through"], "P-45");
        assert_eq!(first["next_after"], "P-20");
        assert_eq!(first["proposals"][0]["headline_truncated"], true);
        assert!(!first.to_string().contains("large-draft-secret"));
        for n in 1..=20 {
            store
                .transition(
                    &id,
                    &format!("P-{n}"),
                    ProposalState::Pending,
                    ProposalState::Failed,
                    None,
                )
                .unwrap();
        }
        store
            .create(&id, "packet", json!({"headline": "new arrival"}), None)
            .unwrap();
        let mut options = ProposalReadOptions {
            only_pending: true,
            after: Some("P-20".into()),
            through: Some("P-45".into()),
            ..Default::default()
        };
        let second = store.response_page(&id, &options, json!({})).unwrap();
        assert_eq!(second["proposals"][0]["id"], "P-21");
        assert_eq!(second["next_after"], "P-40");
        options.after = Some("P-40".into());
        let last = store.response_page(&id, &options, json!({})).unwrap();
        assert_eq!(last["count"], 5);
        assert_eq!(last["proposals"][4]["id"], "P-45");
        assert_eq!(last["has_more"], false);
        assert!(last["next_after"].is_null());
        for page in [first, second, last] {
            assert!(
                serde_json::to_vec(&page).unwrap().len()
                    <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
            );
        }
    }

    #[test]
    fn proposal_exact_read_expands_draft_and_opt_in_history_without_mutation() {
        use serde_json::json;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let store = ProposalStore::new(root).unwrap();
        let id = instance();
        store
            .create(
                &id,
                "packet",
                json!({"headline": "example", "body": "exact draft"}),
                None,
            )
            .unwrap();
        store
            .transition(
                &id,
                "P-1",
                ProposalState::Pending,
                ProposalState::Failed,
                Some("history-only".into()),
            )
            .unwrap();
        let mut options = ProposalReadOptions {
            proposal_id: Some("P-1".into()),
            ..Default::default()
        };
        let compact = store.response_page(&id, &options, json!({})).unwrap();
        assert_eq!(compact["proposals"][0]["draft"]["body"], "exact draft");
        assert!(compact["proposals"][0].get("events").is_none());
        options.include_events = true;
        let expanded = store.response_page(&id, &options, json!({})).unwrap();
        assert_eq!(
            expanded["proposals"][0]["events"][0]["note"],
            "history-only"
        );
        assert_eq!(
            store.get(&id, "P-1").unwrap().unwrap().state,
            ProposalState::Failed
        );
        options.after = Some("P-0".into());
        assert!(store.response_page(&id, &options, json!({})).is_err());
        assert!(
            store
                .response_page(
                    &id,
                    &ProposalReadOptions {
                        after: Some("P-0".into()),
                        ..Default::default()
                    },
                    json!({})
                )
                .is_err()
        );
        assert!(
            store
                .response_page(
                    &id,
                    &ProposalReadOptions {
                        since: Some("not-a-timestamp".into()),
                        ..Default::default()
                    },
                    json!({})
                )
                .is_err()
        );
    }

    #[test]
    fn proposals_are_isolated_per_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProposalStore::new(tmp.path().to_path_buf()).unwrap();
        let a = instance();
        let b = ConsultantId::from_str("bg-11111111-22222222").unwrap();
        let pa = store
            .create(&a, "packet", serde_json::json!({"a": 1}), None)
            .unwrap();
        let pb = store
            .create(&b, "packet", serde_json::json!({"b": 1}), None)
            .unwrap();
        assert_eq!(pa.id, "P-1");
        assert_eq!(pb.id, "P-1");
    }

    #[test]
    fn create_is_idempotent_by_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProposalStore::new(tmp.path().to_path_buf()).unwrap();
        let id = instance();
        let first = store
            .create(
                &id,
                "agent",
                serde_json::json!({"name": "a"}),
                Some("idem".to_string()),
            )
            .unwrap();
        let second = store
            .create(
                &id,
                "agent",
                serde_json::json!({"name": "a"}),
                Some("idem".to_string()),
            )
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.draft, serde_json::json!({"name": "a"}));
    }

    #[test]
    fn create_rejects_idempotency_key_reuse_with_different_draft() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProposalStore::new(tmp.path().to_path_buf()).unwrap();
        let id = instance();
        store
            .create(
                &id,
                "agent",
                serde_json::json!({"name": "a"}),
                Some("idem".to_string()),
            )
            .unwrap();
        let err = store
            .create(
                &id,
                "agent",
                serde_json::json!({"name": "b"}),
                Some("idem".to_string()),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ProposalStoreError::IdempotencyConflict { .. }
        ));
    }

    #[test]
    fn orphan_tempfile_does_not_change_visible_proposal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProposalStore::new(tmp.path().to_path_buf()).unwrap();
        let id = instance();
        store
            .create(&id, "packet", serde_json::json!({"v": 1}), None)
            .unwrap();
        let dir = store.root().join(id.as_str());
        fs::write(
            dir.join(".P-1.json.tmp-deadbeef"),
            r#"{"id":"P-1","state":"applied"}"#,
        )
        .unwrap();
        let proposal = store.get(&id, "P-1").unwrap().unwrap();
        assert_eq!(proposal.state, ProposalState::Pending);
        assert_eq!(proposal.draft, serde_json::json!({"v": 1}));
    }

    #[test]
    fn concurrent_cas_has_single_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(ProposalStore::new(tmp.path().to_path_buf()).unwrap());
        let id = instance();
        store
            .create(&id, "brofile", serde_json::json!({}), None)
            .unwrap();

        let a_store = store.clone();
        let a_id = id.clone();
        let first = thread::spawn(move || {
            a_store.transition(
                &a_id,
                "P-1",
                ProposalState::Pending,
                ProposalState::Applying,
                None,
            )
        });
        let b_store = store.clone();
        let second = thread::spawn(move || {
            b_store.transition(
                &id,
                "P-1",
                ProposalState::Pending,
                ProposalState::Applying,
                None,
            )
        });

        let wins = [
            first.join().unwrap().is_ok(),
            second.join().unwrap().is_ok(),
        ]
        .into_iter()
        .filter(|won| *won)
        .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn non_terminal_scan_excludes_applied_and_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProposalStore::new(tmp.path().to_path_buf()).unwrap();
        let id = instance();
        store
            .create(&id, "packet", serde_json::json!({}), None)
            .unwrap();
        store
            .create(&id, "agent", serde_json::json!({}), None)
            .unwrap();
        store
            .transition(
                &id,
                "P-2",
                ProposalState::Pending,
                ProposalState::Failed,
                Some("rejected".to_string()),
            )
            .unwrap();
        assert_eq!(store.list_non_terminal().unwrap().len(), 1);
    }
}
