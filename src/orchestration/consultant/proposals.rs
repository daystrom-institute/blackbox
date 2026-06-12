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

    pub fn get(&self, instance_id: &ConsultantId, proposal_id: &str) -> Result<Option<ConsultantProposal>> {
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
