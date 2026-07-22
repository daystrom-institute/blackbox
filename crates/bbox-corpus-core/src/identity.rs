//! Durable identity primitives for the checkout/knowledge seam.
//!
//! Slice 1a of
//! `design/corpus/knowledge/checkout-identity-and-provisional-knowledge.md`.
//! These are PURE, ADDITIVE primitives: minting and resolution helpers plus a
//! reuse-safe checkout marker. Nothing here rewires how knowledge scope keys
//! are actually chosen today — that wiring lands in later slices. The point of
//! this slice is that the primitives exist, are correct, and are tested, so the
//! provisional-lane and migration work can consume them.
//!
//! Two identity axes live here:
//!
//! - **`repo_id`** — the durable, cross-host repo-FAMILY id. The authoritative
//!   value is RECORDED in committed `.bbox/config.toml`; the legacy 32-bit
//!   `entity_ref::repo_id_for_root` hash is only a bootstrap hint. Minting
//!   fails CLOSED on shallow clones (see [`mint_repo_id`]). Resolution walks a
//!   fixed precedence ladder (see [`resolve_repo_id`]).
//! - **`checkout_id`** — the host-local, reuse-safe identity of one concrete
//!   checkout, persisted in `.bbox/local/checkout-id` (gitignored). Strong
//!   random, minted once, so a replacement checkout at the same path never
//!   inherits a removed checkout's state (see [`ensure_checkout_id`]).

use std::io::{self, Read};
use std::path::Path;

use anyhow::{Context, Result};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::git;

/// Result of minting a durable `repo_id` at first eject/init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoIdMint {
    /// The full first-commit SHA — the strong, cross-host repo-family id.
    /// Concurrent clones converge on whichever first commit lands; a genuine
    /// divergence surfaces as a merge conflict in `.bbox/config.toml`.
    FirstCommit(String),
    /// A repo with no first commit (no history yet) records a strong-random
    /// id so it still has a durable key.
    Random(String),
}

impl RepoIdMint {
    /// The recorded value regardless of provenance.
    pub fn value(&self) -> &str {
        match self {
            RepoIdMint::FirstCommit(v) | RepoIdMint::Random(v) => v,
        }
    }

    pub fn into_value(self) -> String {
        match self {
            RepoIdMint::FirstCommit(v) | RepoIdMint::Random(v) => v,
        }
    }
}

/// Why minting refused. Minting is fail-closed: it never fabricates a durable
/// id from an untrustworthy source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoIdMintError {
    /// The repository is a shallow clone, so `git rev-list --max-parents=0`
    /// returns the grafted shallow BOUNDARY, not the true root. Minting here
    /// would fabricate a wrong identity that then travels. The operator must
    /// supply a recorded id, unshallow/fetch full history, or provide an
    /// override before a durable id is minted.
    Shallow,
    /// The path is not inside a git repository, so there is no first commit to
    /// anchor a repo-family id.
    NotAGitRepo,
}

impl std::fmt::Display for RepoIdMintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoIdMintError::Shallow => f.write_str(
                "refusing to mint repo_id from a shallow clone: the apparent \
                 first commit is the shallow boundary, not the repository root; \
                 supply a recorded/override repo_id or fetch full history first",
            ),
            RepoIdMintError::NotAGitRepo => {
                f.write_str("refusing to mint repo_id: path is not inside a git repository")
            }
        }
    }
}

impl std::error::Error for RepoIdMintError {}

/// Mint a durable, cross-host `repo_id` for `git_root`, for recording in
/// committed `.bbox/config.toml`.
///
/// Fail-closed policy (design §3.1, review round 5 finding 1):
///
/// 1. A SHALLOW clone refuses ([`RepoIdMintError::Shallow`]). The shallow
///    boundary is not the true root, so a minted id there would be wrong AND
///    durable AND traveling — the worst combination. Callers must already have
///    a recorded/override id, or unshallow first.
/// 2. A repo WITH a first commit records the **full first-commit SHA** (not its
///    32-bit hash): strong entropy, and concurrent clones converge.
/// 3. A repo with NO first commit (empty history) but that IS a git repo
///    records a strong-random id.
/// 4. A non-repo refuses ([`RepoIdMintError::NotAGitRepo`]).
///
/// This is deliberately distinct from [`resolve_repo_id`]: minting WRITES the
/// authority once; resolution READS whichever authority already exists.
pub fn mint_repo_id(git_root: &Path) -> std::result::Result<RepoIdMint, RepoIdMintError> {
    if git::git_root_for_path(git_root).is_none() {
        return Err(RepoIdMintError::NotAGitRepo);
    }
    if git::is_shallow_repository(git_root) {
        return Err(RepoIdMintError::Shallow);
    }
    match git::git_first_commit_for_path(git_root) {
        Some(sha) => Ok(RepoIdMint::FirstCommit(sha)),
        None => Ok(RepoIdMint::Random(random_hex())),
    }
}

/// Inputs to durable `repo_id` resolution, gathered from a checkout's
/// committed config plus the bootstrap-computed fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoIdInputs {
    /// Operator intent (`[project] project_key_override`) — always wins.
    pub project_key_override: Option<String>,
    /// The committed authority (`[project] repo_id`).
    pub recorded: Option<String>,
    /// Also-known-as ids for history-rewrite reconciliation
    /// (`[project] aka_repo_ids`). Declared, so preferred over the weak
    /// computed hash when no current id is recorded.
    pub aka_repo_ids: Vec<String>,
    /// The legacy computed `entity_ref::repo_id_for_root` hash — bootstrap
    /// hint only, used when a checkout's config has recorded nothing yet.
    pub computed: Option<String>,
}

/// The durable identity of one repo-owned project scope. `repo_id` identifies
/// the repository family across hosts; `bbox_root_relpath` distinguishes
/// independently-owned `.bbox/` roots inside one monorepo checkout.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct PublishedScope {
    pub repo_id: String,
    pub bbox_root_relpath: String,
}

/// Resolve the authoritative durable `repo_id` for a project, walking the
/// fixed precedence ladder (design §3.1):
///
/// `project_key_override` > recorded `repo_id` > first `aka_repo_ids` entry >
/// computed bootstrap hash.
///
/// Returns `None` only when every source is empty (no override, no recorded id,
/// no aka ids, no computed hash) — the caller keeps its own fallback.
pub fn resolve_repo_id(inputs: &RepoIdInputs) -> Option<String> {
    if let Some(v) = non_empty(inputs.project_key_override.as_deref()) {
        return Some(v);
    }
    if let Some(v) = non_empty(inputs.recorded.as_deref()) {
        return Some(v);
    }
    if let Some(v) = inputs
        .aka_repo_ids
        .iter()
        .find_map(|a| non_empty(Some(a.as_str())))
    {
        return Some(v);
    }
    non_empty(inputs.computed.as_deref())
}

/// Resolve only an operator-supplied or recorded durable authority.
///
/// Unlike [`resolve_repo_id`], this deliberately excludes `aka_repo_ids` and
/// the computed bootstrap hint. Live publisher and overlay admission must not
/// turn either migration aid into a new durable scope merely because the
/// implementation reached the checkout before its committed config upgrade.
pub fn resolve_recorded_repo_id(inputs: &RepoIdInputs) -> Option<String> {
    non_empty(inputs.project_key_override.as_deref())
        .or_else(|| non_empty(inputs.recorded.as_deref()))
}

/// True when `candidate` names the same repo family as `inputs` — the resolved
/// authority OR any also-known-as id. This is the reconciliation membership
/// test: an entry keyed under a PRE-rewrite id still matches after a history
/// rewrite records the old id in `aka_repo_ids`.
pub fn repo_id_matches(candidate: &str, inputs: &RepoIdInputs) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return false;
    }
    if resolve_repo_id(inputs).as_deref() == Some(candidate) {
        return true;
    }
    inputs
        .aka_repo_ids
        .iter()
        .any(|a| a.trim() == candidate && !candidate.is_empty())
}

/// Repo-relative path of the `.bbox` root — the monorepo discriminator and the
/// second component of the durable `(repo_id, bbox_root_relpath)` published
/// scope key.
///
/// `project_root` is the directory that owns the `.bbox/` (the base checkout
/// root or a monorepo subproject root); `git_root` is its enclosing repository
/// root. A project AT the repo root normalizes to `"."`. Returns `None` when
/// `project_root` is not under `git_root` (callers treat that as a
/// non-monorepo / unresolved case rather than guessing).
pub fn bbox_root_relpath(git_root: &Path, project_root: &Path) -> Option<String> {
    let rel = project_root.strip_prefix(git_root).ok()?;
    if rel.as_os_str().is_empty() {
        return Some(".".to_string());
    }
    // Normalize separators to `/` so the key is portable across hosts.
    let joined = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some(joined)
}

const CHECKOUT_ID_RELPATH: &str = "checkout-id";

/// Ensure a durable, reuse-safe `checkout_id` for the checkout rooted at
/// `checkout_dir`, persisting it in `.bbox/local/checkout-id` (gitignored).
///
/// Reuse-safety (design §3.2, review round 2 finding 7 / round 4 finding 3):
/// the id is STRONG-RANDOM and minted exactly once, atomically
/// (create-if-absent). It is NOT derived from the path, so a replacement
/// checkout occupying a removed checkout's directory mints a FRESH id and never
/// inherits the removed checkout's overlay/GC state. `checkout_dir` identifies
/// only WHERE the marker lives, never its value — so this same function
/// normalizes the base checkout to a concrete `checkout_id` exactly like any
/// worktree.
///
/// The write races safely under an advisory lock on the host-local directory.
/// Empty or truncated markers left by an older crashed writer are atomically
/// replaced while the lock is held, so every concurrent minter observes one
/// stable id.
// Deliberate host-local marker I/O on a caller thread that is already doing
// path/git work (write-side checkout resolution), not a tokio worker hot path —
// same posture as `git::managed_checkout_root`.
#[allow(clippy::disallowed_methods)]
pub fn ensure_checkout_id(checkout_dir: &Path) -> Result<String> {
    let local_dir = checkout_dir.join(".bbox").join("local");
    let marker = local_dir.join(CHECKOUT_ID_RELPATH);
    reject_checkout_identity_symlinks(checkout_dir)?;

    // Fast path: already minted.
    if let Some(existing) = read_checkout_id(&marker)? {
        return Ok(existing);
    }

    std::fs::create_dir_all(&local_dir)
        .with_context(|| format!("creating {}", local_dir.display()))?;
    reject_checkout_identity_symlinks(checkout_dir)?;

    let local_lock = std::fs::File::open(&local_dir)
        .with_context(|| format!("opening checkout identity lane {}", local_dir.display()))?;
    local_lock
        .lock_exclusive()
        .with_context(|| format!("locking checkout identity lane {}", local_dir.display()))?;
    ensure_local_gitignore(&local_dir)?;
    reject_checkout_identity_symlinks(checkout_dir)?;

    // Recheck after taking the lock. Another process may have completed the
    // marker between the unlocked fast path and lane acquisition.
    if let Some(existing) = read_checkout_id(&marker)? {
        return Ok(existing);
    }

    let candidate = random_hex();
    crate::json_store::atomic_write_bytes_from_dir_locked(
        &marker,
        &local_dir,
        candidate.as_bytes(),
    )
    .with_context(|| format!("writing {}", marker.display()))?;
    local_lock
        .sync_all()
        .with_context(|| format!("fsync checkout identity lane {}", local_dir.display()))?;
    Ok(candidate)
}

/// Read an existing `checkout_id` marker if present and non-empty.
// Host-local marker read; see `ensure_checkout_id`.
#[allow(clippy::disallowed_methods)]
pub fn read_checkout_id(marker: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(marker) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("refusing symlinked checkout-id marker {}", marker.display());
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", marker.display())),
    }
    match std::fs::File::open(marker) {
        Ok(mut f) => {
            let mut buf = String::new();
            f.read_to_string(&mut buf)
                .with_context(|| format!("reading {}", marker.display()))?;
            let trimmed = buf.trim();
            Ok(is_checkout_id(trimmed).then(|| trimmed.to_string()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", marker.display())),
    }
}

/// Keep `.bbox/local/` ignored so the checkout-id marker (and the other
/// host-local sidecars that share this dir) is never committed. A symlink or
/// write failure aborts identity creation rather than writing through an
/// untrusted path. Mirrors `knowledge.rs` and `bbox_project_init`.
#[allow(clippy::disallowed_methods)] // host-local sidecar; see `ensure_checkout_id`
fn ensure_local_gitignore(local_dir: &Path) -> Result<()> {
    let gitignore = local_dir.join(".gitignore");
    if std::fs::symlink_metadata(&gitignore).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        anyhow::bail!(
            "refusing symlinked checkout-local gitignore {}",
            gitignore.display()
        );
    }
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n!.gitignore\n")
            .with_context(|| format!("writing {}", gitignore.display()))?;
    }
    Ok(())
}

fn reject_checkout_identity_symlinks(checkout_dir: &Path) -> Result<()> {
    let bbox_dir = checkout_dir.join(".bbox");
    let local_dir = bbox_dir.join("local");
    let marker = local_dir.join(CHECKOUT_ID_RELPATH);
    for path in [&bbox_dir, &local_dir, &marker] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing checkout identity path through symlink {}",
                    path.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting checkout identity path {}", path.display())
                });
            }
        }
    }
    Ok(())
}

/// 32 lowercase-hex characters (128 bits) of strong OS randomness, via the
/// workspace-standard `uuid` v4 source.
fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn is_checkout_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn non_empty(v: Option<&str>) -> Option<String> {
    let v = v?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git_repo(dir: &Path, shallow: bool, with_commit: bool) {
        run(dir, &["init", "-q"]);
        run(dir, &["config", "user.email", "t@example.com"]);
        run(dir, &["config", "user.name", "Test"]);
        if with_commit {
            std::fs::write(dir.join("f.txt"), "hello").unwrap();
            run(dir, &["add", "."]);
            run(dir, &["commit", "-q", "-m", "first"]);
        }
        if shallow {
            // Fabricate the shallow marker; is_shallow_repository reads
            // `rev-parse --is-shallow-repository`, which honors `.git/shallow`.
            std::fs::write(dir.join(".git").join("shallow"), "").unwrap();
        }
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn mint_returns_full_first_commit_sha() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_git_repo(&root, false, true);
        let mint = mint_repo_id(&root).expect("mint");
        match mint {
            RepoIdMint::FirstCommit(sha) => {
                // Full SHA, not the 32-bit 8-hex hash.
                assert_eq!(sha.len(), 40, "expected full SHA, got {sha}");
                assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
            }
            other => panic!("expected FirstCommit, got {other:?}"),
        }
    }

    #[test]
    fn mint_fails_closed_on_shallow() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_git_repo(&root, true, true);
        assert_eq!(mint_repo_id(&root), Err(RepoIdMintError::Shallow));
    }

    #[test]
    fn mint_random_for_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_git_repo(&root, false, false);
        match mint_repo_id(&root).expect("mint") {
            RepoIdMint::Random(v) => assert_eq!(v.len(), 32),
            other => panic!("expected Random, got {other:?}"),
        }
    }

    #[test]
    fn mint_refuses_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(mint_repo_id(&root), Err(RepoIdMintError::NotAGitRepo));
    }

    #[test]
    fn resolve_precedence_ladder() {
        // override wins over everything
        let inputs = RepoIdInputs {
            project_key_override: Some("ovr".into()),
            recorded: Some("rec".into()),
            aka_repo_ids: vec!["aka".into()],
            computed: Some("cmp".into()),
        };
        assert_eq!(resolve_repo_id(&inputs).as_deref(), Some("ovr"));

        // recorded beats aka + computed
        let inputs = RepoIdInputs {
            project_key_override: None,
            recorded: Some("rec".into()),
            aka_repo_ids: vec!["aka".into()],
            computed: Some("cmp".into()),
        };
        assert_eq!(resolve_repo_id(&inputs).as_deref(), Some("rec"));

        // aka beats computed when nothing recorded
        let inputs = RepoIdInputs {
            project_key_override: None,
            recorded: None,
            aka_repo_ids: vec!["aka".into()],
            computed: Some("cmp".into()),
        };
        assert_eq!(resolve_repo_id(&inputs).as_deref(), Some("aka"));

        // computed is the last resort
        let inputs = RepoIdInputs {
            project_key_override: None,
            recorded: None,
            aka_repo_ids: vec![],
            computed: Some("cmp".into()),
        };
        assert_eq!(resolve_repo_id(&inputs).as_deref(), Some("cmp"));

        // nothing → None
        assert_eq!(resolve_repo_id(&RepoIdInputs::default()), None);
    }

    #[test]
    fn resolve_skips_blank_sources() {
        let inputs = RepoIdInputs {
            project_key_override: Some("   ".into()),
            recorded: Some("".into()),
            aka_repo_ids: vec!["  ".into(), "real".into()],
            computed: None,
        };
        assert_eq!(resolve_repo_id(&inputs).as_deref(), Some("real"));
    }

    #[test]
    fn recorded_resolution_excludes_migration_hints() {
        let inputs = RepoIdInputs {
            project_key_override: None,
            recorded: None,
            aka_repo_ids: vec!["old".into()],
            computed: Some("weak".into()),
        };
        assert_eq!(resolve_recorded_repo_id(&inputs), None);

        let inputs = RepoIdInputs {
            project_key_override: Some("operator".into()),
            recorded: Some("recorded".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_recorded_repo_id(&inputs).as_deref(),
            Some("operator")
        );
    }

    #[test]
    fn repo_id_membership_matches_authority_and_aka() {
        let inputs = RepoIdInputs {
            project_key_override: None,
            recorded: Some("rec".into()),
            aka_repo_ids: vec!["old1".into(), "old2".into()],
            computed: Some("cmp".into()),
        };
        assert!(repo_id_matches("rec", &inputs));
        assert!(repo_id_matches("old1", &inputs));
        assert!(repo_id_matches("old2", &inputs));
        assert!(!repo_id_matches("cmp", &inputs)); // computed is not authority here
        assert!(!repo_id_matches("nope", &inputs));
        assert!(!repo_id_matches("", &inputs));
    }

    #[test]
    fn bbox_root_relpath_root_is_dot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(bbox_root_relpath(&root, &root).as_deref(), Some("."));
    }

    #[test]
    fn bbox_root_relpath_subproject() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sub = root.join("services").join("api");
        assert_eq!(
            bbox_root_relpath(&root, &sub).as_deref(),
            Some("services/api")
        );
    }

    #[test]
    fn bbox_root_relpath_outside_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other = other.path().canonicalize().unwrap();
        assert_eq!(bbox_root_relpath(&root, &other), None);
    }

    #[test]
    fn checkout_id_minted_once_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let first = ensure_checkout_id(&root).unwrap();
        assert_eq!(first.len(), 32);
        let second = ensure_checkout_id(&root).unwrap();
        assert_eq!(first, second, "checkout_id must be stable across calls");
        // Marker is under the gitignored local dir.
        let marker = root.join(".bbox").join("local").join("checkout-id");
        assert!(marker.exists());
        let gitignore = root.join(".bbox").join("local").join(".gitignore");
        assert!(gitignore.exists(), "local/.gitignore must be created");
    }

    #[test]
    fn checkout_id_fresh_after_marker_removed() {
        // A replacement checkout at the same path (marker gone) mints a FRESH
        // id — the reuse-safety invariant.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let first = ensure_checkout_id(&root).unwrap();
        std::fs::remove_file(root.join(".bbox").join("local").join("checkout-id")).unwrap();
        let second = ensure_checkout_id(&root).unwrap();
        assert_ne!(
            first, second,
            "a new checkout at the same path must not inherit the old id"
        );
    }

    #[test]
    fn checkout_id_remints_empty_or_partial_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let marker = root.join(".bbox").join("local").join("checkout-id");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();

        for torn in ["", "0123456789abcdef"] {
            std::fs::write(&marker, torn).unwrap();
            let id = ensure_checkout_id(&root).unwrap();
            assert!(is_checkout_id(&id));
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), id);
        }
    }

    #[cfg(unix)]
    #[test]
    fn checkout_id_rejects_symlinked_local_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.join(".bbox")).unwrap();
        symlink(outside.path(), root.join(".bbox/local")).unwrap();

        let error = ensure_checkout_id(&root).unwrap_err();
        assert!(error.to_string().contains("through symlink"));
        assert!(!outside.path().join("checkout-id").exists());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_id_rejects_symlinked_marker() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let local = root.join(".bbox/local");
        std::fs::create_dir_all(&local).unwrap();
        let target = outside.path().join("shared-id");
        std::fs::write(&target, "0123456789abcdef0123456789abcdef").unwrap();
        symlink(&target, local.join("checkout-id")).unwrap();

        let error = ensure_checkout_id(&root).unwrap_err();
        assert!(error.to_string().contains("through symlink"));
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn read_checkout_id_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(read_checkout_id(&root.join("nope")).unwrap(), None);
    }
}
