//! Bridge parity harness (Phase 5 plan sections 11 and 14.4).
//!
//! Section 11 is a freeze, not a guideline: while the version-1 registry is
//! the runtime authority, every legacy surface it lists keeps its EXACT
//! response. The only sanctioned bridge-visible additions are dormant code,
//! the catalog-inactive refusal from the two new catalog-only tools, and
//! empty catalog runtime state that is not serialized into an existing
//! response. Anything else needs a new explicit decision.
//!
//! So the proof is a replay, not a set of hand-written expectations: one
//! canonical bridge fixture is driven through every listed surface, each
//! full response is captured, and the whole capture is compared byte for
//! byte against committed bytes. A hand-written assertion proves only what
//! its author thought to assert; a committed capture fails on ANY field,
//! ordering, or wording change, including one arriving through a shared
//! type (Risk 18).
//!
//! # Determinism, and why the normalization list is short
//!
//! A parity harness goes vacuous the moment it normalizes generously: sweep
//! a timestamp regex over the capture and a real timestamp change stops
//! failing too. The rule here is that determinism is bought at the SOURCE
//! wherever it can be, and what is left over is substituted by EXACT VALUE,
//! never by pattern.
//!
//! Bought at the source:
//! - Git identity and both dates are pinned, so every commit SHA in the
//!   capture is a fixed constant. That also pins `repo_id`, which is the
//!   hash of the repository's first commit.
//! - The checkout identity marker is pre-written rather than minted, so the
//!   random `checkout_id` becomes a constant, and with it every overlay key
//!   and provisional entity ref derived from it.
//!
//! Left over, and substituted by exact value:
//! - the fixture root, because it is a per-run temporary directory;
//! - the version-1 registry `project_id`, because it is the hash of that
//!   per-run path;
//! - `registered_at`, because the registry stamps wall-clock time;
//! - the overlay working fingerprint, because it is derived from filesystem
//!   metadata that no fixture controls.
//!
//! Each row DECLARES which of those four it expects. Substitution is
//! self-policing in both directions: a declared substitution that never
//! fires fails as vacuous, and an undeclared one that does fire fails as an
//! unaudited normalization. That is what keeps the list from quietly
//! growing into a regex sweep.
//!
//! # Doctor is a structural row, and says so
//!
//! One surface is captured structurally rather than verbatim: doctor
//! findings embed host-global state (daemon version, index byte counts,
//! store paths outside the fixture) that neither the bridge nor the catalog
//! controls, so a verbatim capture would be host-dependent - flaky, not
//! loud, which is worse than no row at all. The doctor row therefore
//! captures the section inventory in order, each section's finding COUNT,
//! and each finding's level and suggested-next command. A new, dropped, or
//! reclassified finding still fails; only the free-text body is out.

//! The whole harness lives inside one `#[cfg(test)]` module rather than
//! behind a file-level `#![cfg(test)]`. The clause 2 Proof B ownership
//! ratchet exempts test modules by truncating each file at its first
//! `#[cfg(test)]` line, and the inner-attribute form does not match that
//! pattern - a file-level gate would have this harness's deliberate
//! capture of the frozen bridge watcher carrier counted as a new runtime
//! occurrence.

#[cfg(test)]
mod harness {

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_record::ResolvedCheckoutScope;
    use bbox_gaps::gaps::{
        BlockingLevel, GapImpact, GapKind, GapListParams, GapNote, GapResolution,
        committed_gap_note_bytes,
    };
    use bbox_knowledge::knowledge::{
        Approval, Category, KnowledgeEntry, KnowledgeListParams, Priority, RenderParams, Scope,
        Status, committed_knowledge_entry_bytes,
    };
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::CallToolResult;
    use serde_json::{Value, json};

    use crate::server::{BlackboxServer, SharedState};

    /// Committed canonical bytes, relative to the repository root.
    const FIXTURE_RELPATH: &str = "tests/fixtures/bridge-parity/bridge-parity.json";

    // ---------------------------------------------------------------------------
    // Pinned fixture identity
    // ---------------------------------------------------------------------------

    /// Fixed Git identity and dates. Every commit SHA in the capture is a
    /// consequence of these plus the committed tree, so pinning them removes
    /// the single largest source of run-to-run drift instead of normalizing it
    /// away afterwards.
    const GIT_NAME: &str = "Bridge Parity";
    const GIT_EMAIL: &str = "bridge-parity@example.invalid";
    const GIT_DATE: &str = "2026-01-01T00:00:00+0000";

    /// Pre-written so the checkout identity is a constant rather than
    /// `random_hex()`. Overlay keys, provisional entity refs, and every
    /// snapshot id are derived from it.
    const OWN_CHECKOUT_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb01";
    const PEER_CHECKOUT_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02";

    // ---------------------------------------------------------------------------
    // Declared normalizations
    // ---------------------------------------------------------------------------

    /// The four values that cannot be pinned at the source.
    ///
    /// Every one is substituted by its EXACT captured value, never by pattern,
    /// so a value the fixture does not know about cannot be swallowed: it
    /// survives into the capture and fails the byte comparison.
    #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
    enum Normalization {
        /// The per-run temporary directory holding the fixture.
        FixtureRoot,
        /// The version-1 registry project id: the 8-hex hash of the canonical
        /// fixture path, so nondeterministic for the same reason the root is.
        RegistryProjectId,
        /// `registered_at`, stamped from wall-clock time at registration.
        RegisteredAt,
        /// The overlay working fingerprint, derived from filesystem metadata.
        WorkingFingerprint,
        /// The provenance export plan's `generation`: a SHA-256 over the
        /// project id, the notes ref, and the document set. The document set
        /// and the notes ref are pinned, so what varies is the project id -
        /// the same per-run path hash, one indirection further on.
        PlanGeneration,
        /// `last_unix_secs` / `last_success_unix_secs` on a checkout
        /// observation: the wall-clock second a counter last moved. The
        /// counter key-space, the exact counts, and the sequences are all
        /// captured verbatim; only the clock reading is substituted.
        ObservationWallClock,
        /// The daemon version in doctor's `daemon` section. It is
        /// `CARGO_PKG_VERSION`, so it moves on every release with no bridge
        /// behavior having changed.
        DaemonVersion,
        /// `cfg.paths.state_dir` in doctor's `daemon` section: the HOST's
        /// configured state directory, which sits outside the fixture root
        /// and differs per machine and per lane. It must not reach the
        /// committed bytes for a second reason beyond determinism: it
        /// carries the operator's home path, and this is a public repo.
        HostStateDir,
    }

    impl Normalization {
        fn placeholder(self) -> &'static str {
            match self {
                Self::FixtureRoot => "<FIXTURE_ROOT>",
                Self::RegistryProjectId => "<REGISTRY_PROJECT_ID>",
                Self::RegisteredAt => "<REGISTERED_AT>",
                Self::WorkingFingerprint => "<WORKING_FINGERPRINT>",
                Self::PlanGeneration => "<PLAN_GENERATION>",
                Self::ObservationWallClock => "<OBSERVED_AT_UNIX_SECS>",
                Self::DaemonVersion => "blackboxd <DAEMON_VERSION>",
                Self::HostStateDir => "<HOST_STATE_DIR>",
            }
        }

        /// The justification, carried in the capture itself so a reader of the
        /// committed bytes sees why a placeholder is there without reading this
        /// file.
        fn justification(self) -> &'static str {
            match self {
                Self::FixtureRoot => "per-run temporary directory",
                Self::RegistryProjectId => "hash of the per-run canonical fixture path",
                Self::RegisteredAt => "registry stamps wall-clock time",
                Self::WorkingFingerprint => "derived from filesystem metadata",
                Self::PlanGeneration => "digest over the per-run registry project id",
                Self::ObservationWallClock => "wall-clock second a counter last moved",
                Self::DaemonVersion => "CARGO_PKG_VERSION, moves every release",
                Self::HostStateDir => "host state directory outside the fixture",
            }
        }
    }

    // ---------------------------------------------------------------------------
    // The canonical bridge fixture
    // ---------------------------------------------------------------------------

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", GIT_NAME)
            .env("GIT_AUTHOR_EMAIL", GIT_EMAIL)
            .env("GIT_AUTHOR_DATE", GIT_DATE)
            .env("GIT_COMMITTER_NAME", GIT_NAME)
            .env("GIT_COMMITTER_EMAIL", GIT_EMAIL)
            .env("GIT_COMMITTER_DATE", GIT_DATE)
            // A host `~/.gitconfig` must not reach the fixture: a template
            // directory, a commit hook, or `commit.gpgsign` would change the
            // committed tree or the commit object and move every SHA.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_knowledge(root: &Path, entry: &KnowledgeEntry) {
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        // Single owner (`committed_knowledge_entry_bytes`). A second encoder
        // here is the incident class that made an earlier suite vacuously
        // green: the fixture and the writers agreed with each other and both
        // disagreed with production.
        std::fs::write(
            dir.join(format!("{}.json", entry.id)),
            committed_knowledge_entry_bytes(entry).unwrap(),
        )
        .unwrap();
    }

    fn write_gap(root: &Path, gap: &GapNote) {
        let dir = root.join(".bbox/gaps");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", gap.id)),
            committed_gap_note_bytes(gap).unwrap(),
        )
        .unwrap();
    }

    fn knowledge_entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.to_string(),
            title: format!("entry {id}"),
            content: content.to_string(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn gap_note(id: &str, title: &str) -> GapNote {
        GapNote {
            id: id.to_string(),
            title: title.to_string(),
            gap_kind: GapKind::Tooling,
            domain: "bridge-parity".to_string(),
            wanted_capability: "serve published gaps on the version-1 bridge".to_string(),
            missing_primitive: None,
            fallback_used: None,
            evidence: Vec::new(),
            impact: GapImpact::Medium,
            blocking_level: BlockingLevel::WorkaroundAvailable,
            dedupe_key: "tooling/bridge-parity/published".to_string(),
            suggested_owner: None,
            notes: None,
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project: None,
            project_id: None,
            write_dir: None,
            provisional_checkout_id: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    /// One published repository, one peer worktree, one registered version-1
    /// project, and a bridge-mode server over them.
    struct BridgeFixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        base: PathBuf,
        peer: PathBuf,
        scope: PublishedScope,
        registry_project_id: String,
        registered_at: String,
        server: BlackboxServer,
    }

    impl BridgeFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let base = root.join("base");
            std::fs::create_dir_all(&base).unwrap();

            git(&base, &["init", "-q", "-b", "main"]);
            std::fs::write(base.join("README.md"), "bridge parity fixture\n").unwrap();
            git(&base, &["add", "README.md"]);
            git(&base, &["commit", "-q", "-m", "seed"]);

            // `repo_id` is the hash of the first commit, so it is pinned by the
            // pinned Git identity above and needs no substitution.
            let repo_id = bbox_config::config::ensure_recorded_repo_id(&base)
                .unwrap()
                .repo_id;

            write_knowledge(&base, &knowledge_entry("bp-shared", "PUBLISHED_CONTENT"));
            write_knowledge(&base, &knowledge_entry("bp-deleted", "PUBLISHED_DELETED"));
            write_gap(&base, &gap_note("gap-bb000001", "published gap"));
            git(&base, &["add", ".bbox"]);
            git(&base, &["commit", "-q", "-m", "published lanes"]);

            let peer = root.join("peer");
            git(
                &base,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "peer",
                    peer.to_str().unwrap(),
                ],
            );

            // Pre-write both checkout identity markers so no `random_hex()`
            // reaches the capture.
            pin_checkout_id(&base, OWN_CHECKOUT_ID);
            pin_checkout_id(&peer, PEER_CHECKOUT_ID);

            let state_dir = root.join("state");
            std::fs::create_dir_all(&state_dir).unwrap();
            let state = Arc::new(SharedState::for_test(&state_dir));
            assert!(
                state.project_authority.is_bridge(),
                "the parity fixture must run against the version-1 bridge"
            );
            let record = state
                .project_authority
                .bridge_registry()
                .unwrap()
                .write()
                .register_path(&base)
                .unwrap();
            let server = BlackboxServer::new(state);
            let scope = PublishedScope::try_new(repo_id, ".").unwrap();

            Self {
                _temp: temp,
                root,
                base,
                peer,
                scope,
                registry_project_id: record.project_id.clone(),
                registered_at: record.registered_at.clone(),
                server,
            }
        }

        /// Dirty both checkouts and drive the bridge overlay recompute for each,
        /// so Own and All have real provisional content rather than a
        /// hand-published snapshot.
        fn recompute_overlays(&self) {
            write_knowledge(&self.base, &knowledge_entry("bp-shared", "OWN_CONTENT"));
            std::fs::remove_file(self.base.join(".bbox/knowledge/bp-deleted.json")).unwrap();
            write_gap(&self.base, &gap_note("gap-bb000001", "own gap title"));
            write_knowledge(&self.peer, &knowledge_entry("bp-shared", "PEER_CONTENT"));

            let own = self.checkout(&self.base, OWN_CHECKOUT_ID);
            let peer = self.checkout(&self.peer, PEER_CHECKOUT_ID);
            for checkout in [&own, &peer] {
                self.server
                    .register_dark_knowledge_checkout(checkout)
                    .unwrap();
                self.server.refresh_dark_knowledge_overlay(checkout);
                self.server.refresh_dark_gap_overlay(checkout);
            }
            self.server.set_session_checkout_for_test(
                self.registry_project_id.clone(),
                self.scope.clone(),
                OWN_CHECKOUT_ID.to_string(),
                self.base.clone(),
            );
        }

        /// Force the next publisher authorization to be a cold miss.
        ///
        /// This is what makes the observation counters EXACT rather than
        /// projected. The authorization cache has a 250 millisecond TTL, so
        /// whether a given call takes a lease depended on how fast the machine
        /// was: the same replay produced different counts under load. Dropping
        /// the cache entry before every captured row removes the timer from the
        /// measurement entirely, so the replay always takes its full cold-path
        /// lease count.
        ///
        /// It also captures the more interesting path. A warm run measures how
        /// often the cache happened to hit; a cold run measures what the bridge
        /// actually does when it has to resolve authority.
        fn cold_authorization(&self) {
            self.server
                .invalidate_publisher_authority_cache(&self.scope);
        }

        fn checkout(&self, dir: &Path, checkout_id: &str) -> ResolvedCheckoutScope {
            ResolvedCheckoutScope {
                project_id: self.registry_project_id.clone(),
                published_scope: self.scope.clone(),
                checkout_id: checkout_id.to_string(),
                checkout_dir: dir.to_string_lossy().into_owned(),
                checkout_project_dir: dir.to_string_lossy().into_owned(),
                branch_ref: bbox_corpus_core::git::current_branch(dir)
                    .map(|branch| format!("refs/heads/{branch}")),
            }
        }

        /// The exact values behind each declared normalization.
        ///
        /// Working fingerprints are read back from the overlay stores rather
        /// than guessed, so the substitution is exactly what the recompute
        /// produced and cannot silently cover a different value.
        fn substitutions(&self) -> Vec<Substitution> {
            let mut substitutions = vec![
                substitution(
                    Normalization::FixtureRoot,
                    self.root.to_string_lossy().into_owned(),
                    None,
                ),
                substitution(
                    Normalization::RegistryProjectId,
                    self.registry_project_id.clone(),
                    None,
                ),
                substitution(
                    Normalization::RegisteredAt,
                    self.registered_at.clone(),
                    None,
                ),
                // Read from the SAME sources doctor renders them from, not
                // parsed back out of the captured text: a substitution
                // derived from the capture would agree with whatever the
                // capture happened to say.
                // ANCHORED to the rendered prefix, not the bare version.
                // A bare "0.0.1" also matches inside the bind address
                // 127.0.0.1, and the first regenerated capture duly read
                // "127.<DAEMON_VERSION>". Short, low-entropy values have to
                // carry enough surrounding context to be unambiguous; the
                // guard below catches the general case.
                substitution(
                    Normalization::DaemonVersion,
                    format!("blackboxd {}", env!("CARGO_PKG_VERSION")),
                    None,
                ),
                substitution(
                    Normalization::HostStateDir,
                    self.server
                        .state
                        .config
                        .read()
                        .paths
                        .state_dir
                        .display()
                        .to_string(),
                    None,
                ),
            ];
            let mut fingerprints = BTreeMap::new();
            for snapshot in self.server.state.knowledge_overlays.read().snapshots() {
                if let Some(stamp) = &snapshot.stamp {
                    fingerprints
                        .insert(stamp.checkout_id.clone(), stamp.working_fingerprint.clone());
                }
            }
            for snapshot in self.server.state.gap_overlays.read().snapshots() {
                if let Some(stamp) = &snapshot.stamp {
                    fingerprints
                        .insert(stamp.checkout_id.clone(), stamp.working_fingerprint.clone());
                }
            }
            assert!(
                !fingerprints.is_empty(),
                "the overlay recompute produced no stamp, so the Own and All rows would be empty"
            );
            for (checkout_id, fingerprint) in fingerprints {
                substitutions.push(substitution(
                    Normalization::WorkingFingerprint,
                    fingerprint,
                    Some(&checkout_id),
                ));
            }
            substitutions
        }
    }

    fn pin_checkout_id(checkout_dir: &Path, checkout_id: &str) {
        let local = checkout_dir.join(".bbox/local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        std::fs::write(local.join("checkout-id"), format!("{checkout_id}\n")).unwrap();
        assert_eq!(
            bbox_corpus_core::identity::ensure_checkout_id(checkout_dir).unwrap(),
            checkout_id,
            "the pinned marker must be what the identity reader returns"
        );
    }

    // ---------------------------------------------------------------------------
    // Capture
    // ---------------------------------------------------------------------------

    fn tool_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A tool response captured whole: the refusal flag and the exact text, so
    /// a success that becomes a refusal (or the reverse) fails even when the
    /// body happens to match.
    fn tool_row(result: &CallToolResult) -> Value {
        json!({
            "is_error": result.is_error.unwrap_or(false),
            "text": tool_text(result),
        })
    }

    /// Pin the process-wide system-memory catalog to a fixture-owned set.
    ///
    /// The rendered knowledge and gap views append this catalog. The bookend
    /// review is right that truncating it was a projection: a change confined
    /// to the trailer left the comparison green. Truncation was reached for
    /// because capturing the REAL catalog verbatim would break bridge parity
    /// on every unrelated `system-defaults/memories` edit.
    ///
    /// Pinning removes both problems instead of trading one for the other. The
    /// catalog becomes two small fixture memories, so the trailer is captured
    /// COMPLETE and byte for byte, and it moves only when this file moves.
    ///
    /// Two memories rather than one, because one cannot show ordering.
    fn pin_system_memory_catalog(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for (slug, title, order) in [
            (
                "bridge-parity-alpha",
                "Bridge parity fixture memory alpha",
                0,
            ),
            ("bridge-parity-beta", "Bridge parity fixture memory beta", 1),
        ] {
            let body = format!(
                "+++\ntitle = {title:?}\ntags = [\"bridge-parity\", \"fixture\"]\n\
             order = {order}\ntemplate = false\n+++\n\n\
             # {title}\n\nFixture memory pinned by the bridge parity harness.\n"
            );
            std::fs::write(dir.join(format!("{slug}.md")), body).unwrap();
        }
        crate::system_memory::init_for_tests_from(dir);

        // The catalog is a process-wide `OnceLock`. Under nextest every test is
        // its own process so the pin always wins, but the documented plain
        // `cargo test` fallback shares one process, and there `get_or_init`
        // would silently keep whoever initialized first. Silently comparing
        // against the REAL catalog is precisely the kind of confusion this
        // harness exists to prevent, so prove the pin took.
        assert!(
            crate::system_memory::get("sm-bridge-parity-alpha").is_some(),
            "the fixture system-memory catalog did not take. Another test in \
         this process initialized the process-wide catalog first; run the \
         bridge parity tests under nextest, which isolates per process."
        );
        assert!(
            crate::system_memory::get("sm-agentic-opening-sequence").is_none(),
            "the REAL system-memory catalog is live in this process, so the \
         published views would capture repo memories instead of the pinned \
         fixture pair."
        );
    }

    /// One captured surface plus the normalizations it declares.
    struct Row {
        name: &'static str,
        value: Value,
        declares: BTreeSet<Normalization>,
    }

    fn row(name: &'static str, value: Value, declares: &[Normalization]) -> Row {
        Row {
            name,
            value,
            declares: declares.iter().copied().collect(),
        }
    }

    /// One nondeterministic value and the placeholder that replaces it.
    ///
    /// A normalization can have more than one instance (two checkouts produce
    /// two working fingerprints), so the placeholder is per-instance while the
    /// declaration a row makes stays at the normalization level.
    struct Substitution {
        normalization: Normalization,
        actual: String,
        placeholder: String,
    }

    fn substitution(
        normalization: Normalization,
        actual: impl Into<String>,
        suffix: Option<&str>,
    ) -> Substitution {
        let base = normalization.placeholder();
        let placeholder = match suffix {
            Some(suffix) => format!("{}_{suffix}>", base.trim_end_matches('>')),
            None => base.to_string(),
        };
        Substitution {
            normalization,
            actual: actual.into(),
            placeholder,
        }
    }

    /// Substitute by exact value, and refuse both failure modes.
    ///
    /// A declared substitution that never fires means the row stopped carrying
    /// the value it was written to tolerate, and its declaration is now a
    /// standing license to normalize something else. An undeclared one that
    /// fires means a nondeterministic value reached a row nobody audited. Both
    /// are how a parity harness stops proving anything, so both fail here.
    fn normalize(
        row: &Row,
        substitutions: &[Substitution],
        audit: bool,
    ) -> (Value, BTreeSet<Normalization>) {
        let mut rendered = serde_json::to_string(&row.value).unwrap();
        let mut fired = BTreeSet::new();
        for substitution in substitutions {
            assert!(
                !substitution.actual.is_empty(),
                "row {}: substitution {:?} has no captured value",
                row.name,
                substitution.normalization
            );
            if !rendered.contains(substitution.actual.as_str()) {
                continue;
            }
            assert!(
                audit || row.declares.contains(&substitution.normalization),
                "row {}: UNAUDITED normalization {:?} fired on value {:?}. A nondeterministic \
             value reached a row that did not declare it; declare it or make the value \
             deterministic at the source.",
                row.name,
                substitution.normalization,
                substitution.actual
            );
            rendered = rendered.replace(substitution.actual.as_str(), &substitution.placeholder);
            fired.insert(substitution.normalization);
        }
        // Guard the whole substring-substitution technique, not just the
        // one value that got caught. A short or low-entropy `actual` can
        // match inside an unrelated token, and the result reads as a
        // plausible capture rather than as an error: the first version
        // substitution landed inside the bind address and produced
        // "127.<DAEMON_VERSION>". A placeholder that ends up welded to
        // adjacent word characters is that mistake, whatever caused it.
        for substitution in substitutions {
            let token = substitution
                .placeholder
                .trim_matches(|c| c == ':' || c == '"');
            let mut from = 0;
            while let Some(at) = rendered[from..].find(token) {
                let start = from + at;
                let end = start + token.len();
                let before = rendered[..start].chars().next_back();
                let after = rendered[end..].chars().next();
                assert!(
                    !before.is_some_and(|c| c.is_alphanumeric())
                        && !after.is_some_and(|c| c.is_alphanumeric()),
                    "row {}: substitution {:?} landed INSIDE a token \
                     ({:?}...{:?} around {token}). The replaced value is \
                     too short or too low-entropy to be unambiguous; anchor \
                     it with surrounding context.",
                    row.name,
                    substitution.normalization,
                    before,
                    after
                );
                from = end;
            }
        }
        for declared in &row.declares {
            assert!(
                audit || fired.contains(declared),
                "row {}: VACUOUS normalization {declared:?} was declared but never fired. The \
             declaration is now a standing license to normalize something else; drop it.",
                row.name
            );
        }
        (serde_json::from_str(&rendered).unwrap(), fired)
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn rendered_capture(capture: &Value) -> String {
        format!("{}\n", serde_json::to_string_pretty(capture).unwrap())
    }

    /// Compare the capture against the committed bytes.
    fn settle(capture: Value) {
        let rendered = rendered_capture(&capture);
        let path = repo_root().join(FIXTURE_RELPATH);
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "reading canonical bridge parity bytes {}: {error}. \
             Regenerate with the ignored producer: \
             cargo nextest run --workspace --run-ignored all \
             -E 'test(produce_bridge_parity_fixture)'",
                path.display()
            )
        });
        if committed == rendered {
            return;
        }
        // A whole-capture diff is unreadable; name the rows that moved.
        let committed_value: Value = serde_json::from_str(&committed).unwrap();
        let mut moved = Vec::new();
        let empty = serde_json::Map::new();
        let old = committed_value
            .get("rows")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let new = capture
            .get("rows")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        for name in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
            if old.get(name) != new.get(name) {
                moved.push(name.clone());
            }
        }
        panic!(
            "BRIDGE PARITY BROKEN (plan section 11): {} row(s) changed: {}.\n\
         Section 11 freezes every listed legacy surface byte-identical while the \
         version-1 registry is the runtime authority. A change here needs a new \
         explicit decision, not a regenerated fixture.\n\
         Committed bytes: {}\n\
         Live capture:\n{rendered}",
            moved.len(),
            moved.join(", "),
            path.display(),
        );
    }

    // ---------------------------------------------------------------------------
    // The captured surfaces (plan section 14.4)
    // ---------------------------------------------------------------------------

    /// Publisher authorization: `AuthorizedPublisher` with its existing four
    /// fields, and the scope-keyed cache returning the same decision on the
    /// second call. Section 11 freezes the field set, so the row carries all
    /// four by name.
    fn publisher_authorization_row(fixture: &BridgeFixture) -> Row {
        let projects = fixture
            .server
            .state
            .records_provider
            .records_snapshot()
            .records;
        let first = fixture
            .server
            .authorize_publisher(&projects, &fixture.scope)
            .expect("the bridge fixture has exactly one publisher for its scope");
        let cached = fixture
            .server
            .authorize_publisher(&projects, &fixture.scope)
            .expect("the scope-keyed cache must answer the repeat call");
        assert_eq!(
            (
                &first.project_id,
                &first.published_scope,
                &first.branch_ref,
                &first.commit
            ),
            (
                &cached.project_id,
                &cached.published_scope,
                &cached.branch_ref,
                &cached.commit
            ),
            "a scope-keyed cache hit must be the same decision"
        );
        // The frozen election entry point, driven with the same committed
        // authority the fixture recorded, so the row captures what the bridge
        // itself would classify rather than a synthetic input.
        let election =
            bbox_indexing::publisher::elect_publisher(&projects, &fixture.scope, |path| {
                crate::config::read_repo_id_inputs(path)
            });
        row(
            "publisher_authorization",
            json!({
                "authorized_publisher": {
                    "project_id": first.project_id,
                    "published_scope": {
                        "repo_id": first.published_scope.repo_id(),
                        "bbox_root_relpath": first.published_scope.bbox_root_relpath(),
                    },
                    "branch_ref": first.branch_ref,
                    "commit": first.commit,
                },
                "elect_publisher": format!("{election:?}"),
            }),
            &[Normalization::RegistryProjectId, Normalization::FixtureRoot],
        )
    }

    /// Published, Own, and All for both lanes, captured through the tools an
    /// external consumer actually calls.
    async fn view_rows(fixture: &BridgeFixture) -> Vec<Row> {
        let project = fixture.base.to_string_lossy().into_owned();
        let mut rows = Vec::new();
        for (name, mode) in [
            ("published_knowledge", "published"),
            ("own_knowledge", "own"),
            ("all_knowledge", "all"),
        ] {
            let result = fixture
                .server
                .bbox_knowledge(Parameters(KnowledgeListParams {
                    project: Some(project.clone()),
                    provisional: Some(mode.to_string()),
                    ..Default::default()
                }))
                .await;
            rows.push(row(name, tool_row(&result), &[]));
        }
        for (name, mode) in [
            ("published_gaps", "published"),
            ("own_gaps", "own"),
            ("all_gaps", "all"),
        ] {
            let result = fixture.server.bbox_gaps(Parameters(GapListParams {
                project: Some(project.clone()),
                provisional: Some(mode.to_string()),
                json: Some(true),
                ..Default::default()
            }));
            rows.push(row(
                name,
                tool_row(&result),
                if mode == "published" {
                    &[Normalization::FixtureRoot]
                } else {
                    &[
                        Normalization::FixtureRoot,
                        Normalization::WorkingFingerprint,
                    ]
                },
            ));
        }
        rows
    }

    /// File provider: a relative `file:` ref resolved through the checkout
    /// authority that section 9 assigns `RenderFileProvider`.
    async fn file_provider_row(fixture: &BridgeFixture) -> Row {
        let result = fixture
            .server
            .bbox_ref_size(Parameters(
                bbox_mcp_tools::mcp_tools::ref_size::RefSizeParams {
                    refs: vec!["file:README.md".into(), "file:missing.md".into()],
                    project_dir: Some(fixture.base.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            ))
            .await;
        row("file_provider", tool_row(&result), &[])
    }

    async fn blame_row(fixture: &BridgeFixture) -> Row {
        let result = fixture
            .server
            .bbox_blame(Parameters(bbox_mcp_tools::mcp_tools::blame::BlameParams {
                file: Some(
                    fixture
                        .base
                        .join("README.md")
                        .to_string_lossy()
                        .into_owned(),
                ),
                line: Some(1),
                entity_ref: None,
                locality: None,
            }))
            .await;
        row("blame", tool_row(&result), &[])
    }

    async fn render_row(fixture: &BridgeFixture) -> Row {
        let result = fixture
            .server
            .bbox_render(Parameters(RenderParams {
                project: Some(fixture.base.to_string_lossy().into_owned()),
                scope: Some("project".into()),
                dry_run: Some(true),
                ..Default::default()
            }))
            .await;
        row("render", tool_row(&result), &[Normalization::FixtureRoot])
    }

    async fn provenance_rows(fixture: &BridgeFixture) -> Vec<Row> {
        let plan = fixture
            .server
            .bbox_provenance_export_plan(Parameters(
                bbox_mcp_tools::mcp_tools::provenance_plan::ProvenanceExportPlanParams::default(),
            ))
            .await;
        let export = fixture
            .server
            .bbox_provenance_export(Parameters(
                bbox_mcp_tools::mcp_tools::provenance::ProvenanceParams {
                    project_id: Some(fixture.registry_project_id.clone()),
                },
            ))
            .await;
        let import = fixture
            .server
            .bbox_provenance_import(Parameters(
                bbox_mcp_tools::mcp_tools::provenance::ProvenanceParams {
                    project_id: Some(fixture.registry_project_id.clone()),
                },
            ))
            .await;
        vec![
            row(
                "provenance_export_plan",
                tool_row(&plan),
                &[
                    Normalization::RegistryProjectId,
                    Normalization::PlanGeneration,
                ],
            ),
            row("provenance_note_export", tool_row(&export), &[]),
            row("provenance_note_import", tool_row(&import), &[]),
        ]
    }

    /// Project administration on the bridge: the version-1 registry listing,
    /// which is the only project administration surface the bridge serves.
    fn project_administration_row(fixture: &BridgeFixture) -> Row {
        row(
            "project_administration",
            tool_row(&fixture.server.bbox_project_list()),
            &[
                Normalization::FixtureRoot,
                Normalization::RegistryProjectId,
                Normalization::RegisteredAt,
            ],
        )
    }

    /// Watcher behavior: section 11 keeps the legacy `Selected` and
    /// `CheckoutId` carriers, so the row captures both carrier shapes and the
    /// registration the bridge installs for the fixture's checkouts.
    fn watcher_row(fixture: &BridgeFixture) -> Row {
        use bbox_artifacts::watcher::ArtifactWatchCarrier;
        let selected = ArtifactWatchCarrier::selected(&fixture.registry_project_id).unwrap();
        let checkout =
            ArtifactWatchCarrier::checkout(&fixture.registry_project_id, OWN_CHECKOUT_ID).unwrap();
        let describe = |carrier: &ArtifactWatchCarrier| {
            json!({
                "project_id": carrier.project_id(),
                "attachment": format!("{:?}", carrier.attachment()),
                "is_attachment": carrier.is_attachment(),
            })
        };
        row(
            "watcher_carriers",
            json!({
                "selected": describe(&selected),
                "checkout": describe(&checkout),
            }),
            &[Normalization::RegistryProjectId],
        )
    }

    /// The one acquisition count that is timing-dependent in principle,
    /// and the global sequence numbers derived from it (D-041).
    ///
    /// Every OTHER counter in the snapshot is exact and stays exact. Only
    /// `publisher_config_tree_read` varies, because it is the one kind
    /// acquired inside the publisher-authorization cache's 250 millisecond
    /// TTL window: how many of the replay's authorizations fall inside that
    /// window is a function of machine speed, not of bridge behavior.
    ///
    /// Measured rather than assumed. Forcing the TTL to zero makes every
    /// authorization a miss and the count reaches 61; forcing it to an hour
    /// leaves only the harness's own explicit invalidations and it settles
    /// at 39. The lane observes 45 and the cluster 47, both strictly inside
    /// that envelope. Neither bound is reachable from a test without
    /// changing production behavior, so no fixture discipline can pin the
    /// value.
    ///
    /// The sequence numbers go with it because they are derived: a counter
    /// that moves two extra times shifts every `last_sequence` after it and
    /// the top-level `sequence` by the same amount.
    const TIMING_DEPENDENT: &str = "<TIMING_DEPENDENT_D041>";
    const TIMING_DEPENDENT_KIND: &str = "publisher_config_tree_read";

    /// Blank exactly the D-041 fields, in place, and nothing else.
    ///
    /// Blanking alone would be a hole: a bridge that stopped acquiring
    /// this kind entirely, or that reset its sequence, would produce a
    /// zero and the sentinel would hide it. So each narrowed field is
    /// STRUCTURALLY asserted before it is replaced. The value is not
    /// compared; that it is a positive integer, and that the global
    /// sequence dominates every counter's last sequence, still is.
    fn narrow_timing_dependent_counters(value: &mut Value) {
        let mut sequences = Vec::new();
        let mut top_level_sequence = None;
        narrow_walk(value, &mut sequences, &mut top_level_sequence);
        // Monotonicity across the snapshot: the global sequence is issued
        // last, so it dominates every per-counter last sequence. A counter
        // claiming a sequence beyond the global one is a corrupt snapshot
        // whatever the exact numbers are.
        if let Some(global) = top_level_sequence {
            for observed in &sequences {
                assert!(
                    *observed <= global,
                    "checkout observation snapshot is inconsistent: a counter \
                     reports last_sequence {observed} beyond the global \
                     sequence {global}. D-041 narrows these VALUES, not the \
                     invariant between them."
                );
            }
        }
    }

    fn narrow_walk(value: &mut Value, sequences: &mut Vec<u64>, top: &mut Option<u64>) {
        match value {
            Value::Object(map) => {
                let is_varying_kind = map
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == TIMING_DEPENDENT_KIND);
                for (key, child) in map.iter_mut() {
                    let narrowed = match key.as_str() {
                        // Derived from the global acquisition ordering, so
                        // they shift with the varying count wherever they
                        // appear.
                        "sequence" | "last_sequence" => true,
                        // The acquisition count itself, on the one varying
                        // kind only. Every other kind keeps its exact count.
                        "granted" | "count" => is_varying_kind,
                        _ => false,
                    };
                    if !narrowed {
                        narrow_walk(child, sequences, top);
                        continue;
                    }
                    let observed = child.as_u64().unwrap_or_else(|| {
                        panic!(
                            "D-041 narrows {key} as a COUNT; it is now {child}, \
                             which means the observation schema changed and the \
                             narrowing no longer describes the field it names."
                        )
                    });
                    assert!(
                        observed > 0,
                        "D-041 narrows the VALUE of {key}, not whether the \
                         surface is exercised at all. It is 0, so the bridge \
                         stopped acquiring {TIMING_DEPENDENT_KIND} entirely, \
                         which the sentinel must not hide."
                    );
                    match key.as_str() {
                        "sequence" => *top = Some(observed),
                        "last_sequence" => sequences.push(observed),
                        _ => {}
                    }
                    *child = Value::String(TIMING_DEPENDENT.to_string());
                }
            }
            Value::Array(items) => {
                for item in items {
                    narrow_walk(item, sequences, top);
                }
            }
            _ => {}
        }
    }

    /// Doctor renders the same count into prose, so the same field has to
    /// be narrowed in both places or the narrowing leaks back in as text.
    fn narrow_timing_dependent_prose(text: &str) -> String {
        text.split('\n')
            .map(|line| {
                let Some(at) = line.find(TIMING_DEPENDENT_KIND) else {
                    return line.to_string();
                };
                let Some(granted) = line[at..].find(" granted") else {
                    return line.to_string();
                };
                let count_start = at + line[at..at + granted].rfind(' ').map_or(0, |i| i + 1);
                format!(
                    "{}{TIMING_DEPENDENT}{}",
                    &line[..count_start],
                    &line[at + granted..]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The checkout observation snapshot after the whole replay, COMPLETE.
    ///
    /// Previously projected to kinds, denials, booleans, and the counter
    /// key-space, because exact counts moved with machine load: the 250
    /// millisecond publisher-authorization cache TTL means a slower run takes
    /// more cache misses and therefore more `PublisherConfigTreeRead` leases.
    ///
    /// The projection is gone and the variance is removed at its source
    /// instead. `pin_publisher_authorization_misses` invalidates the scope's
    /// authorization cache before every captured row, so every authorization
    /// in the replay is a cold miss by construction rather than a race against
    /// a 250 millisecond timer. Counts, sequences, and the granted totals are
    /// all exact again; only the wall-clock reading is substituted.
    fn checkout_observation_row(fixture: &BridgeFixture) -> Row {
        let health = fixture.server.state.checkout_access_observations.health();
        let mut value = serde_json::to_value(&health).unwrap();
        narrow_timing_dependent_counters(&mut value);
        row(
            "checkout_observations",
            value,
            &[Normalization::ObservationWallClock],
        )
    }

    /// Doctor, COMPLETE, including every finding message.
    ///
    /// Previously projected to sections, counts, levels, and next commands.
    /// Finding messages are captured verbatim now; the fields that genuinely
    /// cannot be pinned are named individually in the ledger entry and
    /// substituted by exact value, not dropped.
    async fn doctor_row(fixture: &BridgeFixture) -> Row {
        // detail=full returns exact bounded body pages (A14). Reassemble the
        // pages here so the parity capture still freezes the COMPLETE report
        // payload: concatenated pages are byte-identical to the historical
        // monolithic serialization, so the committed fixture does not move.
        let mut full_text = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = fixture
                .server
                .bbox_doctor(Parameters(crate::tools::doctor::DoctorParams {
                    format: Some("json".into()),
                    detail: Some("full".into()),
                    cursor,
                    ..Default::default()
                }))
                .await;
            let page_value = tool_row(&result);
            let page_text = page_value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let page: Value = serde_json::from_str(page_text)
                .unwrap_or_else(|err| panic!("doctor detail=full page must parse as JSON: {err}"));
            let body = &page["body"];
            full_text.push_str(
                body.get("text")
                    .and_then(Value::as_str)
                    .expect("doctor body page carries text"),
            );
            match body.get("next_cursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        // Doctor carries the observation snapshot twice: as a `checkout_access`
        // object and as rendered findings. Both get the same D-041 narrowing,
        // applied to the SAME fields, so the narrowing cannot leak back in
        // through the prose copy.
        let mut value = json!({ "is_error": false, "text": full_text });
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            let narrowed_prose = narrow_timing_dependent_prose(text);
            let narrowed = match serde_json::from_str::<Value>(&narrowed_prose) {
                Ok(mut body) => {
                    narrow_timing_dependent_counters(&mut body);
                    serde_json::to_string_pretty(&body).unwrap()
                }
                Err(_) => narrowed_prose,
            };
            value["text"] = Value::String(narrowed);
        }
        row(
            "doctor_report",
            value,
            &[
                Normalization::FixtureRoot,
                Normalization::ObservationWallClock,
                Normalization::DaemonVersion,
                Normalization::HostStateDir,
            ],
        )
    }

    /// The two catalog-only tools, which section 11 requires to refuse with
    /// `error.project_catalog_inactive` on the bridge. Capturing the refusal
    /// bytes here freezes the wording alongside every other bridge response.
    async fn catalog_only_refusal_row(fixture: &BridgeFixture) -> Row {
        let advance = fixture
            .server
            .bbox_project_publisher_advance(Parameters(
                crate::tools::project_catalog::ProjectPublisherAdvanceParams {
                    project_id: "p_00000000000000000000000000000000".into(),
                    attachment_id: Some("att_00000000000000000000000000000000".into()),
                    source_generation_id: None,
                    mode: "establish".into(),
                    full_ref: Some("refs/heads/main".into()),
                    expected_generation_id: None,
                    expected_pointer_sha256: None,
                    auto_advance: None,
                    dry_run: false,
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge parity".into(),
                },
            ))
            .await;
        let status = fixture
            .server
            .bbox_project_publisher_status(Parameters(
                crate::tools::project_catalog::ProjectPublisherStatusParams {
                    project_id: "p_00000000000000000000000000000000".into(),
                    ..Default::default()
                },
            ))
            .await;
        for result in [&advance, &status] {
            assert!(
                tool_text(result).contains("error.project_catalog_inactive"),
                "a catalog-only tool must refuse on the bridge"
            );
        }
        row(
            "catalog_only_tools_refuse",
            json!({
                "bbox_project_publisher_advance": tool_row(&advance),
                "bbox_project_publisher_status": tool_row(&status),
            }),
            &[],
        )
    }

    /// Every wall-clock reading present in the captured rows.
    ///
    /// Collected from the CAPTURE itself rather than from a second health
    /// read, so the substituted value is exactly the one in the row.
    fn observation_wall_clock_substitutions(rows: &[Row]) -> Vec<Substitution> {
        let mut readings = BTreeSet::new();
        for row in rows {
            collect_wall_clock(&row.value, &mut readings);
        }
        readings
            .into_iter()
            .flat_map(|reading| {
                // The same reading appears in TWO JSON positions: as a bare
                // NUMBER on an observation counter, and inside a doctor
                // message STRING. One placeholder cannot be valid in both,
                // so each reading emits two substitutions and the
                // value-position form runs first.
                //
                // Matching the leading colon is what makes the
                // value-position form safe: it cannot match a quoted string
                // that merely starts with the same digits, because a quote
                // sits between the colon and the digits there.
                let token = Normalization::ObservationWallClock.placeholder();
                [
                    Substitution {
                        normalization: Normalization::ObservationWallClock,
                        actual: format!(":{reading}"),
                        placeholder: format!(":\"{token}\""),
                    },
                    Substitution {
                        normalization: Normalization::ObservationWallClock,
                        actual: reading.to_string(),
                        placeholder: token.to_string(),
                    },
                ]
            })
            .collect()
    }

    fn collect_wall_clock(value: &Value, out: &mut BTreeSet<u64>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if (key == "last_unix_secs" || key == "last_success_unix_secs")
                        && let Some(reading) = child.as_u64()
                    {
                        // Guard the string substitution: a COUNT must never be
                        // mistaken for a clock reading and blanked.
                        assert!(
                            reading >= 1_000_000_000,
                            "{key} {reading} is too small to be a clock reading"
                        );
                        out.insert(reading);
                    }
                    collect_wall_clock(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_wall_clock(item, out);
                }
            }
            _ => {}
        }
    }

    /// The plan generation digest, read out of the captured plan row itself.
    fn plan_generation_substitutions(rows: &[Row]) -> Vec<Substitution> {
        let Some(plan) = rows.iter().find(|row| row.name == "provenance_export_plan") else {
            return Vec::new();
        };
        let Some(text) = plan.value.get("text").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Ok(body) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        body.get("generation")
            .and_then(Value::as_str)
            .map(|generation| {
                vec![substitution(
                    Normalization::PlanGeneration,
                    generation,
                    None,
                )]
            })
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------------------
    // The blocking proof
    // ---------------------------------------------------------------------------

    /// Plan section 14.4. Replay the canonical bridge fixture across every
    /// surface section 11 freezes and compare the whole capture to committed
    /// bytes.
    ///
    /// Blocking by construction: this is an ordinary `#[test]`, so a bridge
    /// output change fails the workspace gate rather than being noticed later.
    /// Build the fixture, replay every frozen surface, and return the
    /// normalized capture.
    ///
    /// The producer and the verifier below share this function by
    /// construction, so the committed bytes cannot be produced by a code path
    /// that differs from the one that checks them - the failure mode that makes
    /// a golden-file test agree with itself and with nothing else.
    async fn capture(audit: bool) -> Value {
        // Several captured tools consult the process-wide system-memory
        // catalog. Without it they refuse with a panic-shaped error, which
        // would freeze a broken response as the parity baseline. Pinned to
        // a fixture-owned pair so the trailer they append is captured
        // COMPLETE and moves only when this file moves.
        let memories = tempfile::tempdir().unwrap();
        pin_system_memory_catalog(memories.path());
        let fixture = BridgeFixture::new();
        fixture.recompute_overlays();

        // Every row starts from a cold authorization cache, so the lease
        // counts the observation row captures are a property of the code
        // path and not of how fast this machine happened to be.
        let mut rows = Vec::new();
        fixture.cold_authorization();
        rows.push(publisher_authorization_row(&fixture));
        fixture.cold_authorization();
        rows.extend(view_rows(&fixture).await);
        fixture.cold_authorization();
        rows.push(file_provider_row(&fixture).await);
        fixture.cold_authorization();
        rows.push(blame_row(&fixture).await);
        fixture.cold_authorization();
        rows.push(render_row(&fixture).await);
        fixture.cold_authorization();
        rows.extend(provenance_rows(&fixture).await);
        fixture.cold_authorization();
        rows.push(project_administration_row(&fixture));
        fixture.cold_authorization();
        rows.push(watcher_row(&fixture));
        fixture.cold_authorization();
        rows.push(catalog_only_refusal_row(&fixture).await);
        fixture.cold_authorization();
        rows.push(doctor_row(&fixture).await);
        // Last: the observation snapshot must reflect every lease the replay
        // above actually took.
        rows.push(checkout_observation_row(&fixture));

        // Section 14.4 names eleven surfaces. Asserting the inventory here is
        // what stops a future edit from deleting a row and leaving a green
        // harness that covers less than the plan requires.
        let expected: BTreeSet<&str> = [
            "publisher_authorization",
            "published_knowledge",
            "own_knowledge",
            "all_knowledge",
            "published_gaps",
            "own_gaps",
            "all_gaps",
            "file_provider",
            "blame",
            "render",
            "provenance_export_plan",
            "provenance_note_export",
            "provenance_note_import",
            "project_administration",
            "watcher_carriers",
            "catalog_only_tools_refuse",
            "doctor_report",
            "checkout_observations",
        ]
        .into_iter()
        .collect();
        let captured: BTreeSet<&str> = rows.iter().map(|row| row.name).collect();
        assert_eq!(
            captured, expected,
            "the parity harness must cover exactly the section 14.4 surface list"
        );

        let mut substitutions = fixture.substitutions();
        // Wall-clock readings come from the captured observation row itself,
        // not from a second health read, so the substituted value is exactly
        // the one in the capture.
        substitutions.extend(plan_generation_substitutions(&rows));
        substitutions.extend(observation_wall_clock_substitutions(&rows));
        let mut normalized = serde_json::Map::new();
        let mut observed = BTreeMap::new();
        for row in &rows {
            let (value, fired) = normalize(row, &substitutions, audit);
            normalized.insert(row.name.to_string(), value);
            observed.insert(row.name, fired);
        }
        if audit {
            let report = observed
                .iter()
                .map(|(name, fired)| format!("  {name}: {fired:?}"))
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!("observed normalizations per row:\n{report}");
        }

        let mut legend = serde_json::Map::new();
        for substitution in &substitutions {
            legend.insert(
                substitution.placeholder.clone(),
                Value::String(substitution.normalization.justification().to_string()),
            );
        }

        json!({
            "contract": "Phase 5 plan section 11 bridge parity. Every row is a full \
                         response from a bridge-mode server over one canonical fixture. \
                         A diff here is a bridge output change and needs a new explicit \
                         decision, not a regenerated fixture.",
            "normalizations": legend,
            "rows": normalized,
        })
    }

    /// Plan section 14.4. Replay the canonical bridge fixture across every
    /// surface section 11 freezes and compare the whole capture to committed
    /// bytes.
    ///
    /// Blocking by construction: an ordinary `#[test]`, so a bridge output
    /// change fails the workspace gate rather than being noticed later.
    #[tokio::test]
    async fn bridge_parity_holds_against_canonical_fixtures() {
        settle(capture(false).await);
    }

    /// Not a test: the canonical-bytes producer, following the same ignored
    /// producer convention as the D-030 migrated-root fixture.
    ///
    /// It is a test rather than an env switch on the verifier because the lane
    /// build shim forwards only a fixed env allowlist into the builder pod, so
    /// an env-gated mode is unreachable exactly where the gates run.
    #[tokio::test]
    #[ignore = "canonical-bytes producer; run explicitly to regenerate the parity fixture"]
    async fn produce_bridge_parity_fixture() {
        let path = repo_root().join(FIXTURE_RELPATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rendered_capture(&capture(false).await)).unwrap();
        eprintln!("wrote {}", path.display());
    }

    /// Not a test: reports, per row, which substitutions ACTUALLY fire, and
    /// asserts neither direction.
    ///
    /// It exists so a maintainer adding a row sets its declaration from
    /// evidence instead of guessing and then loosening the declaration until
    /// the verifier passes - that loosening path is exactly how the
    /// self-policing check would rot into a regex sweep. The declaration still
    /// has to be written by hand in this file; the audit only reports.
    #[tokio::test]
    #[ignore = "normalization audit; run explicitly when adding or moving a row"]
    async fn audit_bridge_parity_normalizations() {
        capture(true).await;
    }
}
