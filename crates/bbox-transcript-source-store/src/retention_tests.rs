//! Retention, reclamation and admission behavior of the landing store.
use super::*;
use crate::lock::{self, Acquire};
use bbox_corpus_core::project_catalog::{ConnectorKind, ConnectorSourceId};
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

fn scope_for(id: &str) -> ConnectorScope {
    ConnectorScope::new(
        ConnectorSourceId::parse(id).unwrap(),
        ConnectorKind::parse("native_transcript").unwrap(),
    )
}
fn scope() -> ConnectorScope {
    scope_for("csrc_0123456789abcdef")
}
fn snapshot_in(scope: &ConnectorScope, label: &str, bytes: &[u8]) -> StreamSnapshot {
    StreamSnapshot {
        schema_version: SCHEMA_VERSION,
        scope: scope.clone(),
        stream_id: sha256(label.as_bytes()),
        source: NativeSource::Claude,
        account: "default".into(),
        session_id: "session-fixture".into(),
        is_subagent: false,
        content_sha256: sha256(bytes),
        byte_length: bytes.len() as u64,
        chunks: bytes
            .chunks(CHUNK_BYTES)
            .map(|chunk| ChunkRef {
                sha256: sha256(chunk),
                size: chunk.len() as u64,
            })
            .collect(),
    }
}
fn snapshot(bytes: &[u8]) -> StreamSnapshot {
    snapshot_in(&scope(), "claude/default/session", bytes)
}
fn install(store: &TranscriptSourceStore, snapshot: &StreamSnapshot, bytes: &[u8], index: usize) {
    let chunk = &snapshot.chunks[index];
    let start = index * CHUNK_BYTES;
    store
        .install_chunk(
            &snapshot.scope,
            &snapshot.stream_id,
            &chunk.sha256,
            &bytes[start..start + chunk.size as usize],
        )
        .unwrap();
}
fn stage_all(store: &TranscriptSourceStore, snapshot: &StreamSnapshot, bytes: &[u8]) {
    for index in 0..snapshot.chunks.len() {
        install(store, snapshot, bytes, index);
    }
}
fn publish(
    store: &TranscriptSourceStore,
    snapshot: &StreamSnapshot,
    expected: Option<String>,
) -> Result<PublishReceipt> {
    store.publish(
        &PublishRequest {
            snapshot: snapshot.clone(),
            expected_generation: expected,
        },
        "now",
    )
}
fn generation(store: &TranscriptSourceStore, snapshot: &StreamSnapshot) -> Option<String> {
    store
        .status(&snapshot.scope, &snapshot.stream_id)
        .unwrap()
        .generation
}
fn stream_dir(store: &TranscriptSourceStore, snapshot: &StreamSnapshot) -> PathBuf {
    store
        .stream_root(&snapshot.scope, &snapshot.stream_id)
        .unwrap()
}
fn staged(store: &TranscriptSourceStore, snapshot: &StreamSnapshot) -> BTreeSet<String> {
    match fs::read_dir(stream_dir(store, snapshot).join(CHUNKS_DIR)) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}
fn materializations(store: &TranscriptSourceStore, snapshot: &StreamSnapshot) -> usize {
    fs::read_dir(stream_dir(store, snapshot))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".jsonl")
        })
        .count()
}
fn current_bytes(store: &TranscriptSourceStore, snapshot: &StreamSnapshot) -> Vec<u8> {
    let row = store
        .current(&snapshot.scope, &snapshot.stream_id)
        .unwrap()
        .unwrap();
    fs::read(store.data_path(&row).unwrap()).unwrap()
}
/// A two-chunk transcript: one full chunk and a short tail.
fn two_chunk_bytes() -> Vec<u8> {
    format!("{}\n", "a".repeat(CHUNK_BYTES + 10)).into_bytes()
}
fn appended(bytes: &[u8]) -> Vec<u8> {
    let mut longer = bytes.to_vec();
    longer.extend_from_slice(b"{\"tail\":true}\n");
    longer
}
fn later(days: u64) -> SystemTime {
    SystemTime::now() + Duration::from_secs(days * 24 * 60 * 60)
}
fn set_age(path: &Path, age: Duration) {
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}
fn is_missing_chunk_refusal(error: &anyhow::Error) -> bool {
    error.to_string().contains("missing transcript chunk")
}

#[test]
fn appended_publication_assembles_the_prefix_from_current_bytes_and_absorbs_its_staging() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let first_bytes = two_chunk_bytes();
    let first = snapshot(&first_bytes);
    stage_all(&store, &first, &first_bytes);
    let first_generation = publish(&store, &first, None).unwrap().generation;
    assert!(
        staged(&store, &first).is_empty(),
        "the admitted snapshot absorbed every staged chunk"
    );

    // Another producer's in-flight chunk is not part of the admitted snapshot.
    let foreign = b"{\"foreign\":true}\n";
    store
        .install_chunk(&scope(), &first.stream_id, &sha256(foreign), foreign)
        .unwrap();

    let next_bytes = appended(&first_bytes);
    let next = snapshot(&next_bytes);
    assert_eq!(next.chunks[0], first.chunks[0]);
    assert_eq!(
        store.missing_chunks(&next).unwrap(),
        vec![next.chunks[1].sha256.clone()],
        "only the changed tail is requested"
    );
    install(&store, &next, &next_bytes, 1);
    publish(&store, &next, Some(first_generation)).unwrap();
    assert_eq!(current_bytes(&store, &next), next_bytes);
    assert_eq!(
        staged(&store, &next),
        BTreeSet::from([sha256(foreign)]),
        "only the chunk outside the admitted snapshot survives"
    );
}

#[test]
fn a_damaged_current_range_is_missing_and_the_staged_repair_wins_over_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let first_bytes = two_chunk_bytes();
    let first = snapshot(&first_bytes);
    stage_all(&store, &first, &first_bytes);
    let first_generation = publish(&store, &first, None).unwrap().generation;
    let next_bytes = appended(&first_bytes);
    let next = snapshot(&next_bytes);
    // A verified range answers presence on its own.
    assert!(
        !store
            .missing_chunks(&next)
            .unwrap()
            .contains(&next.chunks[0].sha256)
    );

    let row = store.current(&scope(), &first.stream_id).unwrap().unwrap();
    let mut data = OpenOptions::new()
        .write(true)
        .open(store.data_path(&row).unwrap())
        .unwrap();
    data.seek(SeekFrom::Start(10)).unwrap();
    data.write_all(b"DAMAGED").unwrap();
    drop(data);

    assert_eq!(
        store.missing_chunks(&next).unwrap(),
        [next.chunks[0].sha256.clone(), next.chunks[1].sha256.clone()]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
        "damaged bytes are never reported present"
    );
    let refused = publish(&store, &next, Some(first_generation.clone())).unwrap_err();
    assert!(is_missing_chunk_refusal(&refused), "{refused:#}");
    assert_eq!(generation(&store, &next), Some(first_generation.clone()));

    stage_all(&store, &next, &next_bytes);
    publish(&store, &next, Some(first_generation)).unwrap();
    assert_eq!(current_bytes(&store, &next), next_bytes);
}

/// One single-chunk stream `[x]` is current; producer B has staged `y` and
/// been told nothing is missing. `invalidate` removes `y` behind its back.
fn presence_invalidation_case(invalidate: impl FnOnce(&TranscriptSourceStore, &StreamSnapshot)) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let x_bytes = b"{\"x\":1}\n".to_vec();
    let x = snapshot(&x_bytes);
    stage_all(&store, &x, &x_bytes);
    let x_generation = publish(&store, &x, None).unwrap().generation;

    let y_bytes = b"{\"y\":2}\n".to_vec();
    let y = snapshot(&y_bytes);
    let status = generation(&store, &y);
    stage_all(&store, &y, &y_bytes);
    assert!(store.missing_chunks(&y).unwrap().is_empty());

    invalidate(&store, &y);

    assert_eq!(generation(&store, &y), Some(x_generation.clone()));
    let refused = publish(&store, &y, status).unwrap_err();
    assert!(is_missing_chunk_refusal(&refused), "{refused:#}");
    assert_eq!(
        generation(&store, &y),
        Some(x_generation.clone()),
        "the pointer did not move"
    );
    assert_eq!(current_bytes(&store, &y), x_bytes);

    // The producer's retry re-asks missing and uploads what is absent.
    let status = generation(&store, &y);
    assert_eq!(
        store.missing_chunks(&y).unwrap(),
        vec![y.chunks[0].sha256.clone()]
    );
    stage_all(&store, &y, &y_bytes);
    publish(&store, &y, status).unwrap();
    assert_eq!(current_bytes(&store, &y), y_bytes);
}

#[test]
fn presence_invalidated_by_staging_expiry_fails_publish_before_the_pointer_moves() {
    presence_invalidation_case(|store, _| {
        let report = store.reap(&ReaperPolicy::default(), later(2)).unwrap();
        assert_eq!(report.totals().expired_chunks.files, 1);
    });
}

#[test]
fn presence_invalidated_by_budget_eviction_fails_publish_before_the_pointer_moves() {
    presence_invalidation_case(|store, _| {
        let policy = ReaperPolicy {
            staging_budget_bytes: 0,
            ..ReaperPolicy::default()
        };
        let report = store.reap(&policy, SystemTime::now()).unwrap();
        assert_eq!(report.totals().budget_evicted_chunks.files, 1);
    });
}

#[test]
fn presence_invalidated_by_replace_and_restore_fails_publish_before_the_pointer_moves() {
    presence_invalidation_case(|store, y| {
        // Producer A replaces the stream with B's bytes (absorbing B's staged
        // chunk) and then restores the original generation.
        let original = store.current(&y.scope, &y.stream_id).unwrap().unwrap();
        let replaced = publish(store, y, Some(original.generation.clone()))
            .unwrap()
            .generation;
        let x_bytes = b"{\"x\":1}\n".to_vec();
        let x = snapshot(&x_bytes);
        stage_all(store, &x, &x_bytes);
        publish(store, &x, Some(replaced)).unwrap();
        assert_eq!(
            generation(store, &x),
            Some(original.generation),
            "the expected generation is equal again"
        );
        assert!(staged(store, y).is_empty());
    });
}

#[test]
fn a_generation_admitted_between_status_and_missing_leaves_the_other_producer_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let base_bytes = b"{\"base\":0}\n".to_vec();
    let base = snapshot(&base_bytes);
    stage_all(&store, &base, &base_bytes);
    let base_generation = publish(&store, &base, None).unwrap().generation;

    // B read the status; A admits a different generation before B's missing.
    let b_status = generation(&store, &base);
    let a_bytes = b"{\"a\":1}\n".to_vec();
    let a = snapshot(&a_bytes);
    stage_all(&store, &a, &a_bytes);
    let a_generation = publish(&store, &a, b_status.clone()).unwrap().generation;

    let b_bytes = b"{\"b\":2}\n".to_vec();
    let b = snapshot(&b_bytes);
    assert_eq!(
        store.missing_chunks(&b).unwrap(),
        vec![b.chunks[0].sha256.clone()]
    );
    stage_all(&store, &b, &b_bytes);
    let conflict = publish(&store, &b, b_status.clone()).unwrap_err();
    assert!(
        conflict
            .to_string()
            .contains("transcript_generation_conflict"),
        "{conflict:#}"
    );
    assert_eq!(generation(&store, &b), Some(a_generation.clone()));

    // When B offers the very bytes A admitted, B succeeds idempotently.
    assert!(store.missing_chunks(&a).unwrap().is_empty());
    assert_eq!(
        publish(&store, &a, b_status).unwrap().generation,
        a_generation
    );

    // A fresh status lets B complete.
    let retry = generation(&store, &b);
    assert_ne!(retry, Some(base_generation));
    stage_all(&store, &b, &b_bytes);
    publish(&store, &b, retry).unwrap();
    assert_eq!(current_bytes(&store, &b), b_bytes);
}

#[test]
fn chunks_left_by_a_crash_after_pointer_install_are_removed_by_the_staging_phase() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    store.hooks.skip_absorb.store(true, Ordering::SeqCst);
    let bytes = two_chunk_bytes();
    let snap = snapshot(&bytes);
    stage_all(&store, &snap, &bytes);
    publish(&store, &snap, None).unwrap();
    assert_eq!(staged(&store, &snap).len(), 2, "deletion was skipped");

    let report = store
        .reap(&ReaperPolicy::default(), SystemTime::now())
        .unwrap();
    assert_eq!(report.totals().absorbed_chunks.files, 2);
    assert_eq!(report.totals().absorbed_chunks.bytes, bytes.len() as u64);
    assert!(staged(&store, &snap).is_empty());
    assert_eq!(current_bytes(&store, &snap), bytes);
}

#[test]
fn the_generation_phase_waits_for_the_reader_lease_while_the_staging_phase_runs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let first_bytes = b"{\"n\":1}\n".to_vec();
    let first = snapshot(&first_bytes);
    stage_all(&store, &first, &first_bytes);
    publish(&store, &first, None).unwrap();

    let lease = store.reader_lease(&scope()).unwrap();
    store.hooks.skip_absorb.store(true, Ordering::SeqCst);
    for n in 2..=4 {
        let bytes = format!("{{\"n\":{n}}}\n").into_bytes();
        let next = snapshot(&bytes);
        let expected = generation(&store, &next);
        stage_all(&store, &next, &bytes);
        publish(&store, &next, expected).unwrap();
    }
    assert_eq!(materializations(&store, &first), 4);

    let held = store
        .reap(&ReaperPolicy::default(), SystemTime::now())
        .unwrap();
    assert_eq!(
        held.scopes[0].generation_phase,
        Some(GenerationPhase::LeaseHeld)
    );
    assert_eq!(held.totals().non_current_generations.files, 0);
    assert_eq!(held.totals().absorbed_chunks.files, 1, "staging still ran");
    assert_eq!(materializations(&store, &first), 4);
    let health = store.source_storage_health(&scope()).unwrap().unwrap();
    assert_eq!(health.generation_phase, Some(GenerationPhase::LeaseHeld));
    assert_eq!(
        health.accounting.as_ref().unwrap().non_current_generations,
        3
    );

    drop(lease);
    let ran = store
        .reap(&ReaperPolicy::default(), SystemTime::now())
        .unwrap();
    assert_eq!(ran.scopes[0].generation_phase, Some(GenerationPhase::Ran));
    assert_eq!(ran.totals().non_current_generations.files, 3);
    assert_eq!(materializations(&store, &first), 1);
    assert_eq!(current_bytes(&store, &first), b"{\"n\":4}\n");
    let accounting = store
        .source_storage_health(&scope())
        .unwrap()
        .unwrap()
        .accounting
        .unwrap();
    assert_eq!(accounting.live_bytes, 8);
    assert_eq!(accounting.non_current_generation_bytes, 0);
    assert_eq!(
        accounting.staged_chunks, 2,
        "young unadmitted staging is retained"
    );
    assert_eq!(accounting.published_streams, 1);
}

#[test]
fn a_producer_blocked_on_a_removed_stream_lock_recreates_the_stream_and_holds_the_only_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let producer = TranscriptSourceStore::open(&root).unwrap();
    let old_bytes = b"{\"old\":true}\n".to_vec();
    let old = snapshot(&old_bytes);
    stage_all(&producer, &old, &old_bytes);
    let stream = stream_dir(&producer, &old);

    // The deleter holds the verified lock of the unpublished stream.
    let deleter = lock::acquire(&stream, Acquire::Existing, &Hooks::default())
        .unwrap()
        .unwrap();
    let (opened_tx, opened_rx) = mpsc::channel();
    *producer.hooks.after_lock_open.lock().unwrap() = Some(Box::new(move || {
        opened_tx.send(()).unwrap();
    }));
    let new_bytes = b"{\"new\":true}\n".to_vec();
    let new = snapshot(&new_bytes);
    let worker = {
        let producer = producer.clone();
        let new = new.clone();
        let new_bytes = new_bytes.clone();
        std::thread::spawn(move || {
            producer.install_chunk(&scope(), &new.stream_id, &new.chunks[0].sha256, &new_bytes)
        })
    };
    // The producer opened the lock that is about to be unlinked.
    opened_rx.recv().unwrap();
    let removed = deleter.remove_stream_dir(&Hooks::default()).unwrap();
    assert!(removed.directory_removed);
    worker.join().unwrap().unwrap();

    // The inode number of the unlinked lock may be reused once every holder
    // closed it; the directory contents show the producer started over.
    assert!(stream.join("lock").is_file());
    assert_eq!(
        staged(&producer, &new),
        BTreeSet::from([new.chunks[0].sha256.clone()])
    );
    // The re-created lock is the one a new holder verifies.
    assert!(
        lock::acquire(&stream, Acquire::Existing, &Hooks::default())
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_published_stream_directory_and_lock_survive_any_staging_age() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let bytes = b"{\"kept\":true}\n".to_vec();
    let snap = snapshot(&bytes);
    stage_all(&store, &snap, &bytes);
    publish(&store, &snap, None).unwrap();
    let report = store.reap(&ReaperPolicy::default(), later(30)).unwrap();
    assert_eq!(report.totals().unpublished_streams.files, 0);
    let stream = stream_dir(&store, &snap);
    assert!(stream.join("lock").is_file());
    assert!(stream.join(CURRENT_NAME).is_file());
    assert_eq!(current_bytes(&store, &snap), bytes);
}

#[test]
fn a_producer_recreating_the_stream_after_lock_unlink_keeps_its_directory_lock_and_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let producer = TranscriptSourceStore::open(&root).unwrap();
    let reaper = TranscriptSourceStore::open(&root).unwrap();
    let old_bytes = b"{\"old\":true}\n".to_vec();
    let old = snapshot(&old_bytes);
    stage_all(&producer, &old, &old_bytes);
    let new_bytes = b"{\"new\":true}\n".to_vec();
    let new = snapshot(&new_bytes);
    {
        let producer = producer.clone();
        let new = new.clone();
        let new_bytes = new_bytes.clone();
        *reaper.hooks.after_lock_unlink.lock().unwrap() = Some(Box::new(move || {
            producer
                .install_chunk(&scope(), &new.stream_id, &new.chunks[0].sha256, &new_bytes)
                .unwrap();
        }));
    }
    let report = reaper.reap(&ReaperPolicy::default(), later(2)).unwrap();
    assert_eq!(report.totals().expired_chunks.files, 1);
    assert_eq!(
        report.totals().unpublished_streams.files,
        0,
        "the empty-only removal failed and the attempt ended"
    );
    let stream = stream_dir(&producer, &new);
    assert!(stream.join("lock").is_file());
    assert_eq!(
        staged(&producer, &new),
        BTreeSet::from([new.chunks[0].sha256.clone()])
    );
    assert!(producer.missing_chunks(&new).unwrap().is_empty());
}

#[test]
fn status_and_missing_for_an_unknown_stream_create_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let bytes = two_chunk_bytes();
    let unknown = snapshot(&bytes);
    assert!(generation(&store, &unknown).is_none());
    assert_eq!(store.missing_chunks(&unknown).unwrap().len(), 2);
    assert!(!root.join(scope().connector_source_id().as_str()).exists());

    // The directory disappears while missing waits on its lock.
    stage_all(&store, &unknown, &bytes);
    let stream = stream_dir(&store, &unknown);
    let deleter = lock::acquire(&stream, Acquire::Existing, &Hooks::default())
        .unwrap()
        .unwrap();
    let (opened_tx, opened_rx) = mpsc::channel();
    *store.hooks.after_lock_open.lock().unwrap() = Some(Box::new(move || {
        opened_tx.send(()).unwrap();
    }));
    let worker = {
        let store = store.clone();
        let unknown = unknown.clone();
        std::thread::spawn(move || store.missing_chunks(&unknown))
    };
    opened_rx.recv().unwrap();
    assert!(
        deleter
            .remove_stream_dir(&Hooks::default())
            .unwrap()
            .directory_removed
    );
    assert_eq!(worker.join().unwrap().unwrap().len(), 2);
    assert!(!stream.exists());
    assert!(generation(&store, &unknown).is_none());
    assert!(!stream.exists());
}

#[test]
fn expired_contact_temporaries_are_removed_under_the_contact_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    store
        .record_contact(
            &ContactRequest {
                scope: scope(),
                scan_id: sha256(b"scan"),
                completed: None,
            },
            "start",
        )
        .unwrap();
    drop(store.reader_lease(&scope()).unwrap());
    let scope_root = store.scope_root(&scope());
    let temporary = scope_root.join(".tmpInterrupted");
    fs::write(&temporary, b"{\"partial\":").unwrap();

    let held = lock_contact(&scope_root).unwrap();
    let worker = {
        let store = store.clone();
        std::thread::spawn(move || store.reap(&ReaperPolicy::default(), later(2)))
    };
    std::thread::sleep(Duration::from_millis(200));
    assert!(temporary.exists(), "the sweep waits for the contact lock");
    drop(held);
    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.totals().contact_temporaries.files, 1);
    assert!(!temporary.exists());
    for kept in [
        "contact.json",
        CONTACT_LOCK_NAME,
        READERS_LOCK_NAME,
        HEALTH_NAME,
    ] {
        assert!(scope_root.join(kept).is_file(), "{kept}");
    }
    assert_eq!(
        store
            .source_contact(&scope())
            .unwrap()
            .unwrap()
            .last_contact_at,
        "start"
    );
}

#[test]
fn staging_over_the_budget_is_evicted_oldest_first_across_streams() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let one = snapshot_in(&scope(), "stream-one", b"{\"one\":11111}\n");
    let two = snapshot_in(&scope(), "stream-two", b"{\"two\":22222}\n");
    let newest = snapshot_in(&scope(), "stream-one", b"{\"new\":33333}\n");
    let size = one.byte_length;
    assert_eq!(two.byte_length, size);
    assert_eq!(newest.byte_length, size);
    stage_all(&store, &one, b"{\"one\":11111}\n");
    stage_all(&store, &two, b"{\"two\":22222}\n");
    stage_all(&store, &newest, b"{\"new\":33333}\n");
    let chunk = |snap: &StreamSnapshot| {
        stream_dir(&store, snap)
            .join(CHUNKS_DIR)
            .join(&snap.chunks[0].sha256)
    };
    set_age(&chunk(&one), Duration::from_secs(3 * 3600));
    set_age(&chunk(&two), Duration::from_secs(2 * 3600));
    set_age(&chunk(&newest), Duration::from_secs(3600));

    let policy = ReaperPolicy {
        staging_budget_bytes: size + size / 2,
        ..ReaperPolicy::default()
    };
    let report = store.reap(&policy, SystemTime::now()).unwrap();
    assert_eq!(report.totals().budget_evicted_chunks.files, 2);
    assert!(!chunk(&one).exists());
    assert!(!chunk(&two).exists());
    assert!(chunk(&newest).exists());
    let accounting = report.scopes[0].accounting.clone().unwrap();
    assert_eq!(accounting.staged_chunk_bytes, size);
    assert_eq!(
        accounting.unpublished_streams, 2,
        "young unpublished streams stay"
    );
}

#[test]
fn storage_errors_wrapped_in_a_persist_error_surface_as_storage_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = TranscriptSourceStore::open(&root).unwrap();
    let bytes = b"{\"full\":true}\n".to_vec();
    let snap = snapshot(&bytes);
    *store.hooks.write_error.lock().unwrap() = Some(std::io::ErrorKind::StorageFull);
    let error = store
        .install_chunk(&scope(), &snap.stream_id, &snap.chunks[0].sha256, &bytes)
        .unwrap_err();
    assert!(
        error.downcast_ref::<StorageExhausted>().is_some(),
        "{error:#}"
    );
    *store.hooks.write_error.lock().unwrap() = None;
    stage_all(&store, &snap, &bytes);
    *store.hooks.write_error.lock().unwrap() = Some(std::io::ErrorKind::QuotaExceeded);
    let error = publish(&store, &snap, None).unwrap_err();
    assert!(
        error.downcast_ref::<StorageExhausted>().is_some(),
        "{error:#}"
    );
    assert!(
        generation(&store, &snap).is_none(),
        "the pointer did not move"
    );
}

#[test]
fn admission_counts_reservations_of_concurrent_streams_and_releases_them_on_every_exit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let reserve = 1_000_000;
    let chunk = 100u64;
    // A new stream costs its chunk plus three metadata allowances.
    let need = chunk + 3 * METADATA_ALLOWANCE;
    let available = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(reserve + 2 * need - 1));
    let probe_available = available.clone();
    let ledger = Arc::new(StorageAdmission::with_probe(
        &root,
        ReservePolicy::Fixed(reserve),
        Arc::new(move |_: &Path| {
            Ok(FsSpace {
                total: u64::MAX / 2,
                available: probe_available.load(Ordering::SeqCst),
            })
        }),
    ));
    let first = TranscriptSourceStore::open(&root)
        .unwrap()
        .with_admission(ledger.clone());
    let second = TranscriptSourceStore::open(&root)
        .unwrap()
        .with_admission(ledger.clone());
    let a_bytes = format!("{}\n", "a".repeat(chunk as usize - 1)).into_bytes();
    let b_bytes = format!("{}\n", "b".repeat(chunk as usize - 1)).into_bytes();
    let a = snapshot_in(&scope(), "stream-a", &a_bytes);
    let b = snapshot_in(&scope(), "stream-b", &b_bytes);

    let (opened_tx, opened_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    *first.hooks.after_lock_open.lock().unwrap() = Some(Box::new(move || {
        opened_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    }));
    let worker = {
        let first = first.clone();
        let a = a.clone();
        let a_bytes = a_bytes.clone();
        std::thread::spawn(move || {
            first.install_chunk(&scope(), &a.stream_id, &a.chunks[0].sha256, &a_bytes)
        })
    };
    opened_rx.recv().unwrap();
    assert_eq!(ledger.outstanding(), need, "stream a holds its reservation");
    let refused = second
        .install_chunk(&scope(), &b.stream_id, &b.chunks[0].sha256, &b_bytes)
        .unwrap_err();
    assert!(
        refused.downcast_ref::<StorageExhausted>().is_some(),
        "{refused:#}"
    );
    assert!(
        !stream_dir(&second, &b).exists(),
        "a refused install creates nothing"
    );
    release_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(ledger.outstanding(), 0);
    second
        .install_chunk(&scope(), &b.stream_id, &b.chunks[0].sha256, &b_bytes)
        .unwrap();
    assert_eq!(ledger.outstanding(), 0);

    // A failed publish releases too, and a current generation needs no headroom.
    let receipt = publish(&first, &a, None).unwrap();
    let wrong = StreamSnapshot {
        content_sha256: sha256(b"wrong"),
        ..b.clone()
    };
    assert!(publish(&second, &wrong, None).is_err());
    assert_eq!(ledger.outstanding(), 0);
    available.store(0, Ordering::SeqCst);
    assert_eq!(
        publish(&first, &a, None).unwrap().generation,
        receipt.generation
    );
    let exhausted = publish(&second, &b, None).unwrap_err();
    assert!(exhausted.downcast_ref::<StorageExhausted>().is_some());
    assert_eq!(ledger.outstanding(), 0);
}

#[test]
fn the_first_run_reclaims_a_legacy_store_with_oversized_and_orphaned_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // An absent root is an empty run and creates nothing.
    let absent = root.join("absent");
    let empty = TranscriptSourceStore::for_read(&absent)
        .reap(&ReaperPolicy::default(), SystemTime::now())
        .unwrap();
    assert!(empty.scopes.is_empty());
    assert!(!absent.exists());

    // Legacy shape: every chunk ever installed retained, and generations
    // accumulated under a held reader lease.
    let store = TranscriptSourceStore::open(&root).unwrap();
    assert!(store.source_storage_health(&scope()).unwrap().is_none());
    store.hooks.skip_absorb.store(true, Ordering::SeqCst);
    let lease = store.reader_lease(&scope()).unwrap();
    let mut bytes = two_chunk_bytes();
    let mut last = snapshot(&bytes);
    for _ in 0..3 {
        last = snapshot(&bytes);
        let expected = generation(&store, &last);
        stage_all(&store, &last, &bytes);
        publish(&store, &last, expected).unwrap();
        bytes = appended(&bytes);
    }
    drop(lease);
    let published = stream_dir(&store, &last);
    assert_eq!(materializations(&store, &last), 3);
    assert_eq!(
        staged(&store, &last).len(),
        4,
        "prefix chunk plus three tails"
    );
    // Oversized garbage under a chunk name never verifies.
    let oversized = published.join(CHUNKS_DIR).join(sha256(b"oversized"));
    fs::write(&oversized, vec![b'z'; CHUNK_BYTES + 1]).unwrap();
    // Orphans: an interrupted publish temporary, an unpublished stream, and
    // a source that no longer has a grant.
    let orphan_temporary = published.join(".tmpOrphan");
    fs::write(&orphan_temporary, b"partial").unwrap();
    let orphan = snapshot_in(&scope(), "orphan", b"{\"orphan\":true}\n");
    stage_all(&store, &orphan, b"{\"orphan\":true}\n");
    let revoked = scope_for("csrc_fedcba9876543210");
    let revoked_stream = snapshot_in(&revoked, "revoked", b"{\"revoked\":true}\n");
    stage_all(&store, &revoked_stream, b"{\"revoked\":true}\n");

    let report = store.reap(&ReaperPolicy::default(), later(2)).unwrap();
    assert_eq!(report.scopes.len(), 2, "every source directory is visited");
    let totals = report.totals();
    assert_eq!(totals.absorbed_chunks.files, 2, "current prefix and tail");
    assert_eq!(
        totals.expired_chunks.files, 5,
        "two superseded tails, the oversized file, and each orphaned stream's chunk"
    );
    assert_eq!(totals.non_current_generations.files, 2);
    assert_eq!(totals.unpublished_streams.files, 2);
    assert_eq!(totals.temporary_files.files, 1);
    assert_eq!(report.error_count(), 0, "{report:?}");

    assert!(staged(&store, &last).is_empty());
    assert_eq!(materializations(&store, &last), 1);
    assert_eq!(
        current_bytes(&store, &last),
        appended(&appended(&two_chunk_bytes()))
    );
    assert!(!stream_dir(&store, &orphan).exists());
    assert!(!stream_dir(&store, &revoked_stream).exists());
    assert_eq!(store.snapshots(&scope()).unwrap().len(), 1);

    let health = store.source_storage_health(&scope()).unwrap().unwrap();
    let accounting = health.accounting.unwrap();
    assert_eq!(health.generation_phase, Some(GenerationPhase::Ran));
    assert_eq!(accounting.live_bytes, last.byte_length);
    assert_eq!(accounting.staged_chunk_bytes, 0);
    assert_eq!(accounting.non_current_generation_bytes, 0);
    assert_eq!(accounting.unpublished_streams, 0);
    assert!(accounting.stored_bytes >= accounting.live_bytes);
    assert!(
        accounting.stored_bytes < accounting.live_bytes + 64 * 1024,
        "live content is stored once: {accounting:?}"
    );
    let revoked_health = store.source_storage_health(&revoked).unwrap().unwrap();
    assert_eq!(revoked_health.accounting.unwrap().live_bytes, 0);

    // A corrupt cached record is an error to its reader, never a panic, and
    // the next run replaces it.
    let scope_root = store.scope_root(&scope());
    fs::write(scope_root.join(HEALTH_NAME), b"{not json").unwrap();
    assert!(store.source_storage_health(&scope()).is_err());
    store
        .reap(&ReaperPolicy::default(), SystemTime::now())
        .unwrap();
    assert!(store.source_storage_health(&scope()).unwrap().is_some());
}
