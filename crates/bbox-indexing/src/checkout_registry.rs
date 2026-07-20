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

use bbox_corpus_core::json_store::atomic_write_json_locked;
use bbox_corpus_core::project_record::ProjectRecord;

/// One registered checkout. Enough to recompute its overlay and to re-verify it
/// against the write gate; nothing that must travel with the repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRow {
    /// Durable, reuse-safe checkout identity (`.bbox/local/checkout-id`). The
    /// primary key of the overlay and this registry's GC identity.
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
            version: 1,
            checkouts: Vec::new(),
        }
    }
}

impl Default for CheckoutRegistryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Host-local checkout registry, persisted as JSON at `store_path`.
pub struct CheckoutRegistry {
    store: CheckoutRegistryStore,
    store_path: PathBuf,
}

impl CheckoutRegistry {
    /// Open the registry, reading `store_path` if present or starting empty.
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = std::fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            CheckoutRegistryStore::new()
        };
        Ok(Self {
            store,
            store_path: store_path.to_path_buf(),
        })
    }

    pub fn rows(&self) -> &[CheckoutRow] {
        &self.store.checkouts
    }

    pub fn get(&self, checkout_id: &str) -> Option<&CheckoutRow> {
        self.store
            .checkouts
            .iter()
            .find(|r| r.checkout_id == checkout_id)
    }

    fn save(&self) -> Result<()> {
        atomic_write_json_locked(&self.store_path, &self.store)
    }

    /// Register (or update) a checkout, keyed by `checkout_id`. Idempotent: a
    /// second write from the same checkout replaces the row in place (a checkout
    /// can move dir or switch branch), so the registry never accretes duplicate
    /// rows for one checkout. Persists.
    pub fn register(&mut self, row: CheckoutRow) -> Result<()> {
        if let Some(existing) = self
            .store
            .checkouts
            .iter_mut()
            .find(|r| r.checkout_id == row.checkout_id)
        {
            *existing = row;
        } else {
            self.store.checkouts.push(row);
        }
        self.save()
    }

    /// Explicit teardown deregistration by checkout id. Returns whether a row
    /// was removed. Persists when it removes.
    pub fn deregister(&mut self, checkout_id: &str) -> Result<bool> {
        let before = self.store.checkouts.len();
        self.store.checkouts.retain(|r| r.checkout_id != checkout_id);
        let removed = self.store.checkouts.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Reconcile the registry against ground truth: drop every row whose
    /// directory is gone OR no longer passes `still_valid` — the injected
    /// conservative WRITE gate (design finding 3: re-run the gate, do NOT test
    /// for a marker file, because managed checkouts are recognized structurally
    /// or by location, not only by marker). This is the startup reload check and
    /// the periodic reconciliation body. Returns the dropped rows. Persists when
    /// it drops anything.
    pub fn reconcile(&mut self, still_valid: impl Fn(&Path) -> bool) -> Result<Vec<CheckoutRow>> {
        let mut kept = Vec::with_capacity(self.store.checkouts.len());
        let mut dropped = Vec::new();
        for row in std::mem::take(&mut self.store.checkouts) {
            let dir = Path::new(&row.checkout_dir);
            if dir.exists() && still_valid(dir) {
                kept.push(row);
            } else {
                dropped.push(row);
            }
        }
        self.store.checkouts = kept;
        if !dropped.is_empty() {
            self.save()?;
        }
        Ok(dropped)
    }
}

/// Discover candidate checkout directories that are re-findable WITHOUT the
/// registry (design §3.3, the self-healing DISCOVERABLE set):
///
/// - the cockpit-managed worktree roots (`$BRO_HOME/{fleet,agent}/worktrees`)
///   and their immediate children, and
/// - every worktree of every registered repo, via `git worktree list`.
///
/// Returns canonical, de-duplicated directories. This does NOT cover
/// arbitrary-location marker clones — those are recoverable only through the
/// registry (or they re-register on next write). The caller enriches each
/// discovered dir into a [`CheckoutRow`] (its `checkout_id`, `repo_id`, etc.)
/// when it registers; discovery yields locations only.
pub fn discover_checkout_dirs(projects: &[ProjectRecord]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf, found: &mut Vec<PathBuf>| {
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
        for wt in bbox_corpus_core::git::list_worktree_paths(Path::new(&project.canonical_path)) {
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
            checkout_id: id.into(),
            checkout_dir: dir.to_string_lossy().into_owned(),
            repo_id: Some("repofam".into()),
            bbox_root_relpath: Some(".".into()),
            branch_ref: Some("feature/x".into()),
        }
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
            reg.get("c1").unwrap().branch_ref.as_deref(),
            Some("feature/y")
        );
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
        assert_eq!(reg2.get("c1").unwrap().checkout_dir, d.to_str().unwrap());
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
        let dropped = reg
            .reconcile(|dir| dir.to_string_lossy() != reject_str)
            .unwrap();
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

        let projects = vec![ProjectRecord {
            project_id: "p1".into(),
            repo_id: None,
            canonical_path: base_d.to_string_lossy().into_owned(),
            registered_at: "2026-01-01".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        }];
        let found = discover_checkout_dirs(&projects);
        assert!(
            found.contains(&base_d),
            "primary checkout discovered: {found:?}"
        );
        assert!(found.contains(&wt_d), "linked worktree discovered: {found:?}");

        // Cleanup the worktree so the tempdir drop is clean.
        run(&base_d, &["worktree", "remove", "--force", wt_d.to_str().unwrap()]);
    }
}
