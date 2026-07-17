//! The tick loop: discover session files, diff against the local cursor, ship
//! new complete-line bytes, and advance the local cursor only after the server
//! receipt confirms the stream tail.
//!
//! Delivery guarantees:
//! - **At-least-once, server as authority.** The local cursor advances only
//!   after a receipt whose `transcript_cursors[stream]` reaches the shipped
//!   `byte_end`. A crash between ship and cursor-save replays the same byte
//!   range next tick; the deterministic `record_id` and the server's byte-range
//!   dedupe make that a no-op.
//! - **No spool.** The provider's own session file is the durable backlog. An
//!   unreachable corpus just leaves the cursor unadvanced and retries next tick.
//! - **Resync on drift.** On startup, and after any gap/overlap rejection, the
//!   collector POSTs an empty batch and adopts the server's acknowledged tails
//!   for its own streams, then retries once.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bbox_transcript_read::adapters::{TranscriptReadAdapter, TranscriptScanTarget};
use bbox_transcript_read::interactive::{ClaudeTranscriptAdapter, CodexTranscriptAdapter};
use bbox_transcript_read::types::{TranscriptSource, TranscriptStorage};
use bro_rpc::ServiceToken;
use tokio::sync::Notify;

use crate::client::{IngestClient, IngestError};
use crate::config::{CollectorConfig, resolve_host_id};
use crate::cursor::CollectorCursors;
use crate::prefix::read_complete_line_prefix;
use crate::record::{StreamKey, build_increment_records, relative_path_under};

/// Fixed account label for codex streams (codex has a single root, no
/// per-account labelling in the provider layout).
const CODEX_ACCOUNT: &str = "codex";
/// Synthetic session id for the account-wide `history.jsonl` streams, which
/// carry no session id in the provider layout but still need a safe component.
const HISTORY_SESSION_ID: &str = "history";
/// Ingest batch cap enforced by the server (`MAX_INGEST_BATCH`).
const MAX_BATCH_RECORDS: usize = 256;

/// Per-tick counters emitted as a structured log line.
#[derive(Debug, Default, Clone)]
pub struct TickSummary {
    pub files_scanned: usize,
    pub bytes_shipped: u64,
    pub streams_behind: usize,
    pub streams_skipped: usize,
    /// True when the corpus was unreachable this tick (drives backoff).
    pub unreachable: bool,
}

/// One discovered shippable file.
struct Discovered {
    key: StreamKey,
    path: PathBuf,
}

enum ShipOutcome {
    /// Fully caught up (nothing left to ship for this stream this tick).
    Advanced { bytes: u64 },
    /// Bytes remain (partial ship, torn tail, or unconfirmed receipt).
    Behind { bytes: u64 },
}

enum ShipError {
    /// Corpus transport failure: abort the tick and back off.
    Unreachable,
    /// This stream cannot ship right now (unsafe identity, oversized line, read
    /// error, or unrecovered cursor drift). Skip it and continue the tick.
    Skipped(String),
}

pub struct Shipper {
    producer: String,
    client: IngestClient,
    cursors: CollectorCursors,
    poll_interval: Duration,
    claude: Option<ClaudeTranscriptAdapter>,
    claude_roots: BTreeMap<String, PathBuf>,
    codex: Option<CodexTranscriptAdapter>,
    codex_root: Option<PathBuf>,
}

impl Shipper {
    pub fn from_config(config: CollectorConfig) -> anyhow::Result<Self> {
        let host_id = resolve_host_id(&config)?;
        let producer = format!("collector:{host_id}");

        let token = ServiceToken::load(&config.service_token_file).map_err(|e| {
            anyhow::anyhow!(
                "load service token {}: {e} (the token file must exist and be owner-only 0600)",
                config.service_token_file.display()
            )
        })?;
        let request_timeout = Duration::from_secs(config.poll_interval_secs.max(30));
        let client = IngestClient::new(&config.corpus_url, &token, request_timeout)?;

        let cursors = CollectorCursors::load(config.state_dir.join("cursors.json"))
            .map_err(|e| anyhow::anyhow!(e))?;

        let claude_roots: BTreeMap<String, PathBuf> = config
            .claude_roots
            .iter()
            .map(|root| (root.account.clone(), root.path.clone()))
            .collect();
        let claude = (!config.claude_roots.is_empty()).then(|| {
            ClaudeTranscriptAdapter::new(
                config
                    .claude_roots
                    .iter()
                    .map(|root| (root.account.clone(), root.path.clone()))
                    .collect(),
            )
        });
        let codex = config
            .codex_root
            .as_ref()
            .map(|root| CodexTranscriptAdapter::new(root.clone()));

        Ok(Self {
            producer,
            client,
            cursors,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            claude,
            claude_roots,
            codex,
            codex_root: config.codex_root,
        })
    }

    /// Producer id (`collector:<host-id>`), exposed for tests and logging.
    pub fn producer(&self) -> &str {
        &self.producer
    }

    /// Run the tick loop until `shutdown` fires. Graceful mid-batch shutdown is
    /// safe (at-least-once + server dedupe make replays no-ops).
    pub async fn run(&mut self, shutdown: Arc<Notify>) -> anyhow::Result<()> {
        if let Err(error) = self.resync().await {
            tracing::warn!(%error, "startup resync failed; proceeding on local cursors");
        }
        let mut backoff = Duration::from_secs(1);
        loop {
            let summary = self.tick().await;
            let sleep_for = if summary.unreachable {
                let wait = backoff.min(self.poll_interval);
                backoff = (backoff * 2).min(self.poll_interval);
                tracing::warn!(
                    files_scanned = summary.files_scanned,
                    backoff_secs = wait.as_secs(),
                    "corpus unreachable; backing off"
                );
                wait
            } else {
                backoff = Duration::from_secs(1);
                tracing::info!(
                    files_scanned = summary.files_scanned,
                    bytes_shipped = summary.bytes_shipped,
                    streams_behind = summary.streams_behind,
                    streams_skipped = summary.streams_skipped,
                    "collector tick"
                );
                self.poll_interval
            };
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::info!("collector shutting down");
                    return Ok(());
                }
                _ = tokio::time::sleep(sleep_for) => {}
            }
        }
    }

    /// Run exactly one scan/ship pass. Public for integration tests.
    pub async fn tick(&mut self) -> TickSummary {
        let mut summary = TickSummary::default();
        let discovered = match self.discover().await {
            Ok(discovered) => discovered,
            Err(error) => {
                tracing::warn!(%error, "transcript discovery failed this tick");
                return summary;
            }
        };
        summary.files_scanned = discovered.len();

        for discovered in discovered {
            match self.ship_stream(&discovered).await {
                Ok(ShipOutcome::Advanced { bytes }) => summary.bytes_shipped += bytes,
                Ok(ShipOutcome::Behind { bytes }) => {
                    summary.bytes_shipped += bytes;
                    summary.streams_behind += 1;
                }
                Err(ShipError::Unreachable) => {
                    summary.unreachable = true;
                    break;
                }
                Err(ShipError::Skipped(reason)) => {
                    summary.streams_skipped += 1;
                    tracing::warn!(
                        stream = %discovered.key.stream_id(&self.producer),
                        reason,
                        "skipping stream this tick"
                    );
                }
            }
        }
        summary
    }

    /// Blocking filesystem discovery, offloaded from the runtime workers.
    async fn discover(&self) -> anyhow::Result<Vec<Discovered>> {
        let claude = self.claude.clone();
        let claude_roots = self.claude_roots.clone();
        let codex = self.codex.clone();
        let codex_root = self.codex_root.clone();
        tokio::task::spawn_blocking(move || {
            discover_blocking(claude, &claude_roots, codex, codex_root.as_deref())
        })
        .await
        .map_err(|e| anyhow::anyhow!("discovery task panicked: {e}"))
    }

    async fn ship_stream(&mut self, discovered: &Discovered) -> Result<ShipOutcome, ShipError> {
        if let Err(reason) = discovered.key.validate() {
            return Err(ShipError::Skipped(reason));
        }
        let stream = discovered.key.stream_id(&self.producer);
        let mut resynced = false;

        'stream: loop {
            let tail = self.cursors.tail(&stream);
            let size = match tokio::fs::metadata(&discovered.path).await {
                Ok(metadata) => metadata.len(),
                Err(error) => return Err(ShipError::Skipped(format!("stat failed: {error}"))),
            };
            if size < tail {
                // Append-only files never shrink; treat as truncation/replacement
                // and let the server stay authoritative (do not rewind blindly).
                return Err(ShipError::Skipped(format!(
                    "file shorter ({size}) than local cursor ({tail}); leaving to server authority"
                )));
            }
            if size == tail {
                return Ok(ShipOutcome::Advanced { bytes: 0 });
            }

            let data = match read_complete_line_prefix(&discovered.path, tail).await {
                Ok(data) => data,
                Err(error) => return Err(ShipError::Skipped(format!("read failed: {error}"))),
            };
            if data.is_empty() {
                // Only a torn (incomplete) final line beyond the cursor.
                return Ok(ShipOutcome::Behind { bytes: 0 });
            }

            let records =
                match build_increment_records(&self.producer, &discovered.key, tail, &data, || {
                    self.cursors.next_record_cursor()
                }) {
                    Ok(records) => records,
                    Err(error) => return Err(ShipError::Skipped(error.to_string())),
                };

            let mut shipped_bytes = 0u64;
            let mut confirmed_tail = tail;
            for batch in size_bounded_batches(&records) {
                let intended_tail = batch_end_offset(batch);
                match self.client.ingest(batch.to_vec()).await {
                    Ok(receipt) => {
                        let server_tail = receipt
                            .transcript_cursors
                            .get(&stream)
                            .copied()
                            .unwrap_or(0);
                        if server_tail < intended_tail {
                            tracing::warn!(
                                %stream,
                                server_tail,
                                intended_tail,
                                "receipt did not confirm shipped tail; retrying next tick"
                            );
                            return Ok(ShipOutcome::Behind {
                                bytes: shipped_bytes,
                            });
                        }
                        self.cursors.set_tail(&stream, server_tail);
                        if let Err(error) = self.cursors.save() {
                            tracing::warn!(%stream, error, "cursor persist failed");
                        }
                        shipped_bytes += intended_tail - confirmed_tail;
                        confirmed_tail = intended_tail;
                    }
                    Err(error) if error.is_stream_cursor_rejection() => {
                        if resynced {
                            return Err(ShipError::Skipped(format!(
                                "cursor still rejected after resync: {error}"
                            )));
                        }
                        tracing::warn!(%stream, %error, "cursor drift; resyncing from server");
                        if let Err(resync_error) = self.resync().await {
                            return Err(ShipError::Skipped(format!(
                                "resync after drift failed: {resync_error}"
                            )));
                        }
                        resynced = true;
                        // Records were built from the stale tail; rebuild from
                        // the adopted server tail by restarting the stream loop.
                        continue 'stream;
                    }
                    Err(IngestError::Unavailable(message)) => {
                        tracing::warn!(%stream, message, "corpus unavailable mid-batch");
                        return Err(ShipError::Unreachable);
                    }
                    Err(error) => {
                        return Err(ShipError::Skipped(format!("ingest failed: {error}")));
                    }
                }
            }
            let _ = confirmed_tail;
            return Ok(ShipOutcome::Advanced {
                bytes: shipped_bytes,
            });
        }
    }

    /// POST an empty batch and adopt the server's acknowledged tails for this
    /// collector's own streams. Also the startup resync.
    pub async fn resync(&mut self) -> Result<(), IngestError> {
        let receipt = self.client.ingest(Vec::new()).await?;
        let prefix = format!("{}/", self.producer);
        let mut adopted = 0usize;
        for (stream, tail) in &receipt.transcript_cursors {
            if stream.starts_with(&prefix) {
                self.cursors.adopt_server_tail(stream, *tail);
                adopted += 1;
            }
        }
        if let Err(error) = self.cursors.save() {
            tracing::warn!(error, "cursor persist after resync failed");
        }
        tracing::debug!(adopted, "resynced stream cursors from server");
        Ok(())
    }
}

/// The server's HTTP body cap is 2 MiB (axum's default on the blackboxd
/// corpus role; pinned `MAX_HTTP_BODY_BYTES` on the standalone service), and
/// a full 256-record batch of near-`MAX_RECORD_BYTES` increments is two
/// orders of magnitude over it. Batches are therefore bounded by cumulative
/// encoded size as well as record count, with headroom for the request
/// envelope and separators. Surfaced by the first live catch-up: large
/// session files 413'd until this cap existed.
const MAX_BATCH_BYTES: usize = 1_500_000;

/// Split ordered records into contiguous batches that respect both the
/// server's `MAX_INGEST_BATCH` record count and the HTTP body budget. A
/// single record larger than the budget still gets its own batch (the
/// per-record `MAX_RECORD_BYTES` cap keeps it under the server body limit).
fn size_bounded_batches(
    records: &[bro_capabilities::RecordEnvelope],
) -> Vec<&[bro_capabilities::RecordEnvelope]> {
    let mut batches = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0usize;
    for (index, record) in records.iter().enumerate() {
        let encoded = serde_json::to_vec(record)
            .map(|body| body.len())
            .unwrap_or(MAX_BATCH_BYTES);
        let count = index - start;
        if count > 0 && (count >= MAX_BATCH_RECORDS || bytes + encoded > MAX_BATCH_BYTES) {
            batches.push(&records[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += encoded;
    }
    if start < records.len() {
        batches.push(&records[start..]);
    }
    batches
}

/// Extract the highest `byte_end` in a batch (the batch tiles one stream
/// contiguously, so the last record carries the batch tail).
fn batch_end_offset(batch: &[bro_capabilities::RecordEnvelope]) -> u64 {
    batch
        .iter()
        .filter_map(|record| record.payload.get("byte_end").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0)
}

fn discover_blocking(
    claude: Option<ClaudeTranscriptAdapter>,
    claude_roots: &BTreeMap<String, PathBuf>,
    codex: Option<CodexTranscriptAdapter>,
    codex_root: Option<&std::path::Path>,
) -> Vec<Discovered> {
    let mut discovered = Vec::new();

    if let Some(adapter) = claude {
        for target in [
            TranscriptScanTarget::Sessions,
            TranscriptScanTarget::History,
        ] {
            let Ok(locations) = adapter.scan_locations(target) else {
                continue;
            };
            for location in locations {
                let Some(account) = location.account.clone() else {
                    continue;
                };
                let Some(root) = claude_roots.get(&account) else {
                    continue;
                };
                let Some(relative_path) = relative_path_under(root, &location.path) else {
                    continue;
                };
                let session_id = match location.storage {
                    TranscriptStorage::HistoryJsonl => HISTORY_SESSION_ID.to_string(),
                    _ => match &location.session_id {
                        Some(id) if !id.is_empty() => id.clone(),
                        _ => continue,
                    },
                };
                discovered.push(Discovered {
                    key: StreamKey {
                        source: source_label(TranscriptSource::Claude),
                        account,
                        session_id,
                        relative_path,
                    },
                    path: location.path,
                });
            }
        }
    }

    if let (Some(adapter), Some(root)) = (codex, codex_root)
        && let Ok(locations) = adapter.scan_locations(TranscriptScanTarget::Sessions)
    {
        for location in locations {
            let Some(relative_path) = relative_path_under(root, &location.path) else {
                continue;
            };
            let session_id = match &location.session_id {
                Some(id) if !id.is_empty() => id.clone(),
                _ => continue,
            };
            discovered.push(Discovered {
                key: StreamKey {
                    source: source_label(TranscriptSource::Codex),
                    account: CODEX_ACCOUNT.to_string(),
                    session_id,
                    relative_path,
                },
                path: location.path,
            });
        }
    }

    discovered
}

fn source_label(source: TranscriptSource) -> String {
    match source {
        TranscriptSource::Claude => "claude".to_string(),
        TranscriptSource::Codex => "codex".to_string(),
        // Gemini and harness sources are out of scope for the v1 inline
        // increment contract; discovery never produces them.
        other => format!("{other:?}").to_lowercase(),
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn record_with_payload(index: usize, payload_bytes: usize) -> bro_capabilities::RecordEnvelope {
        bro_capabilities::RecordEnvelope {
            record_id: format!("r-{index}"),
            producer: "collector:test".into(),
            cursor: (index + 1).to_string(),
            kind: bro_capabilities::TRANSCRIPT_INCREMENT_KIND.into(),
            occurred_at: None,
            subject: None,
            attributes: BTreeMap::new(),
            payload: serde_json::json!({
                "byte_end": (index as u64 + 1) * 10,
                "bytes_b64": "x".repeat(payload_bytes),
            }),
        }
    }

    #[test]
    fn batches_respect_count_and_body_budget_and_preserve_order() {
        // 600KB-encoded records: three would exceed the 1.5MB budget, so
        // batches must hold at most two.
        let records: Vec<_> = (0..5).map(|i| record_with_payload(i, 600_000)).collect();
        let batches = size_bounded_batches(&records);
        assert!(batches.iter().all(|batch| {
            batch.len() <= MAX_BATCH_RECORDS
                && batch
                    .iter()
                    .map(|r| serde_json::to_vec(r).unwrap().len())
                    .sum::<usize>()
                    <= MAX_BATCH_BYTES
        }));
        let flattened: Vec<_> = batches.iter().flat_map(|b| b.iter()).collect();
        assert_eq!(flattened.len(), records.len());
        assert!(
            flattened
                .iter()
                .zip(&records)
                .all(|(a, b)| a.record_id == b.record_id)
        );

        // Tiny records fall back to the count cap.
        let tiny: Vec<_> = (0..300).map(|i| record_with_payload(i, 16)).collect();
        let tiny_batches = size_bounded_batches(&tiny);
        assert_eq!(tiny_batches[0].len(), MAX_BATCH_RECORDS);
        assert_eq!(tiny_batches.len(), 2);

        // A single oversized record still ships alone rather than stalling.
        let big = vec![record_with_payload(0, MAX_BATCH_BYTES + 10)];
        assert_eq!(size_bounded_batches(&big).len(), 1);
    }
}
