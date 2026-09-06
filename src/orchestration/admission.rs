//! Durable, fail-closed deduplication of ordinary bro dispatch requests.
//!
//! A claim permits one invocation attempt, not exactly-once worker execution.
//! If the daemon or caller disappears after the claim, retries inspect the same
//! identity and never launch another worker. Records have no automatic expiry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bbox_corpus_core::json_store::NofollowDirectory;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_RECORD_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operation {
    Exec,
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub task_id: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Attempt {
    pub identity: Identity,
    spawn_started: Arc<AtomicBool>,
}

impl Attempt {
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            spawn_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn mark_spawn_started(&self) {
        self.spawn_started.store(true, Ordering::SeqCst);
    }

    pub fn spawn_started(&self) -> bool {
        self.spawn_started.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Outcome {
    NotAdmitted,
    TaskRecorded {
        session_id: String,
        provider: super::providers::Provider,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    version: u8,
    operation: Operation,
    fingerprint: String,
    pub identity: Identity,
    pub outcome: Option<Outcome>,
}

pub(crate) struct Claim {
    pub record: Record,
    pub first: bool,
}

pub(crate) fn fingerprint(operation: Operation, request: Value, authority: Value) -> String {
    // Sort recursively: nested JSON maps must not make key order significant.
    fn canonical(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let ordered: std::collections::BTreeMap<_, _> = map.into_iter().collect();
                Value::Object(
                    ordered
                        .into_iter()
                        .map(|(key, value)| (key, canonical(value)))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
            other => other,
        }
    }
    digest(
        &serde_json::to_vec(&canonical(serde_json::json!({
            "operation": operation, "request": request, "authority": authority,
        })))
        .expect("JSON value serializes"),
    )
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn key_filename(key: &str) -> Result<String, String> {
    if key.is_empty() || key.len() > 256 || key.trim() != key || key.chars().any(char::is_control) {
        return Err("error.bad_request_key: request_key must be 1-256 UTF-8 bytes without control characters or surrounding whitespace".into());
    }
    Ok(format!("{}.json", digest(key.as_bytes())))
}

fn journal_dir(store_dir: &Path) -> anyhow::Result<NofollowDirectory> {
    NofollowDirectory::open_or_create(&store_dir.join("dispatch-admissions"))
}

fn read_record(dir: &NofollowDirectory, filename: &str) -> anyhow::Result<Option<Record>> {
    let Some(bytes) = dir.read_regular(filename, MAX_RECORD_BYTES, "dispatch admission")? else {
        return Ok(None);
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(record.version == 1, "unsupported admission version");
    anyhow::ensure!(
        uuid::Uuid::parse_str(&record.identity.task_id).is_ok(),
        "invalid admission identity"
    );
    anyhow::ensure!(
        record.fingerprint.len() == 64 && record.fingerprint.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid admission fingerprint"
    );
    Ok(Some(record))
}

pub(crate) async fn claim(
    store_dir: PathBuf,
    operation: Operation,
    key: String,
    fingerprint: String,
) -> Result<Claim, String> {
    let filename = key_filename(&key)?;
    tokio::task::spawn_blocking(move || {
        let result = (|| -> anyhow::Result<Claim> {
            let dir = journal_dir(&store_dir)?;
            dir.lock_exclusive()?;
            if let Some(record) = read_record(&dir, &filename)? {
                if record.operation != operation || record.fingerprint != fingerprint {
                    return Ok(Claim { record, first: false });
                }
                return Ok(Claim { record, first: false });
            }
            let record = Record {
                version: 1,
                operation,
                fingerprint: fingerprint.clone(),
                identity: Identity {
                    task_id: uuid::Uuid::new_v4().to_string(),
                    session_id: (operation == Operation::Exec).then(|| uuid::Uuid::new_v4().to_string()),
                },
                outcome: None,
            };
            dir.atomic_replace(&filename, &serde_json::to_vec(&record)?)?;
            dir.ensure_still_current()?;
            Ok(Claim { record, first: true })
        })();
        match result {
            Ok(claim) if claim.record.operation != operation || claim.record.fingerprint != fingerprint => {
                Err("error.request_key_conflict: request_key already belongs to a different operation, request, or bound workspace; it cannot be reused".into())
            }
            Ok(claim) => Ok(claim),
            Err(error) => {
                tracing::error!(%error, "dispatch admission journal unavailable; no new attempt authorized");
                Err("error.admission_unavailable: durable admission could not be verified; no new attempt authorized. Retry only with the same request_key and inputs".into())
            }
        }
    }).await.map_err(|_| "error.admission_unavailable: admission worker stopped; retry only with the same request_key and inputs".to_string())?
}

pub(crate) async fn finish(
    store_dir: PathBuf,
    key: String,
    mut record: Record,
    outcome: Outcome,
) -> Result<Record, String> {
    let filename = key_filename(&key)?;
    tokio::task::spawn_blocking(move || {
        let result = (|| -> anyhow::Result<Record> {
            let dir = journal_dir(&store_dir)?;
            dir.lock_exclusive()?;
            let current = read_record(&dir, &filename)?.ok_or_else(|| anyhow::anyhow!("admission disappeared"))?;
            anyhow::ensure!(current.identity.task_id == record.identity.task_id && current.fingerprint == record.fingerprint, "admission changed");
            record.outcome = Some(outcome);
            dir.atomic_replace(&filename, &serde_json::to_vec(&record)?)?;
            dir.ensure_still_current()?;
            Ok(record)
        })();
        result.map_err(|error| {
            tracing::error!(%error, "dispatch admission receipt could not be persisted");
            "error.admission_unconfirmed: dispatch outcome could not be durably recorded; retry only with the same request_key and inputs".to_string()
        })
    }).await.map_err(|_| "error.admission_unconfirmed: receipt worker stopped; retry only with the same request_key and inputs".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn admission_concurrent_retries_claim_once_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let digest = fingerprint(
            Operation::Exec,
            json!({"prompt":"private prompt"}),
            json!({"workspace":"one"}),
        );
        let mut calls = Vec::new();
        for _ in 0..8 {
            let root = root.clone();
            let digest = digest.clone();
            calls.push(tokio::spawn(async move {
                claim(root, Operation::Exec, "retry-key".into(), digest)
                    .await
                    .unwrap()
            }));
        }
        let mut first = 0;
        let mut identity = None;
        for call in calls {
            let result = call.await.unwrap();
            first += usize::from(result.first);
            if let Some(ref id) = identity {
                assert_eq!(&result.record.identity.task_id, id);
            }
            identity = Some(result.record.identity.task_id);
        }
        assert_eq!(first, 1);
        let replay = claim(
            root.clone(),
            Operation::Exec,
            "retry-key".into(),
            digest.clone(),
        )
        .await
        .unwrap();
        assert!(!replay.first);
        assert!(replay.record.outcome.is_none());
        let bytes = std::fs::read(
            root.join("dispatch-admissions")
                .join(key_filename("retry-key").unwrap()),
        )
        .unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("private prompt"));
        finish(
            root.clone(),
            "retry-key".into(),
            replay.record,
            Outcome::NotAdmitted,
        )
        .await
        .unwrap();
        let replay = claim(root.clone(), Operation::Exec, "retry-key".into(), digest)
            .await
            .unwrap();
        assert!(matches!(replay.record.outcome, Some(Outcome::NotAdmitted)));
        let changed = claim(root, Operation::Exec, "retry-key".into(), "f".repeat(64)).await;
        assert!(changed.err().unwrap().contains("request_key_conflict"));
    }

    #[tokio::test]
    async fn admission_corruption_and_invalid_keys_never_authorize_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(
            claim(root.clone(), Operation::Resume, " ".into(), "a".repeat(64))
                .await
                .is_err()
        );
        assert!(!root.join("dispatch-admissions").exists());
        claim(
            root.clone(),
            Operation::Resume,
            "corrupt".into(),
            "a".repeat(64),
        )
        .await
        .unwrap();
        let record = root
            .join("dispatch-admissions")
            .join(key_filename("corrupt").unwrap());
        std::fs::write(&record, b"partial").unwrap();
        assert!(
            claim(root, Operation::Resume, "corrupt".into(), "a".repeat(64))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(record).unwrap(), b"partial");
    }
}
