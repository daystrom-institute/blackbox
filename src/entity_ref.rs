use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PARSER_VERSION: &str = "entity-ref-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Knowledge,
    Transcript,
    ProjectFile,
    Session,
    Thread,
    Note,
    Symbol,
    Brofile,
    Whiteboard,
    Commit,
    Task,
    BashCall,
}

impl EntityType {
    pub const ALL: [EntityType; 12] = [
        EntityType::Knowledge,
        EntityType::Transcript,
        EntityType::ProjectFile,
        EntityType::Session,
        EntityType::Thread,
        EntityType::Note,
        EntityType::Symbol,
        EntityType::Brofile,
        EntityType::Whiteboard,
        EntityType::Commit,
        EntityType::Task,
        EntityType::BashCall,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EntityType::Knowledge => "knowledge",
            EntityType::Transcript => "transcript",
            EntityType::ProjectFile => "project_file",
            EntityType::Session => "session",
            EntityType::Thread => "thread",
            EntityType::Note => "note",
            EntityType::Symbol => "symbol",
            EntityType::Brofile => "brofile",
            EntityType::Whiteboard => "whiteboard",
            EntityType::Commit => "commit",
            EntityType::Task => "task",
            EntityType::BashCall => "bash_call",
        }
    }

    pub fn example(self) -> &'static str {
        match self {
            EntityType::Knowledge => "knowledge:<entry_id>",
            EntityType::Transcript => {
                "transcript:<provider>:<session_id>:<line_offset>:<event_idx>"
            }
            EntityType::ProjectFile => {
                "project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>"
            }
            EntityType::Session => "session:<provider>:<session_id>",
            EntityType::Thread => "thread:<thread_id>",
            EntityType::Note => "note:<note_id>",
            EntityType::Symbol => "symbol:<project_id>:<qualified_name>:<defn_hash>",
            EntityType::Brofile => "brofile:<name>",
            EntityType::Whiteboard => "whiteboard:<board_id>",
            EntityType::Commit => "commit:<repo_id>:<sha>",
            EntityType::Task => "task:<task_id>",
            EntityType::BashCall => "bash_call:<session>:<turn>",
        }
    }

    pub fn is_virtual(self) -> bool {
        matches!(self, EntityType::Task | EntityType::BashCall)
    }

    pub(crate) fn from_prefix(prefix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| ty.as_str() == prefix)
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// Canonical entity references use `:` as the field delimiter. Provider names
/// are short identifier-like strings and must not contain `:`; use
/// [`EntityRef::try_render`] when constructing refs from untrusted provider
/// strings so invalid values return a clear error instead of rendering an
/// ambiguous grammar form. `Display` is non-panicking and falls back to an
/// explicit invalid-entity sentinel for malformed in-memory values; use
/// `try_render` for boundary serialization that must reject invalid refs.
pub enum EntityRef {
    Knowledge {
        id: String,
    },
    Transcript {
        provider: String,
        session_id: String,
        line_offset: u64,
        event_idx: u32,
    },
    ProjectFile {
        project_id: String,
        rel_path_hash: String,
        chunk_hash: String,
        occurrence_idx: u32,
    },
    Session {
        provider: String,
        session_id: String,
    },
    Thread {
        thread_id: String,
    },
    Note {
        note_id: String,
    },
    Symbol {
        project_id: String,
        qualified_name: String,
        defn_hash: String,
    },
    Brofile {
        name: String,
    },
    Whiteboard {
        board_id: String,
    },
    Commit {
        repo_id: String,
        sha: String,
    },
    Task {
        task_id: String,
    },
    BashCall {
        session: String,
        turn: u32,
    },
}

impl EntityRef {
    pub fn parse(input: &str) -> Result<Self, EntityRefParseError> {
        let input = input.trim();
        let (prefix, rest) = input.split_once(':').ok_or_else(|| {
            EntityRefParseError::bad_input(
                input,
                "entity ref must start with a known type followed by ':'",
                Some(format!("Use one of: {}", examples_csv())),
            )
        })?;

        let entity_type = EntityType::from_prefix(prefix).ok_or_else(|| {
            let suggested_fix =
                closest_type(prefix).map(|ty| format!("Did you mean `{}`?", ty.example()));
            EntityRefParseError::bad_input(
                input,
                format!("unknown entity ref type `{prefix}`"),
                suggested_fix.or_else(|| Some(format!("Use one of: {}", examples_csv()))),
            )
        })?;

        match entity_type {
            EntityType::Knowledge => parse_single(input, rest, EntityType::Knowledge, |id| {
                EntityRef::Knowledge { id }
            }),
            EntityType::Transcript => parse_transcript(input, rest),
            EntityType::ProjectFile => parse_project_file(input, rest),
            EntityType::Session => parse_session(input, rest),
            EntityType::Thread => parse_single(input, rest, EntityType::Thread, |thread_id| {
                EntityRef::Thread { thread_id }
            }),
            EntityType::Note => parse_single(input, rest, EntityType::Note, |note_id| {
                EntityRef::Note { note_id }
            }),
            EntityType::Symbol => parse_symbol(input, rest),
            EntityType::Brofile => parse_single(input, rest, EntityType::Brofile, |name| {
                EntityRef::Brofile { name }
            }),
            EntityType::Whiteboard => {
                parse_single(input, rest, EntityType::Whiteboard, |board_id| {
                    EntityRef::Whiteboard { board_id }
                })
            }
            EntityType::Commit => parse_commit(input, rest),
            EntityType::Task => parse_single(input, rest, EntityType::Task, |task_id| {
                EntityRef::Task { task_id }
            }),
            EntityType::BashCall => parse_bash_call(input, rest),
        }
    }

    pub fn render(&self) -> String {
        self.try_render()
            .expect("EntityRef::render called with invalid in-memory entity ref")
    }

    pub fn try_render(&self) -> Result<String, EntityRefRenderError> {
        match self {
            EntityRef::Knowledge { id } => Ok(format!("knowledge:{id}")),
            EntityRef::Transcript {
                provider,
                session_id,
                line_offset,
                event_idx,
            } => {
                validate_provider(provider)?;
                Ok(format!(
                    "transcript:{provider}:{session_id}:{line_offset}:{event_idx}"
                ))
            }
            EntityRef::ProjectFile {
                project_id,
                rel_path_hash,
                chunk_hash,
                occurrence_idx,
            } => Ok(format!(
                "project_file:{project_id}:{rel_path_hash}:{chunk_hash}:{occurrence_idx}"
            )),
            EntityRef::Session {
                provider,
                session_id,
            } => {
                validate_provider(provider)?;
                Ok(format!("session:{provider}:{session_id}"))
            }
            EntityRef::Thread { thread_id } => Ok(format!("thread:{thread_id}")),
            EntityRef::Note { note_id } => Ok(format!("note:{note_id}")),
            EntityRef::Symbol {
                project_id,
                qualified_name,
                defn_hash,
            } => Ok(format!("symbol:{project_id}:{qualified_name}:{defn_hash}")),
            EntityRef::Brofile { name } => Ok(format!("brofile:{name}")),
            EntityRef::Whiteboard { board_id } => Ok(format!("whiteboard:{board_id}")),
            EntityRef::Commit { repo_id, sha } => Ok(format!("commit:{repo_id}:{sha}")),
            EntityRef::Task { task_id } => Ok(format!("task:{task_id}")),
            EntityRef::BashCall { session, turn } => Ok(format!("bash_call:{session}:{turn}")),
        }
    }

    pub fn entity_type(&self) -> EntityType {
        match self {
            EntityRef::Knowledge { .. } => EntityType::Knowledge,
            EntityRef::Transcript { .. } => EntityType::Transcript,
            EntityRef::ProjectFile { .. } => EntityType::ProjectFile,
            EntityRef::Session { .. } => EntityType::Session,
            EntityRef::Thread { .. } => EntityType::Thread,
            EntityRef::Note { .. } => EntityType::Note,
            EntityRef::Symbol { .. } => EntityType::Symbol,
            EntityRef::Brofile { .. } => EntityType::Brofile,
            EntityRef::Whiteboard { .. } => EntityType::Whiteboard,
            EntityRef::Commit { .. } => EntityType::Commit,
            EntityRef::Task { .. } => EntityType::Task,
            EntityRef::BashCall { .. } => EntityType::BashCall,
        }
    }

    pub fn is_virtual(&self) -> bool {
        self.entity_type().is_virtual()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRefRenderError {
    pub field: &'static str,
    pub value: String,
    pub message: String,
}

impl EntityRefRenderError {
    fn invalid_provider(provider: &str) -> Self {
        Self {
            field: "provider",
            value: provider.to_string(),
            message: format!("provider must be non-empty and must not contain ':': `{provider}`"),
        }
    }
}

impl fmt::Display for EntityRefRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EntityRefRenderError {}

impl fmt::Display for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_render() {
            Ok(rendered) => f.write_str(&rendered),
            Err(err) => write!(
                f,
                "<invalid entity-ref kind={} field={} value={}>",
                self.entity_type(),
                err.field,
                err.value
            ),
        }
    }
}

fn validate_provider(provider: &str) -> Result<(), EntityRefRenderError> {
    if provider.is_empty() || provider.contains(':') {
        Err(EntityRefRenderError::invalid_provider(provider))
    } else {
        Ok(())
    }
}

impl FromStr for EntityRef {
    type Err = EntityRefParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        EntityRef::parse(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRefParseError {
    pub status: String,
    pub code: String,
    pub message: String,
    pub field: String,
    pub suggested_fix: Option<String>,
}

impl EntityRefParseError {
    fn bad_input(input: &str, message: impl Into<String>, suggested_fix: Option<String>) -> Self {
        Self {
            status: "error.bad_input".to_string(),
            code: "invalid_entity_ref".to_string(),
            message: format!("{}: `{input}`", message.into()),
            field: "entity_ref".to_string(),
            suggested_fix,
        }
    }
}

impl fmt::Display for EntityRefParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.suggested_fix {
            Some(suggested_fix) => write!(f, "{} ({suggested_fix})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for EntityRefParseError {}

pub fn project_id_for_path(path: impl AsRef<Path>) -> io::Result<String> {
    realpath_hash(path)
}

pub fn repo_id_for_path(path: impl AsRef<Path>) -> io::Result<String> {
    let canonical = canonical_input_path(path)?;
    let Some(repo_root) = git_root_for_path(&canonical) else {
        return Ok(hash_path(&canonical));
    };
    repo_id_for_root(&repo_root)
}

pub(crate) fn repo_id_for_root(git_root: &Path) -> io::Result<String> {
    if let Some(first_commit) = git_first_commit_for_path(git_root) {
        return Ok(hash_string(&first_commit));
    }
    if let Some(remote_url) = git_remote_origin_for_path(git_root) {
        return Ok(hash_string(&remote_url));
    }
    Ok(hash_path(git_root))
}

pub fn realpath_hash(path: impl AsRef<Path>) -> io::Result<String> {
    canonical_input_path(path).map(|path| hash_path(&path))
}

/// Hashes a canonical path to 8 lowercase hex characters.
///
/// The value is the first 4 bytes of SHA-256, giving a 32-bit collision space.
/// That is intentionally compact for local project/entity IDs and is not a
/// cryptographic uniqueness guarantee.
pub fn hash_path(path: &Path) -> String {
    hash_bytes(path.to_string_lossy().as_bytes())
}

fn hash_string(value: &str) -> String {
    hash_bytes(value.as_bytes())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

fn canonical_input_path(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    if canonical.is_file() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "project path must be a directory, not a file: {}",
                canonical.display()
            ),
        ))
    } else {
        Ok(canonical)
    }
}

pub(crate) fn git_root_for_path(path: &Path) -> Option<PathBuf> {
    let output = git_output(
        path,
        &["rev-parse", "--show-toplevel"],
        "deriving repository root",
    )?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    fs::canonicalize(root.trim()).ok()
}

fn git_first_commit_for_path(path: &Path) -> Option<String> {
    let output = git_output(
        path,
        &["rev-list", "--max-parents=0", "HEAD"],
        "deriving first commit",
    )?;
    if !output.status.success() {
        return None;
    }
    git_first_commit_from_stdout(&output.stdout)
}

fn git_first_commit_from_stdout(stdout: &[u8]) -> Option<String> {
    let raw = String::from_utf8(stdout.to_vec()).ok()?;
    let mut roots: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    roots.sort_unstable();
    roots.first().map(|line| (*line).to_string())
}

fn git_remote_origin_for_path(path: &Path) -> Option<String> {
    let output = git_output(
        path,
        &["config", "remote.origin.url"],
        "deriving remote origin URL",
    )?;
    if !output.status.success() {
        return None;
    }
    let remote = String::from_utf8(output.stdout).ok()?;
    let remote = remote.trim();
    if remote.is_empty() {
        None
    } else {
        Some(remote.to_string())
    }
}

fn git_output(path: &Path, args: &[&str], action: &'static str) -> Option<std::process::Output> {
    match Command::new("git").arg("-C").arg(path).args(args).output() {
        Ok(output) => Some(output),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                action,
                "failed to execute git"
            );
            None
        }
    }
}

fn parse_single(
    input: &str,
    rest: &str,
    entity_type: EntityType,
    build: impl FnOnce(String) -> EntityRef,
) -> Result<EntityRef, EntityRefParseError> {
    require_no_colon(input, rest, entity_type)?;
    Ok(build(
        non_empty(input, rest, entity_type, "id")?.to_string(),
    ))
}

fn parse_transcript(input: &str, rest: &str) -> Result<EntityRef, EntityRefParseError> {
    let (head, event_idx) = split_last(input, rest, EntityType::Transcript, "event_idx")?;
    let (head, line_offset) = split_last(input, head, EntityType::Transcript, "line_offset")?;
    let (provider, session_id) = split_first(input, head, EntityType::Transcript, "session_id")?;
    Ok(EntityRef::Transcript {
        provider: non_empty(input, provider, EntityType::Transcript, "provider")?.to_string(),
        session_id: non_empty(input, session_id, EntityType::Transcript, "session_id")?.to_string(),
        line_offset: parse_u64(input, line_offset, EntityType::Transcript, "line_offset")?,
        event_idx: parse_u32(input, event_idx, EntityType::Transcript, "event_idx")?,
    })
}

fn parse_project_file(input: &str, rest: &str) -> Result<EntityRef, EntityRefParseError> {
    let parts = exact_parts(input, rest, EntityType::ProjectFile, 4)?;
    Ok(EntityRef::ProjectFile {
        project_id: non_empty(input, parts[0], EntityType::ProjectFile, "project_id")?.to_string(),
        rel_path_hash: non_empty(input, parts[1], EntityType::ProjectFile, "rel_path_hash")?
            .to_string(),
        chunk_hash: non_empty(input, parts[2], EntityType::ProjectFile, "chunk_hash")?.to_string(),
        occurrence_idx: parse_u32(input, parts[3], EntityType::ProjectFile, "occurrence_idx")?,
    })
}

fn parse_session(input: &str, rest: &str) -> Result<EntityRef, EntityRefParseError> {
    let (provider, session_id) = split_first(input, rest, EntityType::Session, "session_id")?;
    Ok(EntityRef::Session {
        provider: non_empty(input, provider, EntityType::Session, "provider")?.to_string(),
        session_id: non_empty(input, session_id, EntityType::Session, "session_id")?.to_string(),
    })
}

fn parse_symbol(input: &str, rest: &str) -> Result<EntityRef, EntityRefParseError> {
    let (project_id, tail) = split_first(input, rest, EntityType::Symbol, "qualified_name")?;
    let (qualified_name, defn_hash) = split_last(input, tail, EntityType::Symbol, "defn_hash")?;
    Ok(EntityRef::Symbol {
        project_id: non_empty(input, project_id, EntityType::Symbol, "project_id")?.to_string(),
        qualified_name: non_empty(input, qualified_name, EntityType::Symbol, "qualified_name")?
            .to_string(),
        defn_hash: non_empty(input, defn_hash, EntityType::Symbol, "defn_hash")?.to_string(),
    })
}

fn parse_commit(input: &str, rest: &str) -> Result<EntityRef, EntityRefParseError> {
    let parts = exact_parts(input, rest, EntityType::Commit, 2)?;
    Ok(EntityRef::Commit {
        repo_id: non_empty(input, parts[0], EntityType::Commit, "repo_id")?.to_string(),
        sha: non_empty(input, parts[1], EntityType::Commit, "sha")?.to_string(),
    })
}

fn parse_bash_call(input: &str, rest: &str) -> Result<EntityRef, EntityRefParseError> {
    let (session, turn) = split_last(input, rest, EntityType::BashCall, "turn")?;
    Ok(EntityRef::BashCall {
        session: non_empty(input, session, EntityType::BashCall, "session")?.to_string(),
        turn: parse_u32(input, turn, EntityType::BashCall, "turn")?,
    })
}

fn exact_parts<'a>(
    input: &str,
    rest: &'a str,
    entity_type: EntityType,
    expected: usize,
) -> Result<Vec<&'a str>, EntityRefParseError> {
    let parts: Vec<_> = rest.split(':').collect();
    if parts.len() == expected {
        Ok(parts)
    } else {
        Err(shape_error(input, entity_type))
    }
}

fn require_no_colon(
    input: &str,
    rest: &str,
    entity_type: EntityType,
) -> Result<(), EntityRefParseError> {
    if rest.contains(':') {
        Err(shape_error(input, entity_type))
    } else {
        Ok(())
    }
}

fn split_first<'a>(
    input: &str,
    rest: &'a str,
    entity_type: EntityType,
    missing_field: &str,
) -> Result<(&'a str, &'a str), EntityRefParseError> {
    rest.split_once(':').ok_or_else(|| {
        EntityRefParseError::bad_input(
            input,
            format!("missing `{missing_field}` in {}", entity_type.as_str()),
            Some(format!("Expected `{}`", entity_type.example())),
        )
    })
}

fn split_last<'a>(
    input: &str,
    rest: &'a str,
    entity_type: EntityType,
    missing_field: &str,
) -> Result<(&'a str, &'a str), EntityRefParseError> {
    rest.rsplit_once(':').ok_or_else(|| {
        EntityRefParseError::bad_input(
            input,
            format!("missing `{missing_field}` in {}", entity_type.as_str()),
            Some(format!("Expected `{}`", entity_type.example())),
        )
    })
}

fn non_empty<'a>(
    input: &str,
    value: &'a str,
    entity_type: EntityType,
    field: &str,
) -> Result<&'a str, EntityRefParseError> {
    if value.is_empty() {
        Err(EntityRefParseError::bad_input(
            input,
            format!("empty `{field}` in {}", entity_type.as_str()),
            Some(format!("Expected `{}`", entity_type.example())),
        ))
    } else {
        Ok(value)
    }
}

fn parse_u32(
    input: &str,
    value: &str,
    entity_type: EntityType,
    field: &str,
) -> Result<u32, EntityRefParseError> {
    value
        .parse()
        .map_err(|_| number_error(input, entity_type, field))
}

fn parse_u64(
    input: &str,
    value: &str,
    entity_type: EntityType,
    field: &str,
) -> Result<u64, EntityRefParseError> {
    value
        .parse()
        .map_err(|_| number_error(input, entity_type, field))
}

fn number_error(input: &str, entity_type: EntityType, field: &str) -> EntityRefParseError {
    EntityRefParseError::bad_input(
        input,
        format!("`{field}` must be numeric in {}", entity_type.as_str()),
        Some(format!("Expected `{}`", entity_type.example())),
    )
}

fn shape_error(input: &str, entity_type: EntityType) -> EntityRefParseError {
    EntityRefParseError::bad_input(
        input,
        format!("invalid {} entity-ref shape", entity_type.as_str()),
        Some(format!("Expected `{}`", entity_type.example())),
    )
}

fn closest_type(prefix: &str) -> Option<EntityType> {
    EntityType::ALL
        .into_iter()
        .map(|ty| (levenshtein(prefix, ty.as_str()), ty))
        .filter(|(distance, _)| *distance <= 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, ty)| ty)
}

fn examples_csv() -> String {
    EntityType::ALL
        .into_iter()
        .map(EntityType::example)
        .collect::<Vec<_>>()
        .join(", ")
}

fn levenshtein(a: &str, b: &str) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.bytes().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.bytes().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current[j + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_property_10k_random_entities() {
        let mut rng = Lcg::new(0x5eed_f1f1);
        for i in 0..10_000 {
            let entity = random_entity(&mut rng, i);
            match entity.try_render() {
                Ok(rendered) => {
                    let parsed = EntityRef::parse(&rendered).unwrap();
                    assert_eq!(parsed, entity);
                    assert_eq!(parsed.render(), rendered);
                }
                Err(err) => {
                    assert_eq!(err.field, "provider");
                    assert!(matches!(
                        entity,
                        EntityRef::Transcript { .. } | EntityRef::Session { .. }
                    ));
                }
            }
        }
    }

    #[test]
    fn provider_colons_are_rejected_at_render_time() {
        let entity = EntityRef::Session {
            provider: "claude:code".to_string(),
            session_id: "session-1".to_string(),
        };
        let err = entity.try_render().unwrap_err();
        assert_eq!(err.field, "provider");
        assert!(err.message.contains("must not contain ':'"));
    }

    #[test]
    fn display_of_invalid_provider_is_non_panicking_sentinel() {
        let entity = EntityRef::Session {
            provider: "claude:code".to_string(),
            session_id: "session-1".to_string(),
        };

        assert_eq!(
            entity.to_string(),
            "<invalid entity-ref kind=session field=provider value=claude:code>"
        );
    }

    #[test]
    fn project_ids_reject_file_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        fs::write(&file_path, "not a directory").unwrap();

        let err = project_id_for_path(&file_path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("must be a directory"));
    }

    #[test]
    fn git_repo_id_uses_first_commit_hash_not_realpath_hash() {
        let repo_path = Path::new(env!("CARGO_MANIFEST_DIR"));
        let first_commit = git_first_commit_for_path(repo_path).unwrap();
        let repo_id = repo_id_for_path(repo_path).unwrap();
        let realpath_id = realpath_hash(repo_path).unwrap();

        assert_eq!(repo_id, hash_string(&first_commit));
        assert_ne!(repo_id, realpath_id);
    }

    #[test]
    fn repo_id_falls_back_to_remote_origin_without_commits() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .output()
            .unwrap();
        assert!(init.status.success());
        let remote = "https://example.invalid/transcript-search.git";
        let remote_add = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["remote", "add", "origin", remote])
            .output()
            .unwrap();
        assert!(remote_add.status.success());

        assert_eq!(repo_id_for_path(dir.path()).unwrap(), hash_string(remote));
    }

    #[test]
    fn first_commit_stdout_uses_lexicographic_min_root() {
        let stdout =
            b"ffff000000000000000000000000000000000000\n1111000000000000000000000000000000000000\n";

        assert_eq!(
            git_first_commit_from_stdout(stdout).as_deref(),
            Some("1111000000000000000000000000000000000000")
        );
    }

    #[test]
    fn commit_ref_round_trips_with_repo_id() {
        let repo_id = repo_id_for_path(env!("CARGO_MANIFEST_DIR")).unwrap();
        assert_eq!(repo_id.len(), 8);
        let entity = EntityRef::Commit {
            repo_id,
            sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
        };
        let parsed = EntityRef::parse(&entity.render()).unwrap();
        assert_eq!(parsed, entity);
    }

    #[test]
    fn parse_bad_input_returns_error_shape_with_suggestion() {
        let err = EntityRef::parse("knowlege:abc").unwrap_err();
        assert_eq!(err.status, "error.bad_input");
        assert_eq!(err.code, "invalid_entity_ref");
        assert_eq!(err.field, "entity_ref");
        assert!(err
            .suggested_fix
            .as_deref()
            .unwrap_or_default()
            .contains("knowledge:<entry_id>"));
    }

    #[test]
    fn numeric_parse_error_suggests_expected_grammar() {
        let err = EntityRef::parse("transcript:codex:sess-1:not-a-line:2").unwrap_err();
        assert_eq!(err.status, "error.bad_input");
        assert!(err
            .suggested_fix
            .as_deref()
            .unwrap_or_default()
            .contains("line_offset"));
    }

    #[test]
    fn virtual_classification_matches_design() {
        assert!(EntityRef::Task {
            task_id: "task-1".to_string()
        }
        .is_virtual());
        assert!(EntityRef::BashCall {
            session: "sess".to_string(),
            turn: 4
        }
        .is_virtual());
        assert!(!EntityRef::Commit {
            repo_id: "abcd1234".to_string(),
            sha: "abc".to_string()
        }
        .is_virtual());
    }

    #[derive(Clone)]
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        fn bounded(&mut self, upper: usize) -> usize {
            (self.next() as usize) % upper
        }

        fn token(&mut self, prefix: &str) -> String {
            const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";
            let len = 4 + self.bounded(20);
            let mut out = String::from(prefix);
            for _ in 0..len {
                out.push(ALPHABET[self.bounded(ALPHABET.len())] as char);
            }
            out
        }

        fn provider(&mut self, prefix: &str, allow_colon: bool) -> String {
            let provider = self.token(prefix);
            if allow_colon && self.bounded(3) == 0 {
                format!("{}:{}", provider, self.token("sub-"))
            } else {
                provider
            }
        }

        fn hex(&mut self, len: usize) -> String {
            const HEX: &[u8] = b"0123456789abcdef";
            let mut out = String::with_capacity(len);
            for _ in 0..len {
                out.push(HEX[self.bounded(HEX.len())] as char);
            }
            out
        }
    }

    fn random_entity(rng: &mut Lcg, i: usize) -> EntityRef {
        match i % EntityType::ALL.len() {
            0 => EntityRef::Knowledge {
                id: rng.token("know-"),
            },
            1 => EntityRef::Transcript {
                provider: rng.provider("p", true),
                session_id: format!("{}:{}", rng.token("sess-"), rng.token("turn-")),
                line_offset: rng.next(),
                event_idx: rng.next() as u32,
            },
            2 => EntityRef::ProjectFile {
                project_id: rng.hex(8),
                rel_path_hash: rng.hex(8),
                chunk_hash: rng.hex(64),
                occurrence_idx: rng.next() as u32,
            },
            3 => EntityRef::Session {
                provider: rng.provider("p", true),
                session_id: format!("{}:{}", rng.token("sess-"), rng.token("sub-")),
            },
            4 => EntityRef::Thread {
                thread_id: rng.token("thread-"),
            },
            5 => EntityRef::Note {
                note_id: rng.token("note-"),
            },
            6 => EntityRef::Symbol {
                project_id: rng.hex(8),
                qualified_name: format!(
                    "{}::{}::{}",
                    rng.token("crate"),
                    rng.token("mod"),
                    rng.token("Type")
                ),
                defn_hash: rng.hex(64),
            },
            7 => EntityRef::Brofile {
                name: rng.token("bro-"),
            },
            8 => EntityRef::Whiteboard {
                board_id: rng.token("board-"),
            },
            9 => EntityRef::Commit {
                repo_id: rng.hex(8),
                sha: rng.hex(40),
            },
            10 => EntityRef::Task {
                task_id: rng.token("task-"),
            },
            11 => EntityRef::BashCall {
                session: format!("{}:{}", rng.token("sess-"), rng.token("tool-")),
                turn: rng.next() as u32,
            },
            _ => unreachable!(),
        }
    }
}
