//! Host-local checkout registry (design §3.3).
//!
//! Slice 2a of
//! `design/corpus/knowledge/checkout-identity-and-provisional-knowledge.md`.
//!
//! The provisional-knowledge overlay serves cross-checkout visibility: "here is
//! what every checkout on this machine has in flight." To do that the daemon
//! must first ENUMERATE the checkouts that exist — a capability that does not
//! exist today (the watcher and knowledge loader see only registered base
//! roots). This registry is that census: a host-local index of the checkouts
//! that have written provisional state, kept honest over time.
//!
//! It is a discovery INDEX, not authority — the durable state lives in each
//! checkout's own `.bbox/` on disk, and a lost registry costs a recompute, not
//! the data (defect A). Because it names physical directories on THIS host, it
//! is host-local and never travels.
//!
//! Degradation is stated honestly (design §3.3):
//! - The DISCOVERABLE set (cockpit worktree roots + `git worktree list` of every
//!   registered repo, see [`discover_checkout_dirs`]) re-enumerates even if the
//!   registry file is lost, so it self-heals unconditionally.
//! - ARBITRARY-location marker clones are re-findable only via the registry; if
//!   it is also lost they re-register on their next provisional write.
//!
//! This slice ships the store and its pure lifecycle operations. Wiring it into
//! the live write path, startup, the reconciliation loop, and the watcher is
//! slice 2b. The write-gate re-verification is injected (not a marker check —
//! design finding 3) so the store stays unit-testable and the daemon supplies
//! the real conservative `resolve_project_context(.., Write)` gate at wiring
//! time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{atomic_write_json_locked, with_store_lock};

/// One registered checkout. Enough to recompute its overlay and to re-verify it
/// against the write gate; nothing that must travel with the repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRow {
    /// Logical project owner captured when the checkout was admitted. Older
    /// rows may lack it and are repaired on the next registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Durable, reuse-safe checkout identity (`.bbox/local/checkout-id`). One
    /// half of the composite registry key and this checkout's GC identity.
    pub checkout_id: String,
    /// Canonical top of the checkout on this host.
    pub checkout_dir: String,
    /// Durable repo-family id the checkout belongs to, when known at register
    /// time. `None` until minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// Monorepo discriminator: repo-relative path of the checkout's `.bbox`
    /// root. `None` for a non-monorepo / not-yet-resolved checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox_root_relpath: Option<String>,
    /// The branch the checkout was on at register time, for operator display
    /// and overlay labeling. Advisory (a checkout can switch branches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutRegistryStore {
    pub version: u32,
    #[serde(default)]
    pub checkouts: Vec<CheckoutRow>,
}

impl CheckoutRegistryStore {
    fn new() -> Self {
        Self {
            version: 2,
            checkouts: Vec::new(),
        }
    }
}

impl Default for CheckoutRegistryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn read_checkout_registry_store(path: &Path) -> Result<(CheckoutRegistryStore, bool)> {
    let mut store: CheckoutRegistryStore = if path.exists() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
    } else {
        CheckoutRegistryStore::new()
    };
    let needs_persist = match store.version {
        1 => {
            store
                .checkouts
                .retain(|row| row.published_scope().is_some());
            store.version = 2;
            true
        }
        2 => false,
        version => anyhow::bail!(
            "unsupported checkout registry version {version} in {}",
            path.display()
        ),
    };
    Ok((store, needs_persist))
}

/// Host-local checkout registry, persisted as JSON at `store_path`.
pub struct CheckoutRegistry {
    store: CheckoutRegistryStore,
    store_path: PathBuf,
    needs_persist: bool,
}

impl CheckoutRegistry {
    /// Open the registry, reading `store_path` if present or starting empty.
    pub fn open(store_path: &Path) -> Result<Self> {
        let (store, needs_persist) = read_checkout_registry_store(store_path)?;
        // Version 1 used checkout_id-only upsert semantics. Retain rows that
        // already carry the authority needed by the composite v2 key and drop
        // scope-less discovery hints that can never be addressed in v2. The
        // next successful mutation persists the upgraded shape.
        Ok(Self {
            store,
            store_path: store_path.to_path_buf(),
            needs_persist,
        })
    }

    /// Open this recoverable discovery index without making daemon startup
    /// depend on its host-local JSON bytes. The returned diagnostic preserves
    /// the failure for logging while reconciliation repopulates the empty
    /// index from discoverable checkouts. The corrupt file is left in place
    /// until a successful registry mutation atomically replaces it.
    pub fn open_recoverable(store_path: &Path) -> (Self, Option<anyhow::Error>) {
        match Self::open(store_path) {
            Ok(registry) => (registry, None),
            Err(error) => (
                Self {
                    store: CheckoutRegistryStore::new(),
                    store_path: store_path.to_path_buf(),
                    needs_persist: true,
                },
                Some(error),
            ),
        }
    }

    pub fn rows(&self) -> &[CheckoutRow] {
        &self.store.checkouts
    }

    pub fn get(&self, checkout_id: &str, scope: &PublishedScope) -> Option<&CheckoutRow> {
        self.store
            .checkouts
            .iter()
            .find(|row| row.matches(checkout_id, scope))
    }

    pub fn rows_for_checkout(&self, checkout_id: &str) -> impl Iterator<Item = &CheckoutRow> {
        self.store
            .checkouts
            .iter()
            .filter(move |row| row.checkout_id == checkout_id)
    }

    fn mutate_store<T>(
        &mut self,
        mutate: impl FnOnce(&mut CheckoutRegistryStore) -> Result<(T, bool)>,
    ) -> Result<T> {
        let path = self.store_path.clone();
        let recovery_fallback = self.store.clone();
        let may_replace_corrupt = self.needs_persist;
        let (next, output) = with_store_lock(&path, || {
            let (mut next, needs_persist) = match read_checkout_registry_store(&path) {
                Ok(current) => current,
                Err(_) if may_replace_corrupt => (recovery_fallback, true),
                Err(error) => return Err(error),
            };
            let (output, changed) = mutate(&mut next)?;
            if changed || needs_persist {
                atomic_write_json_locked(&path, &next)?;
                sync_parent_directory(&path)?;
            }
            Ok((next, output))
        })?;
        self.store = next;
        self.needs_persist = false;
        Ok(output)
    }

    /// Register or update one `(checkout_id, published_scope)` row. A monorepo
    /// checkout may carry several rows without one subproject replacing another.
    pub fn register(&mut self, row: CheckoutRow) -> Result<()> {
        let scope = row.published_scope().with_context(|| {
            format!(
                "checkout {} has no recorded repo_id/bbox_root_relpath authority",
                row.checkout_id
            )
        })?;
        self.mutate_store(move |next| {
            if let Some(existing) = next
                .checkouts
                .iter_mut()
                .find(|existing| existing.matches(&row.checkout_id, &scope))
            {
                if *existing == row {
                    return Ok(((), false));
                }
                *existing = row;
            } else {
                next.checkouts.push(row);
            }
            Ok(((), true))
        })
    }

    /// Remove one scope from a checkout while retaining sibling monorepo
    /// scopes. Returns whether a row was removed.
    pub fn deregister_scope(&mut self, checkout_id: &str, scope: &PublishedScope) -> Result<bool> {
        self.mutate_store(|next| {
            let before = next.checkouts.len();
            next.checkouts
                .retain(|row| !row.matches(checkout_id, scope));
            let removed = next.checkouts.len() != before;
            Ok((removed, removed))
        })
    }

    /// Explicit teardown deregistration by checkout id. Returns whether a row
    /// was removed. Persists when it removes.
    pub fn deregister(&mut self, checkout_id: &str) -> Result<bool> {
        self.mutate_store(|next| {
            let before = next.checkouts.len();
            next.checkouts.retain(|r| r.checkout_id != checkout_id);
            let removed = next.checkouts.len() != before;
            Ok((removed, removed))
        })
    }

    /// Reconcile the registry against ground truth: drop every row whose
    /// directory is gone OR no longer passes `still_valid` — the injected
    /// conservative WRITE gate (design finding 3: re-run the gate, do NOT test
    /// for a marker file, because managed checkouts are recognized structurally
    /// or by location, not only by marker). This is the startup reload check and
    /// the periodic reconciliation body. Returns the dropped rows. Persists when
    /// it drops anything.
    pub fn reconcile(
        &mut self,
        still_valid: impl Fn(&CheckoutRow) -> bool,
    ) -> Result<Vec<CheckoutRow>> {
        self.mutate_store(|next| {
            let mut kept = Vec::with_capacity(next.checkouts.len());
            let mut dropped = Vec::new();
            for row in next.checkouts.iter().cloned() {
                let dir = Path::new(&row.checkout_dir);
                if dir.exists() && still_valid(&row) {
                    kept.push(row);
                } else {
                    dropped.push(row);
                }
            }
            let changed = !dropped.is_empty();
            next.checkouts = kept;
            Ok((dropped, changed))
        })
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)
        .with_context(|| format!("opening {} for fsync", parent.display()))?
        .sync_all()
        .with_context(|| format!("fsync directory {}", parent.display()))
}

impl CheckoutRow {
    pub fn published_scope(&self) -> Option<PublishedScope> {
        PublishedScope::try_new(
            self.repo_id.as_deref()?.trim(),
            self.bbox_root_relpath.as_deref()?.trim(),
        )
        .ok()
    }

    fn matches(&self, checkout_id: &str, scope: &PublishedScope) -> bool {
        self.checkout_id == checkout_id && self.published_scope().as_ref() == Some(scope)
    }
}

/// Discover candidate checkout directories that are re-findable WITHOUT the
/// registry (design §3.3, the self-healing DISCOVERABLE set):
///
/// - the cockpit-managed worktree roots (`$BRO_HOME/{fleet,agent}/worktrees`)
///   and their immediate children, and
/// - every worktree of every registered git repo, via `git worktree list`, and
/// - each registered non-git root itself.
///
/// Returns canonical, de-duplicated directories. This does NOT cover
/// arbitrary-location marker clones — those are recoverable only through the
/// registry (or they re-register on next write). The caller enriches each
/// discovered dir into a [`CheckoutRow`] (its `checkout_id`, `repo_id`, etc.)
/// when it registers; discovery yields locations only.
pub struct CheckoutDiscoveryAccess<'a> {
    pub checkout_root: &'a Path,
    pub project_root: &'a Path,
    pub is_git_repo: bool,
}

pub fn discover_checkout_dirs(projects: &[CheckoutDiscoveryAccess<'_>]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let push = |p: PathBuf, found: &mut Vec<PathBuf>| {
        if !found.contains(&p) {
            found.push(p);
        }
    };

    // Cockpit worktree parents: each immediate child is a dispatch worktree.
    for parent in bbox_util::util::cockpit_managed_worktree_roots() {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(canonical) = std::fs::canonicalize(entry.path()) {
                if canonical.is_dir() {
                    push(canonical, &mut found);
                }
            }
        }
    }

    // Every worktree of every registered repo.
    for project in projects {
        if !project.is_git_repo {
            push(project.project_root.to_path_buf(), &mut found);
            continue;
        }
        for wt in bbox_corpus_core::git::list_worktree_paths(project.checkout_root) {
            push(wt, &mut found);
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, dir: &Path) -> CheckoutRow {
        CheckoutRow {
            project_id: None,
            checkout_id: id.into(),
            checkout_dir: dir.to_string_lossy().into_owned(),
            repo_id: Some("repofam".into()),
            bbox_root_relpath: Some(".".into()),
            branch_ref: Some("feature/x".into()),
        }
    }

    fn root_scope() -> PublishedScope {
        PublishedScope::try_new("repofam", ".").unwrap()
    }

    #[test]
    fn register_is_idempotent_upsert() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();

        let mut reg = CheckoutRegistry::open(&path).unwrap();
        reg.register(row("c1", &d)).unwrap();
        // Re-register same id with a changed branch: replaces in place.
        let mut updated = row("c1", &d);
        updated.branch_ref = Some("feature/y".into());
        reg.register(updated).unwrap();

        assert_eq!(reg.rows().len(), 1);
        assert_eq!(
            reg.get("c1", &root_scope()).unwrap().branch_ref.as_deref(),
            Some("feature/y")
        );
    }

    #[cfg(unix)]
    #[test]
    fn identical_registration_does_not_rewrite_registry_file() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().canonicalize().unwrap();
        let row = row("c1", &checkout);
        let mut registry = CheckoutRegistry::open(&path).unwrap();
        registry.register(row.clone()).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();

        registry.register(row).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    }

    #[test]
    fn register_persists_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();

        let mut reg = CheckoutRegistry::open(&path).unwrap();
        reg.register(row("c1", &d)).unwrap();
        drop(reg);

        let reg2 = CheckoutRegistry::open(&path).unwrap();
        assert_eq!(reg2.rows().len(), 1);
        assert_eq!(
            reg2.get("c1", &root_scope()).unwrap().checkout_dir,
            d.to_str().unwrap()
        );
    }

    #[test]
    fn stale_registry_handles_merge_rows_under_the_canonical_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first_path = first_dir.path().canonicalize().unwrap();
        let second_path = second_dir.path().canonicalize().unwrap();
        let mut first = CheckoutRegistry::open(&path).unwrap();
        let mut stale = CheckoutRegistry::open(&path).unwrap();

        first.register(row("c1", &first_path)).unwrap();
        stale.register(row("c2", &second_path)).unwrap();

        let reopened = CheckoutRegistry::open(&path).unwrap();
        assert!(reopened.get("c1", &root_scope()).is_some());
        assert!(reopened.get("c2", &root_scope()).is_some());
    }

    #[test]
    fn deregister_removes_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();

        let mut reg = CheckoutRegistry::open(&path).unwrap();
        reg.register(row("c1", &d)).unwrap();
        assert!(reg.deregister("c1").unwrap());
        assert!(!reg.deregister("c1").unwrap()); // already gone
        assert!(reg.rows().is_empty());
    }

    #[test]
    fn reconcile_drops_gone_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let live = tempfile::tempdir().unwrap();
        let live_d = live.path().canonicalize().unwrap();
        let gone = tempfile::tempdir().unwrap();
        let gone_d = gone.path().canonicalize().unwrap();

        let mut reg = CheckoutRegistry::open(&path).unwrap();
        reg.register(row("live", &live_d)).unwrap();
        reg.register(row("gone", &gone_d)).unwrap();
        drop(gone); // directory removed

        // Verifier passes everything; only the missing dir is dropped.
        let dropped = reg.reconcile(|_| true).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].checkout_id, "gone");
        assert_eq!(reg.rows().len(), 1);
        assert_eq!(reg.rows()[0].checkout_id, "live");
    }

    #[test]
    fn reconcile_drops_gate_rejected_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let keep = tempfile::tempdir().unwrap();
        let keep_d = keep.path().canonicalize().unwrap();
        let reject = tempfile::tempdir().unwrap();
        let reject_d = reject.path().canonicalize().unwrap();

        let mut reg = CheckoutRegistry::open(&path).unwrap();
        reg.register(row("keep", &keep_d)).unwrap();
        reg.register(row("reject", &reject_d)).unwrap();

        // Both dirs exist; the injected gate rejects one (no longer a managed
        // checkout) — that row is dropped even though its dir is present.
        let reject_str = reject_d.to_string_lossy().into_owned();
        let dropped = reg.reconcile(|row| row.checkout_dir != reject_str).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].checkout_id, "reject");
        assert_eq!(reg.rows().len(), 1);
    }

    #[test]
    fn reconcile_persists_drops_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let gone = tempfile::tempdir().unwrap();
        let gone_d = gone.path().canonicalize().unwrap();

        let mut reg = CheckoutRegistry::open(&path).unwrap();
        reg.register(row("gone", &gone_d)).unwrap();
        drop(gone);
        reg.reconcile(|_| true).unwrap();
        drop(reg);

        let reg2 = CheckoutRegistry::open(&path).unwrap();
        assert!(
            reg2.rows().is_empty(),
            "reconcile drop must be persisted, not just in-memory"
        );
    }

    #[test]
    fn one_checkout_keeps_distinct_monorepo_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().canonicalize().unwrap();
        let mut api = row("c1", &checkout);
        api.bbox_root_relpath = Some("services/api".into());
        let mut web = row("c1", &checkout);
        web.bbox_root_relpath = Some("services/web".into());

        let mut registry = CheckoutRegistry::open(&path).unwrap();
        registry.register(api).unwrap();
        registry.register(web).unwrap();
        assert_eq!(registry.rows_for_checkout("c1").count(), 2);

        let api_scope = PublishedScope::try_new("repofam", "services/api").unwrap();
        let web_scope = PublishedScope::try_new("repofam", "services/web").unwrap();
        assert!(registry.get("c1", &api_scope).is_some());
        assert!(registry.get("c1", &web_scope).is_some());
        assert!(registry.deregister_scope("c1", &api_scope).unwrap());
        assert!(registry.get("c1", &api_scope).is_none());
        assert!(registry.get("c1", &web_scope).is_some());
    }

    #[test]
    fn version_one_store_upgrades_without_collapsing_new_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().canonicalize().unwrap();
        let legacy = CheckoutRegistryStore {
            version: 1,
            checkouts: vec![row("c1", &checkout)],
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let mut registry = CheckoutRegistry::open(&path).unwrap();
        let mut second = row("c1", &checkout);
        second.bbox_root_relpath = Some("services/api".into());
        registry.register(second).unwrap();
        let persisted: CheckoutRegistryStore =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.version, 2);
        assert_eq!(persisted.checkouts.len(), 2);
    }

    #[test]
    fn version_one_upgrade_drops_scope_less_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().canonicalize().unwrap();
        let mut unresolved = row("old", &checkout);
        unresolved.repo_id = None;
        unresolved.bbox_root_relpath = None;
        let legacy = CheckoutRegistryStore {
            version: 1,
            checkouts: vec![unresolved, row("kept", &checkout)],
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let registry = CheckoutRegistry::open(&path).unwrap();
        assert_eq!(registry.rows().len(), 1);
        assert_eq!(registry.rows()[0].checkout_id, "kept");
    }

    #[test]
    fn failed_register_does_not_change_in_memory_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked_parent = tmp.path().join("not-a-directory");
        let path = blocked_parent.join("checkouts.json");
        let mut registry = CheckoutRegistry::open(&path).unwrap();
        std::fs::write(&blocked_parent, "blocked").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().canonicalize().unwrap();

        assert!(registry.register(row("c1", &checkout)).is_err());
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn recoverable_open_degrades_corrupt_json_to_empty_and_replaces_on_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkouts.json");
        std::fs::write(&path, b"{\"version\":").unwrap();

        let (mut registry, diagnostic) = CheckoutRegistry::open_recoverable(&path);
        assert!(diagnostic.is_some());
        assert!(registry.rows().is_empty());

        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().canonicalize().unwrap();
        registry.register(row("recovered", &checkout)).unwrap();

        let reopened = CheckoutRegistry::open(&path).unwrap();
        assert_eq!(reopened.rows().len(), 1);
        assert_eq!(reopened.rows()[0].checkout_id, "recovered");
    }

    #[test]
    fn discover_lists_repo_worktrees() {
        // A real repo with a linked worktree is discoverable via git.
        let base = tempfile::tempdir().unwrap();
        let base_d = base.path().canonicalize().unwrap();
        let run = |dir: &Path, args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        run(&base_d, &["init", "-q"]);
        run(&base_d, &["config", "user.email", "t@example.com"]);
        run(&base_d, &["config", "user.name", "Test"]);
        std::fs::write(base_d.join("f.txt"), "x").unwrap();
        run(&base_d, &["add", "."]);
        run(&base_d, &["commit", "-q", "-m", "seed"]);
        let wt = base.path().parent().unwrap().join("linked-wt");
        run(
            &base_d,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
        );
        let wt_d = wt.canonicalize().unwrap();

        let projects = vec![CheckoutDiscoveryAccess {
            checkout_root: &base_d,
            project_root: &base_d,
            is_git_repo: true,
        }];
        let found = discover_checkout_dirs(&projects);
        assert!(
            found.contains(&base_d),
            "primary checkout discovered: {found:?}"
        );
        assert!(
            found.contains(&wt_d),
            "linked worktree discovered: {found:?}"
        );

        // Cleanup the worktree so the tempdir drop is clean.
        run(
            &base_d,
            &["worktree", "remove", "--force", wt_d.to_str().unwrap()],
        );
    }

    #[test]
    fn discover_includes_registered_non_git_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plain-project");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let project = CheckoutDiscoveryAccess {
            checkout_root: &root,
            project_root: &root,
            is_git_repo: false,
        };

        let discovered = discover_checkout_dirs(&[project]);

        assert!(discovered.contains(&root));
    }
}
