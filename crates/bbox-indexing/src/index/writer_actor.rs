//! IndexWriterActor — single owner of tantivy writes (concurrency-model §4.3).
//!
//! Every in-process tantivy mutation flows through one queue consumed by one
//! dedicated OS thread. That removes the P3 competing-single-writer class:
//! hot store upserts no longer race the background reindexer for tantivy's
//! single-writer lock (the old paths opened a fresh `IndexWriter` per call on
//! a tokio worker, blocking behind a whole reindex pass while holding the
//! `state.idx` write guard — which stalled searches too).
//!
//! Design deltas from §4.3 as written:
//! - The actor creates its writer **per batch / per pass** (with retry on a
//!   busy cross-process lock) instead of holding it for the daemon lifetime.
//!   Serialization through the queue is what eliminates in-process LockBusy;
//!   releasing between batches keeps the boot-time initial index build and
//!   test helpers working, and frees the writer's arena + threads when idle.
//! - Small ops are fire-and-forget (`enqueue`): callers already treated index
//!   sync as best-effort (warn-and-continue), and the durable stores +
//!   periodic reindex reconcile any op lost to a writer error. Tests needing
//!   read-your-write determinism call [`IndexWriterActor::flush_blocking`].
//! - A reindex pass runs as a job inside the actor and drains queued small
//!   ops at phase boundaries into the same writer, so mid-pass writes land in
//!   the pass's own commit instead of waiting behind it.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tantivy::{Index, IndexReader, IndexWriter};

use bbox_knowledge::knowledge::KnowledgeEntry;
use bbox_stores::roadmap::RoadmapItem;
use bbox_threads::threads::Thread;

use super::knowledge_docs::KnowledgeIndexDocument;
use super::reindex::{conservative_log_merge_policy, execute_reindex_pass};
use super::{FieldHandles, ReindexConfig, StatsCache, TranscriptIndex};

/// One queued index mutation.
pub enum IndexWriteOp {
    UpsertKnowledge(Box<KnowledgeEntry>),
    DeleteKnowledge(String),
    ReplaceKnowledge(Vec<KnowledgeIndexDocument>),
    ReplaceKnowledgeLogical {
        logical_ref: String,
        documents: Vec<KnowledgeIndexDocument>,
    },
    ReplaceKnowledgeScope {
        scope_hash: String,
        documents: Vec<KnowledgeIndexDocument>,
    },
    UpsertRoadmap(Box<RoadmapItem>),
    DeleteRoadmap(String),
    UpsertThread(Box<Thread>),
    /// Full thread-store replacement from a point-in-time snapshot.
    UpsertThreadsStore(Vec<Thread>),
    /// Run a reindex pass inside the actor; ack carries the summary line.
    ReindexPass {
        full: bool,
        dirty: bool,
        ack: mpsc::SyncSender<Result<String>>,
    },
    /// Barrier: acked once every previously-enqueued op has been applied and
    /// committed. Drained-queue semantics, not durability.
    Flush(mpsc::SyncSender<()>),
}

/// Cloneable handle to the writer actor. Lives in `SharedState`.
#[derive(Clone)]
pub struct IndexWriterActor {
    tx: mpsc::Sender<IndexWriteOp>,
}

/// Everything the actor thread needs to apply ops and publish commits.
struct ActorCtx {
    index: Index,
    fields: FieldHandles,
    config: ReindexConfig,
    reader: IndexReader,
    stats_cache: StatsCache,
}

/// Cap on ops folded into one writer/commit cycle. Bounds worst-case batch
/// latency; the queue itself is unbounded (ops are small).
const MAX_BATCH_OPS: usize = 256;

const WRITER_HEAP_SMALL_OPS: usize = 50_000_000;
const WRITER_HEAP_REINDEX: usize = 100_000_000;

impl IndexWriterActor {
    /// Spawn the daemon's single tantivy writer actor for `idx`
    /// (concurrency-model §4.3). All production index mutations and
    /// reindex passes flow through the returned handle.
    ///
    /// Also registers the engine's daemon hooks (embed enqueue + the
    /// manual-rebuild store-document pass): every spawn point is a daemon
    /// boot path, so this is the single place store-side wiring attaches
    /// to the engine.
    pub fn spawn_for(idx: &TranscriptIndex) -> Self {
        register_index_store_hooks();
        Self::spawn(
            idx.index_handle(),
            idx.field_handles(),
            idx.reindex_config(),
            idx.reader_handle(),
            idx.stats_cache_handle(),
        )
    }

    pub fn spawn(
        index: Index,
        fields: FieldHandles,
        config: ReindexConfig,
        reader: IndexReader,
        stats_cache: StatsCache,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<IndexWriteOp>();
        let ctx = ActorCtx {
            index,
            fields,
            config,
            reader,
            stats_cache,
        };
        if let Err(err) = std::thread::Builder::new()
            .name("blackbox-index-writer".into())
            .spawn(move || {
                let _scope = bbox_util::util::BlockingScope::enter();
                run_actor(rx, ctx)
            })
        {
            tracing::error!(error = %err, "failed to spawn index writer actor thread");
        }
        Self { tx }
    }

    /// Queue a mutation, fire-and-forget. A send failure (actor thread dead)
    /// is logged; the periodic reindex pass reconciles the index from the
    /// durable stores, so a dropped op degrades freshness, not correctness.
    pub fn enqueue(&self, op: IndexWriteOp) {
        if self.tx.send(op).is_err() {
            tracing::error!("index writer actor unavailable; dropping index write op");
        }
    }

    /// Run a reindex pass on the actor thread and wait for its outcome.
    /// Returns the human-readable summary line on commit/no-op.
    pub fn run_reindex_pass(&self, full: bool, dirty: bool) -> Result<String> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::ReindexPass { full, dirty, ack })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the reindex ack"))?
    }

    /// Block until every op enqueued before this call has been applied and
    /// committed. Test/shutdown determinism helper.
    pub fn flush_blocking(&self) -> Result<()> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::Flush(ack))
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the flush ack"))
    }
}

fn run_actor(rx: mpsc::Receiver<IndexWriteOp>, ctx: ActorCtx) {
    // A ReindexPass that arrives while a small-op batch is being collected is
    // deferred to its own iteration rather than folded into the batch.
    let mut deferred: Option<IndexWriteOp> = None;
    loop {
        let op = match deferred.take() {
            Some(op) => op,
            None => match rx.recv() {
                Ok(op) => op,
                Err(_) => break, // all senders dropped — daemon shutdown
            },
        };
        match op {
            IndexWriteOp::ReindexPass { full, dirty, ack } => {
                let result = run_pass(&ctx, &rx, full, dirty);
                let _ = ack.send(result);
            }
            first => {
                let mut batch = vec![first];
                while batch.len() < MAX_BATCH_OPS {
                    match rx.try_recv() {
                        Ok(pass @ IndexWriteOp::ReindexPass { .. }) => {
                            deferred = Some(pass);
                            break;
                        }
                        Ok(op) => batch.push(op),
                        Err(_) => break,
                    }
                }
                process_batch(&ctx, batch);
            }
        }
    }
    tracing::info!("index writer actor stopped");
}

/// Apply a batch of small ops under one writer and one commit.
fn process_batch(ctx: &ActorCtx, batch: Vec<IndexWriteOp>) {
    let mut flush_acks: Vec<mpsc::SyncSender<()>> = Vec::new();
    let mut ops: Vec<IndexWriteOp> = Vec::new();
    for op in batch {
        match op {
            IndexWriteOp::Flush(ack) => flush_acks.push(ack),
            other => ops.push(other),
        }
    }

    if !ops.is_empty() {
        match create_writer(&ctx.index, WRITER_HEAP_SMALL_OPS) {
            Ok(mut writer) => {
                writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
                for op in ops {
                    apply_small_op(ctx, &mut writer, op);
                }
                match writer.commit() {
                    Ok(_) => post_commit(ctx),
                    Err(err) => tracing::error!(
                        error = %err,
                        "index writer actor: batch commit failed; reindex pass will reconcile"
                    ),
                }
            }
            Err(err) => tracing::error!(
                error = %err,
                dropped_ops = ops.len(),
                "index writer actor: writer unavailable; dropping batch (reindex pass will reconcile)"
            ),
        }
    }

    for ack in flush_acks {
        let _ = ack.send(());
    }
}

/// Execute a reindex pass with this actor's writer, draining queued small
/// ops into the same writer at phase boundaries so they land in the pass's
/// commit. Concurrent pass requests are refused (callers retry); flushes
/// drained mid-pass are acked after the commit.
fn run_pass(
    ctx: &ActorCtx,
    rx: &mpsc::Receiver<IndexWriteOp>,
    full: bool,
    dirty: bool,
) -> Result<String> {
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    if !full {
        writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    }
    let mut pending_flushes: Vec<mpsc::SyncSender<()>> = Vec::new();
    let outcome = {
        let mut drain = |writer: &mut IndexWriter| {
            while let Ok(op) = rx.try_recv() {
                match op {
                    IndexWriteOp::ReindexPass { ack, .. } => {
                        let _ = ack.send(Err(anyhow!(
                            "a reindex pass is already running; retry after it completes"
                        )));
                    }
                    IndexWriteOp::Flush(ack) => pending_flushes.push(ack),
                    small => apply_small_op(ctx, writer, small),
                }
            }
        };
        execute_reindex_pass(
            &ctx.index,
            &ctx.config,
            ctx.fields,
            full,
            dirty,
            &mut writer,
            &mut drain,
        )
    };
    if outcome.is_ok() {
        // Full rebuilds settle merge threads before publishing (the old
        // owned-writer path did this between commit and meta save; doing it
        // post-commit here preserves the settled-segments property).
        if full {
            if let Err(err) = writer.wait_merging_threads() {
                tracing::warn!(error = %err, "index writer actor: wait_merging_threads failed");
            }
        }
        post_commit(ctx);
    }
    for ack in pending_flushes {
        let _ = ack.send(());
    }
    outcome
}

fn apply_small_op(ctx: &ActorCtx, writer: &mut IndexWriter, op: IndexWriteOp) {
    let (kind, result): (&str, Result<()>) = match op {
        IndexWriteOp::UpsertKnowledge(entry) => (
            "upsert_knowledge",
            super::knowledge_docs::apply_knowledge_upsert(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &entry,
            ),
        ),
        IndexWriteOp::DeleteKnowledge(id) => (
            "delete_knowledge",
            super::knowledge_docs::apply_knowledge_delete(writer, ctx.fields, &id),
        ),
        IndexWriteOp::ReplaceKnowledge(documents) => (
            "replace_knowledge",
            super::knowledge_docs::apply_knowledge_replace(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &documents,
            ),
        ),
        IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref,
            documents,
        } => (
            "replace_knowledge_logical",
            super::knowledge_docs::apply_knowledge_logical_replace(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &logical_ref,
                &documents,
            ),
        ),
        IndexWriteOp::ReplaceKnowledgeScope {
            scope_hash,
            documents,
        } => (
            "replace_knowledge_scope",
            super::knowledge_docs::apply_knowledge_scope_replace(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &scope_hash,
                &documents,
            ),
        ),
        IndexWriteOp::UpsertRoadmap(item) => (
            "upsert_roadmap",
            super::roadmap_docs::apply_roadmap_upsert(
                writer,
                ctx.fields,
                &ctx.config.roadmap_path,
                &item,
            ),
        ),
        IndexWriteOp::DeleteRoadmap(id) => (
            "delete_roadmap",
            super::roadmap_docs::apply_roadmap_delete(writer, ctx.fields, &id),
        ),
        IndexWriteOp::UpsertThread(thread) => (
            "upsert_thread",
            super::thread_docs::apply_thread_upsert(
                writer,
                ctx.fields,
                &ctx.config.threads_path,
                &thread,
            ),
        ),
        IndexWriteOp::UpsertThreadsStore(threads) => (
            "upsert_threads_store",
            super::thread_docs::apply_threads_store_upsert(
                writer,
                ctx.fields,
                &ctx.config.threads_path,
                &threads,
            ),
        ),
        IndexWriteOp::ReindexPass { .. } | IndexWriteOp::Flush(_) => {
            debug_assert!(false, "control ops are routed before apply_small_op");
            return;
        }
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, op = kind, "index writer actor: op failed; reindex pass will reconcile");
    }
}

fn knowledge_path(config: &ReindexConfig) -> &Path {
    &config.knowledge_path
}

/// Embed-bootstrap trampoline: the daemon registers
/// `embed_queue::register_index_embed_hooks` here at SharedState
/// construction (same inversion as the threads/notes embed hooks), and
/// every writer-actor spawn fires it. Keeps this module below the embed
/// pipeline in the crate DAG; unregistered means embed enqueue stays a
/// no-op, matching the uninstalled-queue behavior.
static EMBED_BOOTSTRAP: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Register the embed-hook bootstrap. Idempotent; first registration wins.
pub fn register_embed_bootstrap(hook: fn()) {
    let _ = EMBED_BOOTSTRAP.set(hook);
}

/// Wire the store side into the engine's daemon hooks: embed enqueue for
/// project-file/git chunks (via the registered bootstrap), and the
/// knowledge store-document pass for manual rebuilds
/// (`TranscriptIndex::build_index`). Idempotent; called from every
/// writer-actor spawn and directly by store-coupled tests that drive
/// `build_index` without an actor.
pub fn register_index_store_hooks() {
    if let Some(bootstrap) = EMBED_BOOTSTRAP.get() {
        bootstrap();
    }
    super::embed_hook::register_manual_store_pass(manual_knowledge_store_pass);
}

fn manual_knowledge_store_pass(
    config: &ReindexConfig,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut std::collections::HashMap<String, super::FileMeta>,
) -> Result<u64> {
    super::knowledge_docs::reindex_knowledge_store_standalone(
        &config.knowledge_path,
        &config.projects_path,
        fields,
        writer,
        meta,
    )
}

/// Make the committed segments visible to searches and invalidate the
/// stats TTL cache — the same post-write publication the old inline
/// facade methods performed under the `state.idx` write guard.
fn post_commit(ctx: &ActorCtx) {
    if let Err(err) = ctx.reader.reload() {
        tracing::warn!(error = %err, "index writer actor: reader reload failed");
    }
    *ctx.stats_cache.lock() = None;
}

/// Create a writer, retrying briefly when the cross-process lock is busy
/// (e.g. a previous daemon instance still winding down at boot). In-process
/// contention cannot occur — this actor is the only in-process writer.
fn create_writer(index: &Index, heap: usize) -> Result<IndexWriter> {
    let mut delay = Duration::from_millis(100);
    let attempts = 5;
    for attempt in 1..=attempts {
        match index.writer(heap) {
            Ok(w) => return Ok(w),
            Err(tantivy::TantivyError::LockFailure(e, hint)) => {
                if attempt == attempts {
                    return Err(tantivy::TantivyError::LockFailure(e, hint).into());
                }
                tracing::warn!(
                    attempt,
                    "index writer actor: tantivy writer lock busy (cross-process); retrying"
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("retry loop returns on success or final attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{SearchParams, TranscriptIndex};

    fn test_entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: "actor test entry".into(),
            content: content.into(),
            cluster: None,
            variants: Default::default(),
            category: bbox_knowledge::knowledge::Category::Memory,
            scope: bbox_knowledge::knowledge::Scope::Global,
            project: None,
            providers: Vec::new(),
            priority: bbox_knowledge::knowledge::Priority::Standard,
            weight: 100,
            status: bbox_knowledge::knowledge::Status::Active,
            approval: bbox_knowledge::knowledge::Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-06-10T00:00:00Z".into(),
            updated_at: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn test_index(dir: &std::path::Path) -> TranscriptIndex {
        TranscriptIndex::open_or_create(
            &dir.join("idx"),
            Vec::new(),
            None,
            dir.join("projects.json"),
            dir.join("kb.json"),
            dir.join("threads.json"),
            dir.join("roadmap.json"),
        )
        .unwrap()
    }

    /// Write entries into the on-disk central kb store. Reindex passes
    /// reconcile knowledge docs from this file, so test entries that must
    /// survive a pass need to live here, not just in the index.
    fn persist_kb_entries(kb_path: &std::path::Path, entries: &[KnowledgeEntry]) {
        use bbox_stores::store_persister::StoreSnapshot;
        let mut kb = bbox_knowledge::knowledge::Knowledge::open(kb_path).unwrap();
        for entry in entries {
            kb.upsert_generated(entry.clone()).unwrap();
        }
        let snapshot = kb.snapshot().unwrap();
        bbox_corpus_core::json_store::atomic_write_json_locked(kb_path, &snapshot).unwrap();
    }

    fn search(index: &TranscriptIndex, q: &str) -> String {
        index
            .search(&SearchParams {
                query: q.into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
                exclude_self: None,
            })
            .unwrap()
    }

    #[test]
    fn batched_ops_commit_once_and_become_searchable_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);

        // A burst of ops lands in one batch: upserts, a delete of one of
        // them, and a roadmap-style overwrite of the same entity id.
        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(test_entry(
            "aaaa1111",
            "ferrocene compliance baseline",
        ))));
        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(test_entry(
            "bbbb2222",
            "quux transient entry",
        ))));
        actor.enqueue(IndexWriteOp::DeleteKnowledge("bbbb2222".into()));
        actor.flush_blocking().unwrap();

        let hits = search(&index, "ferrocene compliance");
        assert!(hits.contains("ferrocene"), "{hits}");
        let hits = search(&index, "quux transient");
        assert!(
            !hits.contains("quux"),
            "delete in the same batch must win: {hits}"
        );
    }

    #[test]
    fn logical_replace_removes_every_prior_visibility_variant() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);
        let logical_ref = super::super::knowledge_docs::knowledge_entity_id("scope0001");
        let published =
            KnowledgeIndexDocument::published(test_entry("scope0001", "published obsolete marker"));
        let mut provisional = published.clone();
        provisional.entity_id = "provisional_knowledge:scope:checkout:scope0001".into();
        provisional.entry.content = "provisional obsolete marker".into();
        provisional.visibility = "provisional".into();

        actor.enqueue(IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref: logical_ref.clone(),
            documents: vec![published, provisional],
        });
        actor.flush_blocking().unwrap();
        assert!(search(&index, "published obsolete").contains("published"));
        assert!(search(&index, "provisional obsolete").contains("provisional"));

        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(test_entry(
            "scope0001",
            "published variant update",
        ))));
        actor.flush_blocking().unwrap();
        assert!(search(&index, "published variant update").contains("published"));
        assert!(
            search(&index, "provisional obsolete").contains("provisional"),
            "variant-precise upsert must not delete the provisional document"
        );

        actor.enqueue(IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref,
            documents: vec![KnowledgeIndexDocument::published(test_entry(
                "scope0001",
                "replacement current marker",
            ))],
        });
        actor.flush_blocking().unwrap();

        assert!(!search(&index, "published obsolete").contains("published"));
        assert!(!search(&index, "provisional obsolete").contains("provisional"));
        assert!(search(&index, "replacement current").contains("replacement"));
    }

    #[test]
    fn scope_replace_preserves_globals_and_unrelated_projects() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);
        let mut old_scope = KnowledgeIndexDocument::published(test_entry(
            "scope-a-old",
            "obsolete scope alpha marker",
        ));
        old_scope.scope_hash = Some("scope-alpha".into());
        let mut other_scope =
            KnowledgeIndexDocument::published(test_entry("scope-b", "retained scope beta marker"));
        other_scope.scope_hash = Some("scope-beta".into());
        let global =
            KnowledgeIndexDocument::published(test_entry("global-entry", "retained global marker"));
        actor.enqueue(IndexWriteOp::ReplaceKnowledge(vec![
            old_scope,
            other_scope,
            global,
        ]));
        actor.flush_blocking().unwrap();

        let mut current_scope = KnowledgeIndexDocument::published(test_entry(
            "scope-a-new",
            "current scope alpha marker",
        ));
        current_scope.scope_hash = Some("scope-alpha".into());
        actor.enqueue(IndexWriteOp::ReplaceKnowledgeScope {
            scope_hash: "scope-alpha".into(),
            documents: vec![current_scope],
        });
        actor.flush_blocking().unwrap();

        assert!(!search(&index, "obsolete scope alpha").contains("obsolete"));
        assert!(search(&index, "current scope alpha").contains("current"));
        assert!(search(&index, "retained scope beta").contains("retained"));
        assert!(search(&index, "retained global").contains("retained"));
    }

    #[test]
    fn ops_enqueued_around_a_reindex_pass_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);

        // Both entries are store-backed: a pass reconciles knowledge docs
        // from the kb store file, so index-only entries would be wiped by
        // design.
        let before = test_entry("cccc3333", "zorbl before pass");
        let during = test_entry("dddd4444", "wibble during pass");
        persist_kb_entries(
            &dir.path().join("kb.json"),
            &[before.clone(), during.clone()],
        );
        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(before)));

        // Run a pass while a second thread races an op into the queue.
        // Whether it drains mid-pass or batches after, flush_blocking must
        // not return until it is committed.
        let actor2 = actor.clone();
        let racer = std::thread::spawn(move || {
            actor2.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(during)));
        });
        let summary = actor.run_reindex_pass(false, true).unwrap();
        assert!(summary.starts_with("auto-reindex:"), "{summary}");
        racer.join().unwrap();
        actor.flush_blocking().unwrap();

        let hits = search(&index, "zorbl");
        assert!(hits.contains("zorbl"), "{hits}");
        let hits = search(&index, "wibble");
        assert!(hits.contains("wibble"), "{hits}");
    }

    #[test]
    fn full_pass_rebuilds_and_keeps_store_docs() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);

        // Store-backed entry: write it to the kb store file so the pass's
        // store-doc phase re-adds it after delete_all_documents.
        let kb_path = dir.path().join("kb.json");
        let entry = test_entry("eeee5555", "florp durable entry");
        persist_kb_entries(&kb_path, &[entry.clone()]);

        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(entry)));
        actor.flush_blocking().unwrap();
        assert!(search(&index, "florp").contains("florp"));

        let summary = actor.run_reindex_pass(true, true).unwrap();
        assert!(summary.starts_with("auto-reindex:"), "{summary}");
        assert!(
            search(&index, "florp").contains("florp"),
            "full rebuild must re-add store-backed docs"
        );
    }
}
