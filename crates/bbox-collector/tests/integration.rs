//! End-to-end collector pass against an in-process corpus server.
//!
//! The server is a minimal axum router backed by the real
//! `blackbox_corpus_service::RecordStore` (a dev-dependency, acceptable for
//! tests only). We drive one collector pass against a fixture claude root and
//! assert:
//! - the corpus archive file byte-for-byte equals the shipped complete-line
//!   prefix;
//! - a replay (fresh local cursor, crash-before-save) dedupes on the server;
//! - a mid-file append ships only the delta on a second pass;
//! - the empty-batch startup resync adopts the server's acknowledged tail.
//!
//! Test isolation invariants: tempdir roots are canonicalized before path
//! assertions, and nothing touches the real `$HOME` / prod daemon.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use bbox_collector::config::AccountRoot;
use bbox_collector::{CollectorConfig, Shipper};
use blackbox_corpus_service::RecordStore;
use bro_capabilities::RecordIngestRequest;

const HOST_ID: &str = "testhost";

async fn spawn_corpus(store: Arc<RecordStore>) -> SocketAddr {
    let app = Router::new()
        .route("/internal/records", post(ingest))
        .with_state(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

async fn ingest(
    State(store): State<Arc<RecordStore>>,
    Json(request): Json<RecordIngestRequest>,
) -> axum::response::Response {
    match store.ingest(request) {
        Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": error.code, "message": error.message, "retryable": false }
            })),
        )
            .into_response(),
    }
}

fn write_token(dir: &Path) -> PathBuf {
    // load_or_create writes a valid owner-only 64-hex token file.
    let path = dir.join("service.token");
    bro_rpc::ServiceToken::load_or_create(&path).unwrap();
    path
}

fn config(
    corpus_url: String,
    token: PathBuf,
    state_dir: PathBuf,
    claude_root: PathBuf,
) -> CollectorConfig {
    CollectorConfig {
        corpus_url,
        service_token_file: token,
        host_id: Some(HOST_ID.to_string()),
        poll_interval_secs: 30,
        state_dir,
        claude_roots: vec![AccountRoot {
            account: "claude".into(),
            path: claude_root,
        }],
        codex_root: None,
    }
}

fn seed_claude_session(claude_root: &Path, lines: &[&str]) -> PathBuf {
    let project = claude_root.join("projects").join("-repo");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join("sess-1.jsonl");
    let mut body = String::new();
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn archive_path(store: &RecordStore) -> PathBuf {
    store
        .collector_archive_root()
        .join(HOST_ID)
        .join("claude")
        .join("claude")
        .join("projects")
        .join("-repo")
        .join("sess-1.jsonl")
}

#[tokio::test(flavor = "multi_thread")]
async fn ships_prefix_dedupes_replay_and_appends_delta() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let claude_root = root.join("claude");
    let token = write_token(&root);

    let store = Arc::new(RecordStore::open(&root.join("corpus")).unwrap());
    let address = spawn_corpus(store.clone()).await;
    let corpus_url = format!("http://{address}");

    let session = seed_claude_session(
        &claude_root,
        &[r#"{"type":"user","sessionId":"sess-1","message":{"content":"one"}}"#],
    );
    let expected_first = std::fs::read(&session).unwrap();

    // Pass 1: fresh cursor ships the whole file.
    let mut shipper = Shipper::from_config(config(
        corpus_url.clone(),
        token.clone(),
        root.join("state-a"),
        claude_root.clone(),
    ))
    .unwrap();
    assert_eq!(shipper.producer(), "collector:testhost");
    let summary = shipper.tick().await;
    assert_eq!(summary.files_scanned, 1);
    assert_eq!(summary.bytes_shipped, expected_first.len() as u64);
    assert_eq!(summary.streams_behind, 0);

    let archived = std::fs::read(archive_path(&store)).unwrap();
    assert_eq!(
        archived, expected_first,
        "archive must equal the shipped prefix byte-for-byte"
    );

    // Replay: a fresh shipper (simulating a crash before the cursor was saved)
    // re-ships from zero. The server dedupes by byte range; the archive stays
    // identical and does not double.
    let mut replay = Shipper::from_config(config(
        corpus_url.clone(),
        token.clone(),
        root.join("state-b"),
        claude_root.clone(),
    ))
    .unwrap();
    let _ = replay.tick().await;
    let after_replay = std::fs::read(archive_path(&store)).unwrap();
    assert_eq!(
        after_replay, expected_first,
        "replay must dedupe: archive unchanged"
    );

    // Mid-file append: only the delta ships on the next pass of the ORIGINAL
    // shipper (its local cursor already covers the first line).
    let mut appended = expected_first.clone();
    let second_line =
        "{\"type\":\"user\",\"sessionId\":\"sess-1\",\"message\":{\"content\":\"two\"}}\n";
    appended.extend_from_slice(second_line.as_bytes());
    std::fs::write(&session, &appended).unwrap();

    let summary = shipper.tick().await;
    assert_eq!(
        summary.bytes_shipped,
        second_line.len() as u64,
        "only the appended delta should ship"
    );
    let after_append = std::fs::read(archive_path(&store)).unwrap();
    assert_eq!(after_append, appended, "archive now holds the full file");
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_batch_resync_adopts_server_tail() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let claude_root = root.join("claude");
    let token = write_token(&root);

    let store = Arc::new(RecordStore::open(&root.join("corpus")).unwrap());
    let address = spawn_corpus(store.clone()).await;
    let corpus_url = format!("http://{address}");

    let session = seed_claude_session(
        &claude_root,
        &[r#"{"type":"user","sessionId":"sess-1","message":{"content":"one"}}"#],
    );
    let file_len = std::fs::metadata(&session).unwrap().len();

    // Ship once so the server knows a tail for this stream.
    let mut primer = Shipper::from_config(config(
        corpus_url.clone(),
        token.clone(),
        root.join("state-primer"),
        claude_root.clone(),
    ))
    .unwrap();
    assert_eq!(primer.tick().await.bytes_shipped, file_len);

    // A brand-new collector with an empty local cursor resyncs via the empty
    // batch and adopts the server's tail; the subsequent tick ships nothing.
    let mut fresh = Shipper::from_config(config(
        corpus_url,
        token,
        root.join("state-fresh"),
        claude_root,
    ))
    .unwrap();
    fresh.resync().await.unwrap();
    let summary = fresh.tick().await;
    assert_eq!(
        summary.bytes_shipped, 0,
        "after resync the fresh collector is already caught up"
    );
    assert_eq!(summary.streams_behind, 0);
}
