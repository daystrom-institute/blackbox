//! Durable command identity and outcome journal.
//!
//! A command is keyed by both fleet sequence and stable command id. Its
//! serialized payload digest is persisted before application, then its outcome
//! is appended and synced before transmission. Replays with identical identity
//! return the recorded outcome. Any sequence, id, or digest mismatch fails
//! closed instead of applying a second effect.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use bro_core::CommandId;
use bro_protocol::{CommandOutcome, WorkerCommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const JOURNAL_VERSION: u16 = 1;
const INDETERMINATE_OUTCOME_CODE: &str = "worker.command_indeterminate";

#[derive(Debug, Clone, PartialEq)]
pub enum CommandDisposition {
    Apply,
    Duplicate(CommandOutcome),
}

#[derive(Debug)]
pub enum CommandJournalError {
    Io(std::io::Error),
    Json(serde_json::Error),
    SequenceGap { expected: u64, actual: u64 },
    IdentityConflict { command_seq: u64, message: String },
    Indeterminate { command_seq: u64 },
    MissingPreparation { command_seq: u64 },
}

impl std::fmt::Display for CommandJournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "command journal I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "command journal JSON failed: {error}"),
            Self::SequenceGap { expected, actual } => {
                write!(
                    formatter,
                    "command sequence gap: expected {expected}, got {actual}"
                )
            }
            Self::IdentityConflict {
                command_seq,
                message,
            } => write!(
                formatter,
                "command identity conflict at sequence {command_seq}: {message}"
            ),
            Self::Indeterminate { command_seq } => write!(
                formatter,
                "command {command_seq} was prepared before restart without a durable outcome"
            ),
            Self::MissingPreparation { command_seq } => {
                write!(
                    formatter,
                    "command {command_seq} has no durable preparation"
                )
            }
        }
    }
}

impl std::error::Error for CommandJournalError {}

impl From<std::io::Error> for CommandJournalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CommandJournalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum JournalEntry {
    Header {
        version: u16,
    },
    Prepared {
        command_seq: u64,
        command_id: CommandId,
        payload_digest: String,
    },
    Outcome {
        outcome: CommandOutcome,
    },
    Ack {
        through_command_seq: u64,
    },
}

#[derive(Debug, Clone)]
struct CommandRecord {
    command_id: CommandId,
    payload_digest: String,
    outcome: Option<CommandOutcome>,
}

#[derive(Default)]
struct JournalState {
    records: BTreeMap<u64, CommandRecord>,
    ids: HashMap<String, u64>,
    outcome_ack: u64,
}

struct LoadedJournal {
    state: JournalState,
    valid_bytes: u64,
}

struct JournalInner {
    file: File,
    state: JournalState,
}

pub struct CommandJournal {
    inner: Mutex<JournalInner>,
}

impl CommandJournal {
    #[allow(clippy::disallowed_methods)]
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CommandJournalError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            create_private_directory(parent)?;
        }
        let existed = path.exists();
        if existed {
            validate_private_file(&path)?;
        }
        let loaded = if existed {
            load_state(&path)?
        } else {
            LoadedJournal {
                state: JournalState::default(),
                valid_bytes: 0,
            }
        };
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path)?;

        let file_bytes = file.metadata()?.len();
        if file_bytes > loaded.valid_bytes {
            file.set_len(loaded.valid_bytes)?;
            file.sync_all()?;
        }
        let needs_header = !existed || loaded.valid_bytes == 0;

        let journal = Self {
            inner: Mutex::new(JournalInner {
                file,
                state: loaded.state,
            }),
        };
        if needs_header {
            let mut inner = journal.lock();
            append_entry(
                &mut inner.file,
                &JournalEntry::Header {
                    version: JOURNAL_VERSION,
                },
            )?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        }
        journal.reject_indeterminate_preparations()?;
        Ok(journal)
    }

    fn reject_indeterminate_preparations(&self) -> Result<(), CommandJournalError> {
        let mut inner = self.lock();
        let indeterminate = inner
            .state
            .records
            .iter()
            .filter(|(_, record)| record.outcome.is_none())
            .map(|(command_seq, record)| CommandOutcome {
                    command_seq: *command_seq,
                    command_id: record.command_id.clone(),
                    accepted: false,
                    terminal: true,
                    result_or_error: serde_json::json!({
                        "code": INDETERMINATE_OUTCOME_CODE,
                        "message": "command was durably prepared without a durable outcome; refusing to reapply it after restart",
                    }),
                }
            )
            .collect::<Vec<_>>();

        for outcome in indeterminate {
            let command_seq = outcome.command_seq;
            append_entry(
                &mut inner.file,
                &JournalEntry::Outcome {
                    outcome: outcome.clone(),
                },
            )?;
            inner
                .state
                .records
                .get_mut(&command_seq)
                .expect("indeterminate record was collected above")
                .outcome = Some(outcome);
        }
        Ok(())
    }

    pub fn last_command_seq(&self) -> u64 {
        self.lock()
            .state
            .records
            .last_key_value()
            .map_or(0, |(seq, _)| *seq)
    }

    pub fn next_command_seq(&self) -> u64 {
        self.last_command_seq().saturating_add(1)
    }

    pub fn prepare(
        &self,
        command: &WorkerCommand,
    ) -> Result<CommandDisposition, CommandJournalError> {
        let digest = command_payload_digest(command)?;
        let mut inner = self.lock();
        if let Some(record) = inner.state.records.get(&command.command_seq) {
            verify_identity(command, &digest, record)?;
            return match &record.outcome {
                Some(outcome) => Ok(CommandDisposition::Duplicate(outcome.clone())),
                None => Err(CommandJournalError::Indeterminate {
                    command_seq: command.command_seq,
                }),
            };
        }

        let expected = inner
            .state
            .records
            .last_key_value()
            .map_or(1, |(seq, _)| seq.saturating_add(1));
        if command.command_seq != expected {
            return Err(CommandJournalError::SequenceGap {
                expected,
                actual: command.command_seq,
            });
        }
        if let Some(prior_seq) = inner.state.ids.get(command.command_id.as_str()) {
            return Err(CommandJournalError::IdentityConflict {
                command_seq: command.command_seq,
                message: format!(
                    "command id {} was already used at sequence {prior_seq}",
                    command.command_id.as_str()
                ),
            });
        }

        append_entry(
            &mut inner.file,
            &JournalEntry::Prepared {
                command_seq: command.command_seq,
                command_id: command.command_id.clone(),
                payload_digest: digest.clone(),
            },
        )?;
        inner
            .state
            .ids
            .insert(command.command_id.as_str().to_string(), command.command_seq);
        inner.state.records.insert(
            command.command_seq,
            CommandRecord {
                command_id: command.command_id.clone(),
                payload_digest: digest,
                outcome: None,
            },
        );
        Ok(CommandDisposition::Apply)
    }

    pub fn finish(&self, outcome: CommandOutcome) -> Result<(), CommandJournalError> {
        let mut inner = self.lock();
        let record = inner.state.records.get(&outcome.command_seq).ok_or(
            CommandJournalError::MissingPreparation {
                command_seq: outcome.command_seq,
            },
        )?;
        if record.command_id != outcome.command_id {
            return Err(CommandJournalError::IdentityConflict {
                command_seq: outcome.command_seq,
                message: "outcome command id differs from its preparation".to_string(),
            });
        }
        if let Some(prior) = &record.outcome {
            if prior == &outcome {
                return Ok(());
            }
            if prior.terminal || !prior.accepted || !outcome.terminal || !outcome.accepted {
                return Err(CommandJournalError::IdentityConflict {
                    command_seq: outcome.command_seq,
                    message: "a stable command outcome changed incompatibly".to_string(),
                });
            }
        }

        append_entry(
            &mut inner.file,
            &JournalEntry::Outcome {
                outcome: outcome.clone(),
            },
        )?;
        let command_seq = outcome.command_seq;
        inner
            .state
            .records
            .get_mut(&command_seq)
            .expect("record checked above")
            .outcome = Some(outcome);
        Ok(())
    }

    pub fn acknowledge_outcomes(
        &self,
        through_command_seq: u64,
    ) -> Result<(), CommandJournalError> {
        let mut inner = self.lock();
        let last = inner
            .state
            .records
            .last_key_value()
            .map_or(0, |(seq, _)| *seq);
        if through_command_seq > last {
            return Err(CommandJournalError::SequenceGap {
                expected: last,
                actual: through_command_seq,
            });
        }
        if through_command_seq <= inner.state.outcome_ack {
            return Ok(());
        }
        require_terminal_outcomes_through(
            &inner.state,
            inner.state.outcome_ack.saturating_add(1),
            through_command_seq,
        )?;
        append_entry(
            &mut inner.file,
            &JournalEntry::Ack {
                through_command_seq,
            },
        )?;
        inner.state.outcome_ack = through_command_seq;
        Ok(())
    }

    pub fn unacknowledged_outcomes(&self) -> Vec<CommandOutcome> {
        let inner = self.lock();
        inner
            .state
            .records
            .range(inner.state.outcome_ack.saturating_add(1)..)
            .filter_map(|(_, record)| record.outcome.clone())
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, JournalInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// Journal construction is a synchronous durability boundary owned by the worker actor.
#[allow(clippy::disallowed_methods)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "command journal parent is not a directory: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_private_file(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("command journal is not a regular file: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("command journal permissions are unsafe: {}", path.display()),
            ));
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn verify_identity(
    command: &WorkerCommand,
    digest: &str,
    record: &CommandRecord,
) -> Result<(), CommandJournalError> {
    if record.command_id != command.command_id {
        return Err(CommandJournalError::IdentityConflict {
            command_seq: command.command_seq,
            message: "replayed sequence carried a different command id".to_string(),
        });
    }
    if record.payload_digest != digest {
        return Err(CommandJournalError::IdentityConflict {
            command_seq: command.command_seq,
            message: "replayed command id carried a different payload digest".to_string(),
        });
    }
    Ok(())
}

pub fn command_payload_digest(command: &WorkerCommand) -> Result<String, serde_json::Error> {
    let payload = serde_json::to_vec(&command.command)?;
    let digest = Sha256::digest(payload);
    Ok(format!("sha256:{digest:x}"))
}

fn append_entry(file: &mut File, entry: &JournalEntry) -> Result<(), CommandJournalError> {
    serde_json::to_writer(&mut *file, entry)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

#[allow(clippy::disallowed_methods)]
fn load_state(path: &Path) -> Result<LoadedJournal, CommandJournalError> {
    let content = std::fs::read(path)?;
    let mut state = JournalState::default();
    let mut offset = 0_usize;
    let mut valid_bytes = 0_usize;
    while offset < content.len() {
        let remaining = &content[offset..];
        let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let line_end = offset + newline + 1;
        let text = trim_ascii(&content[offset..line_end - 1]);
        if text.is_empty() {
            valid_bytes = line_end;
            offset = line_end;
            continue;
        }
        let entry = match serde_json::from_slice::<JournalEntry>(text) {
            Ok(entry) => entry,
            Err(_) if content[line_end..].iter().all(u8::is_ascii_whitespace) => break,
            Err(error) => return Err(error.into()),
        };
        match entry {
            JournalEntry::Header { version } => {
                if version != JOURNAL_VERSION {
                    return Err(CommandJournalError::IdentityConflict {
                        command_seq: 0,
                        message: format!("unsupported command journal version {version}"),
                    });
                }
            }
            JournalEntry::Prepared {
                command_seq,
                command_id,
                payload_digest,
            } => {
                if state.records.contains_key(&command_seq)
                    || state.ids.contains_key(command_id.as_str())
                {
                    return Err(CommandJournalError::IdentityConflict {
                        command_seq,
                        message: "duplicate preparation record".to_string(),
                    });
                }
                state
                    .ids
                    .insert(command_id.as_str().to_string(), command_seq);
                state.records.insert(
                    command_seq,
                    CommandRecord {
                        command_id,
                        payload_digest,
                        outcome: None,
                    },
                );
            }
            JournalEntry::Outcome { outcome } => {
                let record = state.records.get_mut(&outcome.command_seq).ok_or(
                    CommandJournalError::MissingPreparation {
                        command_seq: outcome.command_seq,
                    },
                )?;
                if record.command_id != outcome.command_id {
                    return Err(CommandJournalError::IdentityConflict {
                        command_seq: outcome.command_seq,
                        message: "loaded outcome command id mismatch".to_string(),
                    });
                }
                if let Some(prior) = &record.outcome {
                    let valid_progression = prior == &outcome
                        || (prior.accepted
                            && !prior.terminal
                            && outcome.accepted
                            && outcome.terminal);
                    if !valid_progression {
                        return Err(CommandJournalError::IdentityConflict {
                            command_seq: outcome.command_seq,
                            message: "loaded command outcome changed incompatibly".to_string(),
                        });
                    }
                }
                record.outcome = Some(outcome);
            }
            JournalEntry::Ack {
                through_command_seq,
            } => {
                if through_command_seq < state.outcome_ack {
                    return Err(CommandJournalError::IdentityConflict {
                        command_seq: through_command_seq,
                        message: "loaded outcome acknowledgement moved backwards".to_string(),
                    });
                }
                require_terminal_outcomes_through(
                    &state,
                    state.outcome_ack.saturating_add(1),
                    through_command_seq,
                )?;
                state.outcome_ack = through_command_seq;
            }
        }
        valid_bytes = line_end;
        offset = line_end;
    }
    let mut expected = 1;
    for seq in state.records.keys() {
        if *seq != expected {
            return Err(CommandJournalError::SequenceGap {
                expected,
                actual: *seq,
            });
        }
        expected = expected.saturating_add(1);
    }
    Ok(LoadedJournal {
        state,
        valid_bytes: valid_bytes as u64,
    })
}

fn require_terminal_outcomes_through(
    state: &JournalState,
    from_command_seq: u64,
    through_command_seq: u64,
) -> Result<(), CommandJournalError> {
    for command_seq in from_command_seq..=through_command_seq {
        let terminal = state
            .records
            .get(&command_seq)
            .and_then(|record| record.outcome.as_ref())
            .is_some_and(|outcome| outcome.terminal);
        if !terminal {
            return Err(CommandJournalError::IdentityConflict {
                command_seq,
                message:
                    "outcome acknowledgement covered a command without a durable terminal outcome"
                        .to_string(),
            });
        }
    }
    Ok(())
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise journal truncation and recovery.
#[allow(clippy::disallowed_methods)]
mod tests {
    use bro_protocol::WorkerCommandKind;
    use serde_json::json;

    use super::*;

    fn command(seq: u64, id: &str, text: &str) -> WorkerCommand {
        WorkerCommand {
            command_seq: seq,
            command_id: CommandId::new(id),
            command: WorkerCommandKind::UserTurn {
                text: text.to_string(),
            },
        }
    }

    fn outcome(command: &WorkerCommand, terminal: bool) -> CommandOutcome {
        CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id.clone(),
            accepted: true,
            terminal,
            result_or_error: json!({"queued": true, "terminal": terminal}),
        }
    }

    fn create_private_journal(path: &Path) -> File {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(path).unwrap()
    }

    #[test]
    fn duplicate_command_replays_durable_outcome_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        {
            let journal = CommandJournal::open(&path).unwrap();
            assert_eq!(journal.prepare(&first).unwrap(), CommandDisposition::Apply);
            journal.finish(outcome(&first, true)).unwrap();
        }

        let reopened = CommandJournal::open(&path).unwrap();
        assert_eq!(
            reopened.prepare(&first).unwrap(),
            CommandDisposition::Duplicate(outcome(&first, true))
        );
        assert_eq!(reopened.next_command_seq(), 2);
    }

    #[test]
    fn payload_digest_and_sequence_mismatches_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let first = command(1, "command-1", "hello");
        assert_eq!(journal.prepare(&first).unwrap(), CommandDisposition::Apply);
        journal.finish(outcome(&first, true)).unwrap();

        assert!(matches!(
            journal.prepare(&command(1, "command-1", "changed")),
            Err(CommandJournalError::IdentityConflict { .. })
        ));
        assert!(matches!(
            journal.prepare(&command(3, "command-3", "gap")),
            Err(CommandJournalError::SequenceGap {
                expected: 2,
                actual: 3
            })
        ));
    }

    #[test]
    fn accepted_nonterminal_outcome_can_advance_once_to_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let drain = WorkerCommand {
            command_seq: 1,
            command_id: CommandId::new("drain-1"),
            command: WorkerCommandKind::Drain {
                deadline_unix_ms: None,
                reason: None,
                safe_boundary: Default::default(),
            },
        };
        journal.prepare(&drain).unwrap();
        journal.finish(outcome(&drain, false)).unwrap();
        journal.finish(outcome(&drain, true)).unwrap();
        assert_eq!(
            journal.prepare(&drain).unwrap(),
            CommandDisposition::Duplicate(outcome(&drain, true))
        );
    }

    #[test]
    fn restart_rejects_a_durable_preparation_without_reapplying_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        {
            let journal = CommandJournal::open(&path).unwrap();
            assert_eq!(journal.prepare(&first).unwrap(), CommandDisposition::Apply);
        }

        let reopened = CommandJournal::open(&path).unwrap();
        let CommandDisposition::Duplicate(recovered) = reopened.prepare(&first).unwrap() else {
            panic!("prepared command was offered for application after restart");
        };
        assert!(!recovered.accepted);
        assert!(recovered.terminal);
        assert_eq!(
            recovered.result_or_error["code"],
            INDETERMINATE_OUTCOME_CODE
        );
        assert_eq!(
            reopened
                .unacknowledged_outcomes()
                .into_iter()
                .map(|item| item.command_seq)
                .collect::<Vec<_>>(),
            vec![1]
        );

        let second = command(2, "command-2", "continue");
        assert_eq!(
            reopened.prepare(&second).unwrap(),
            CommandDisposition::Apply
        );
    }

    #[test]
    fn restart_truncates_a_partial_outcome_before_appending_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        {
            let journal = CommandJournal::open(&path).unwrap();
            journal.prepare(&first).unwrap();
        }
        let valid_bytes = std::fs::metadata(&path).unwrap().len();
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"{\"kind\":\"outcome\",\"outcome\":{")
                .unwrap();
            file.sync_data().unwrap();
        }

        let reopened = CommandJournal::open(&path).unwrap();
        let recovered = match reopened.prepare(&first).unwrap() {
            CommandDisposition::Duplicate(outcome) => outcome,
            CommandDisposition::Apply => panic!("partial outcome allowed a second application"),
        };
        assert!(!recovered.accepted);
        assert!(recovered.terminal);
        assert!(std::fs::metadata(&path).unwrap().len() > valid_bytes);
        for line in std::fs::read(&path).unwrap().split(|byte| *byte == b'\n') {
            if !line.is_empty() {
                serde_json::from_slice::<JournalEntry>(line).unwrap();
            }
        }
    }

    #[test]
    fn restart_discards_a_valid_but_unterminated_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        {
            let journal = CommandJournal::open(&path).unwrap();
            journal.prepare(&first).unwrap();
        }
        let unterminated = serde_json::to_vec(&JournalEntry::Outcome {
            outcome: outcome(&first, true),
        })
        .unwrap();
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&unterminated).unwrap();
            file.sync_data().unwrap();
        }

        let reopened = CommandJournal::open(&path).unwrap();
        let recovered = match reopened.prepare(&first).unwrap() {
            CommandDisposition::Duplicate(outcome) => outcome,
            CommandDisposition::Apply => {
                panic!("unterminated outcome allowed a second application")
            }
        };
        assert!(!recovered.accepted);
        assert!(recovered.terminal);
        assert_eq!(
            recovered.result_or_error["code"],
            INDETERMINATE_OUTCOME_CODE
        );
    }

    #[test]
    fn restart_removes_an_invalid_final_line_and_accepts_the_next_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        {
            let journal = CommandJournal::open(&path).unwrap();
            journal.prepare(&first).unwrap();
            journal.finish(outcome(&first, true)).unwrap();
        }
        let valid_bytes = std::fs::metadata(&path).unwrap().len();
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"not-json\n").unwrap();
            file.sync_data().unwrap();
        }

        let reopened = CommandJournal::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), valid_bytes);
        let second = command(2, "command-2", "continue");
        assert_eq!(
            reopened.prepare(&second).unwrap(),
            CommandDisposition::Apply
        );
    }

    #[test]
    fn restart_rejects_incompatible_repeated_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        let digest = command_payload_digest(&first).unwrap();
        let mut changed = outcome(&first, true);
        changed.result_or_error = json!({"changed": true});
        {
            let mut file = create_private_journal(&path);
            append_entry(
                &mut file,
                &JournalEntry::Header {
                    version: JOURNAL_VERSION,
                },
            )
            .unwrap();
            append_entry(
                &mut file,
                &JournalEntry::Prepared {
                    command_seq: 1,
                    command_id: first.command_id.clone(),
                    payload_digest: digest,
                },
            )
            .unwrap();
            append_entry(
                &mut file,
                &JournalEntry::Outcome {
                    outcome: outcome(&first, true),
                },
            )
            .unwrap();
            append_entry(&mut file, &JournalEntry::Outcome { outcome: changed }).unwrap();
        }

        assert!(matches!(
            CommandJournal::open(path),
            Err(CommandJournalError::IdentityConflict { .. })
        ));
    }

    #[test]
    fn outcome_ack_requires_contiguous_terminal_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("commands.jsonl");
        let first = command(1, "command-1", "hello");
        let digest = command_payload_digest(&first).unwrap();
        {
            let mut file = create_private_journal(&path);
            append_entry(
                &mut file,
                &JournalEntry::Header {
                    version: JOURNAL_VERSION,
                },
            )
            .unwrap();
            append_entry(
                &mut file,
                &JournalEntry::Prepared {
                    command_seq: 1,
                    command_id: first.command_id.clone(),
                    payload_digest: digest,
                },
            )
            .unwrap();
            append_entry(
                &mut file,
                &JournalEntry::Outcome {
                    outcome: outcome(&first, false),
                },
            )
            .unwrap();
            append_entry(
                &mut file,
                &JournalEntry::Ack {
                    through_command_seq: 1,
                },
            )
            .unwrap();
        }

        assert!(matches!(
            CommandJournal::open(path),
            Err(CommandJournalError::IdentityConflict { .. })
        ));
    }
}
