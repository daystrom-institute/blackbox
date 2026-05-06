use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::Serialize;
use uuid::Uuid;

use super::types::{
    now_rfc3339, ActionId, ActionJournalEntry, ActionJournalEvent, ActionJournalState,
};

#[derive(Debug)]
pub enum JournalError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    NotFound {
        action_id: ActionId,
    },
    Conflict {
        action_id: ActionId,
        expected: ActionJournalState,
        actual: ActionJournalState,
    },
    InvalidTransition {
        action_id: ActionId,
        from: ActionJournalState,
        to: ActionJournalState,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Serde(e) => write!(f, "json error: {e}"),
            Self::NotFound { action_id } => {
                write!(f, "action journal entry not found: {action_id}")
            }
            Self::Conflict {
                action_id,
                expected,
                actual,
            } => write!(
                f,
                "action {action_id} state conflict: expected {expected:?}, found {actual:?}"
            ),
            Self::InvalidTransition {
                action_id,
                from,
                to,
            } => write!(
                f,
                "invalid action {action_id} transition: {from:?} -> {to:?}"
            ),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<std::io::Error> for JournalError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for JournalError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

pub type Result<T> = std::result::Result<T, JournalError>;

pub struct ActionJournal {
    root: PathBuf,
}

impl ActionJournal {
    pub fn new(state_dir: PathBuf) -> Result<Self> {
        let root = state_dir.join("badgey").join("action_journal");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn record_seen(
        &self,
        action_id: ActionId,
        action_kind: String,
        body: serde_json::Value,
    ) -> Result<ActionJournalEntry> {
        let path = self.action_path(&action_id);
        let _lock = lock_path(&path.with_extension("lock"))?;
        if path.exists() {
            return read_json(&path);
        }
        let entry = ActionJournalEntry::new(action_id, action_kind, body);
        atomic_write_json(&path, &entry)?;
        Ok(entry)
    }

    pub fn get(&self, action_id: &ActionId) -> Result<Option<ActionJournalEntry>> {
        let path = self.action_path(action_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }

    pub fn transition(
        &self,
        action_id: &ActionId,
        from: ActionJournalState,
        to: ActionJournalState,
        note: Option<String>,
    ) -> Result<ActionJournalEntry> {
        let path = self.action_path(action_id);
        if !path.exists() {
            return Err(JournalError::NotFound {
                action_id: action_id.clone(),
            });
        }
        let _lock = lock_path(&path.with_extension("lock"))?;
        let mut entry: ActionJournalEntry = read_json(&path)?;
        if entry.state != from {
            return Err(JournalError::Conflict {
                action_id: action_id.clone(),
                expected: from,
                actual: entry.state,
            });
        }
        if !from.can_transition_to(&to) {
            return Err(JournalError::InvalidTransition {
                action_id: action_id.clone(),
                from,
                to,
            });
        }
        let now = now_rfc3339();
        entry.state = to.clone();
        entry.updated_at = now.clone();
        entry.events.push(ActionJournalEvent {
            at: now,
            from,
            to,
            note,
        });
        atomic_write_json(&path, &entry)?;
        Ok(entry)
    }

    pub fn list_non_terminal(&self) -> Result<Vec<ActionJournalEntry>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let journal_entry: ActionJournalEntry = read_json(&path)?;
            if !journal_entry.state.is_terminal() {
                entries.push(journal_entry);
            }
        }
        entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(entries)
    }

    pub fn archive_expired(&self, older_than: DateTime<Utc>) -> Result<usize> {
        let archive_dir = self.root.join("_archive");
        fs::create_dir_all(&archive_dir)?;
        let mut archived = 0usize;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let journal_entry: ActionJournalEntry = read_json(&path)?;
            if !journal_entry.state.is_terminal() {
                continue;
            }
            let updated_at = DateTime::parse_from_rfc3339(&journal_entry.updated_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            if updated_at >= older_than {
                continue;
            }
            let dest = archive_dir.join(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("entry.json"),
            );
            fs::rename(&path, dest)?;
            let lock_path = path.with_extension("lock");
            if lock_path.exists() {
                let lock_dest = archive_dir.join(
                    lock_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("entry.lock"),
                );
                let _ = fs::rename(lock_path, lock_dest);
            }
            archived += 1;
        }
        Ok(archived)
    }

    fn action_path(&self, action_id: &ActionId) -> PathBuf {
        self.root.join(format!("{action_id}.json"))
    }
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
            .unwrap_or("journal"),
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
    use super::*;

    #[test]
    fn record_seen_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = ActionJournal::new(tmp.path().to_path_buf()).unwrap();
        let action_id = ActionId::new_v4();
        let first = journal
            .record_seen(
                action_id.clone(),
                "bg-action-emit-proposal".to_string(),
                serde_json::json!({"n": 1}),
            )
            .unwrap();
        let second = journal
            .record_seen(
                action_id,
                "bg-action-emit-proposal".to_string(),
                serde_json::json!({"n": 2}),
            )
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(second.body, serde_json::json!({"n": 1}));
    }

    #[test]
    fn transitions_follow_state_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = ActionJournal::new(tmp.path().to_path_buf()).unwrap();
        let action_id = ActionId::new_v4();
        journal
            .record_seen(
                action_id.clone(),
                "spawn".to_string(),
                serde_json::json!({}),
            )
            .unwrap();
        journal
            .transition(
                &action_id,
                ActionJournalState::Seen,
                ActionJournalState::Dispatching {
                    task_id: "task-1".to_string(),
                },
                None,
            )
            .unwrap();
        let invalid = journal.transition(
            &action_id,
            ActionJournalState::Dispatching {
                task_id: "task-1".to_string(),
            },
            ActionJournalState::Seen,
            None,
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn seen_can_complete_without_dispatching_for_local_actions() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = ActionJournal::new(tmp.path().to_path_buf()).unwrap();
        let action_id = ActionId::new_v4();
        journal
            .record_seen(
                action_id.clone(),
                "bg-action-emit-proposal".to_string(),
                serde_json::json!({}),
            )
            .unwrap();
        let completed = journal
            .transition(
                &action_id,
                ActionJournalState::Seen,
                ActionJournalState::Completed {
                    result_ref: "proposal:bg-3f7a91c4-91ff04cc:P-1".to_string(),
                },
                None,
            )
            .unwrap();
        assert!(completed.state.is_terminal());
    }

    #[test]
    fn orphan_tempfile_does_not_change_visible_journal_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = ActionJournal::new(tmp.path().to_path_buf()).unwrap();
        let action_id = ActionId::new_v4();
        journal
            .record_seen(
                action_id.clone(),
                "active".to_string(),
                serde_json::json!({"v": 1}),
            )
            .unwrap();
        fs::write(
            journal
                .root()
                .join(format!(".{}.json.tmp-deadbeef", action_id)),
            r#"{"state":{"completed":{"result_ref":"bad"}}}"#,
        )
        .unwrap();
        let entry = journal.get(&action_id).unwrap().unwrap();
        assert_eq!(entry.state, ActionJournalState::Seen);
        assert_eq!(entry.body, serde_json::json!({"v": 1}));
    }

    #[test]
    fn archive_keeps_non_terminal_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = ActionJournal::new(tmp.path().to_path_buf()).unwrap();
        let active = ActionId::new_v4();
        let done = ActionId::new_v4();
        journal
            .record_seen(active.clone(), "active".to_string(), serde_json::json!({}))
            .unwrap();
        journal
            .record_seen(done.clone(), "done".to_string(), serde_json::json!({}))
            .unwrap();
        journal
            .transition(
                &done,
                ActionJournalState::Seen,
                ActionJournalState::Failed {
                    reason: "test".to_string(),
                },
                None,
            )
            .unwrap();
        let archived = journal
            .archive_expired(Utc::now() + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(archived, 1);
        assert!(journal.get(&active).unwrap().is_some());
        assert!(journal.get(&done).unwrap().is_none());
    }
}
