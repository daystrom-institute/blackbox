//! Explicit-root native transcript producer. No corpus or daemon dependency.
//! Raw complete JSONL snapshots are source-owned; the daemon alone parses and
//! indexes them. Losing local working state never advances server authority.
use anyhow::{Context, Result, ensure};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_transcript_source::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub source: NativeSource,
    pub account: String,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub corpus_url: String,
    pub token_file: PathBuf,
    pub scope: ConnectorScope,
    pub remote_authority: String,
    pub display_name: String,
    pub roots: Vec<RootConfig>,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        ensure!(
            !config.roots.is_empty(),
            "at least one explicit native transcript root is required"
        );
        ensure!(
            config.roots.iter().all(|root| root.path.is_absolute()),
            "transcript roots must be absolute"
        );
        let mut identities = BTreeSet::new();
        for root in &config.roots {
            ensure!(
                identities.insert(format!("{:?}/{}", root.source, root.account)),
                "duplicate transcript source/account root identity"
            );
        }
        validate_url(&config.corpus_url)?;
        Ok(config)
    }
}
fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "corpus URL must not carry credentials, query, or fragment"
    );
    let local = matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    ensure!(
        parsed.scheme() == "https" || (parsed.scheme() == "http" && local),
        "native transcript transport requires HTTPS except loopback fixtures"
    );
    Ok(())
}

pub struct Client {
    url: String,
    http: reqwest::Client,
    token: bro_rpc::ServiceToken,
}
impl Client {
    pub fn new(config: &Config) -> Result<Self> {
        validate_url(&config.corpus_url)?;
        Ok(Self {
            url: config.corpus_url.trim_end_matches('/').into(),
            token: bro_rpc::ServiceToken::load(&config.token_file)?,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()?,
        })
    }
    pub async fn post<T: Serialize + ?Sized, R: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        request: &T,
    ) -> Result<R> {
        decode(
            self.http
                .post(format!(
                    "{}/internal/transcript-source/v1/{route}",
                    self.url
                ))
                .bearer_auth(self.token.expose_secret())
                .json(request)
                .send()
                .await?,
        )
        .await
    }
    pub async fn onboard(&self, config: &Config) -> Result<serde_json::Value> {
        self.post(
            "onboard",
            &OnboardRequest {
                scope: config.scope.clone(),
                remote_authority: config.remote_authority.clone(),
                display_name: config.display_name.clone(),
            },
        )
        .await
    }
    async fn chunk(&self, snapshot: &StreamSnapshot, hash: &str, bytes: Vec<u8>) -> Result<()> {
        decode(
            self.http
                .put(format!(
                    "{}/internal/transcript-source/v1/chunks/{}/{}/{hash}",
                    self.url,
                    snapshot.scope.connector_source_id(),
                    snapshot.stream_id
                ))
                .bearer_auth(self.token.expose_secret())
                .body(bytes)
                .send()
                .await?,
        )
        .await
    }
}
#[derive(Debug)]
pub struct TransportError {
    pub status: u16,
    pub code: String,
}
impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "native transcript transport returned HTTP {} ({})",
            self.status, self.code
        )
    }
}
impl std::error::Error for TransportError {}
impl TransportError {
    /// The daemon's storage admission refusal (`507 Insufficient Storage`).
    pub fn is_storage_exhausted(&self) -> bool {
        self.status == 507 && self.code == "storage_exhausted"
    }
}
fn storage_exhausted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<TransportError>()
            .is_some_and(TransportError::is_storage_exhausted)
    })
}

async fn decode<R: serde::de::DeserializeOwned>(mut response: reqwest::Response) -> Result<R> {
    let status = response.status();
    if !status.is_success() {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > 4096 {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        // Only expose a bounded machine code. Server/proxy bodies may carry
        // paths or credentials and never belong in collector diagnostics.
        let code = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value["code"].as_str().map(str::to_owned))
            .filter(|code| {
                !code.is_empty()
                    && code.len() <= 128
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
            })
            .unwrap_or_else(|| "unclassified_transport_error".into());
        return Err(TransportError {
            status: status.as_u16(),
            code,
        }
        .into());
    }
    Ok(response.json().await?)
}

pub struct Captured {
    pub snapshot: StreamSnapshot,
    path: PathBuf,
    source_length: u64,
    modified: std::time::SystemTime,
}
#[derive(Deserialize)]
struct SessionMetadata {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<SessionPayload>,
}
#[derive(Deserialize)]
struct SessionPayload {
    id: Option<String>,
}

/// Hash complete source bytes in 1MiB buffers. No full-session buffer exists,
/// even for a 1GiB backfill stream. Unknown JSON fields are skipped while
/// finding session metadata instead of materialized as arbitrary Values.
pub fn capture(root: &RootConfig, path: &Path, scope: &ConnectorScope) -> Result<Option<Captured>> {
    let base = root.path.canonicalize()?;
    let canonical = path.canonicalize()?;
    ensure!(
        canonical.starts_with(&base),
        "transcript escaped configured root"
    );
    let relative = canonical.strip_prefix(&base)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= MAX_STREAM_BYTES,
        "native transcript is not a bounded regular file"
    );
    let mut cursor = before.len();
    let complete_length = loop {
        if cursor == 0 {
            return Ok(None);
        }
        let start = cursor.saturating_sub(CHUNK_BYTES as u64);
        file.seek(SeekFrom::Start(start))?;
        let mut tail = vec![0; (cursor - start) as usize];
        file.read_exact(&mut tail)?;
        if let Some(index) = tail.iter().rposition(|byte| *byte == b'\n') {
            break start + index as u64 + 1;
        }
        cursor = start;
    };
    file.seek(SeekFrom::Start(0))?;
    let metadata = serde_json::Deserializer::from_reader(std::io::BufReader::new(
        (&file).take(complete_length),
    ))
    .into_iter::<SessionMetadata>();
    let mut session = None;
    for value in metadata.take(100) {
        let value = value.context("invalid native transcript metadata")?;
        session = match root.source {
            NativeSource::Claude => value.session_id,
            NativeSource::Codex => {
                if value.kind.as_deref() == Some("session_meta") {
                    value.payload.and_then(|payload| payload.id)
                } else {
                    None
                }
            }
        };
        if session.is_some() {
            break;
        }
    }
    let session = session.ok_or_else(|| {
        anyhow::anyhow!("native session identity was not present in its first 100 records")
    })?;
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = complete_length;
    let mut digest = Sha256::new();
    let mut chunks = Vec::new();
    let mut buffer = vec![0; CHUNK_BYTES];
    while remaining > 0 {
        let size = remaining.min(CHUNK_BYTES as u64) as usize;
        file.read_exact(&mut buffer[..size])?;
        digest.update(&buffer[..size]);
        chunks.push(ChunkRef {
            sha256: sha256(&buffer[..size]),
            size: size as u64,
        });
        remaining -= size as u64;
    }
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "native transcript changed during capture; retry next cycle"
    );
    let stream_id = sha256(
        format!(
            "{:?}/{}/{}",
            root.source,
            root.account,
            relative.to_string_lossy()
        )
        .as_bytes(),
    );
    let snapshot = StreamSnapshot {
        schema_version: SCHEMA_VERSION,
        scope: scope.clone(),
        stream_id,
        source: root.source,
        account: root.account.clone(),
        session_id: session,
        is_subagent: relative
            .components()
            .any(|part| part.as_os_str() == "subagents"),
        content_sha256: format!("{:x}", digest.finalize()),
        byte_length: complete_length,
        chunks,
    };
    snapshot.validate()?;
    Ok(Some(Captured {
        snapshot,
        path: canonical,
        source_length: after.len(),
        modified: after.modified()?,
    }))
}
/// Preserve transport causes (including OS errors) in host diagnostics without
/// letting a long chain dominate the watch log. HTTP failures have already
/// been reduced to a validated machine code by `decode`; never log bodies,
/// request headers, or credentials here.
pub fn error_diagnostic(error: &anyhow::Error) -> String {
    const MAX_CHARS: usize = 4096;
    let chain = format!("{error:#}");
    let mut chars = chain.chars();
    let mut bounded: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        bounded.push_str(" [diagnostic truncated]");
    }
    bounded
}

pub type CycleReport = ScanSummary;
/// No mtime checkpoint or local cursor can suppress server reconciliation:
/// every discovered stream compares with the durable server generation.
///
/// A daemon storage refusal aborts the cycle: no further stream is walked and
/// no completion contact is posted, because a completion records a finished
/// walk. The next cycle's scan-start contact replaces the in-progress scan.
pub async fn publish_cycle(config: &Config, client: &Client) -> Result<CycleReport> {
    let scan_id =
        sha256(format!("{}:{:?}", std::process::id(), std::time::SystemTime::now()).as_bytes());
    let _: SourceContact = client
        .post(
            "contact",
            &ContactRequest {
                scope: config.scope.clone(),
                scan_id: scan_id.clone(),
                completed: None,
            },
        )
        .await
        .context("recording native transcript scan start failed")?;
    let mut report = CycleReport::default();
    for root in &config.roots {
        ensure!(
            root.path.is_dir(),
            "configured transcript root is unavailable"
        );
        for entry in walkdir::WalkDir::new(&root.path)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry?;
            if !entry.file_type().is_file()
                || entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                || entry.file_name() == "history.jsonl"
            {
                continue;
            }
            report.discovered += 1;
            let captured = match capture(root, entry.path(), &config.scope) {
                Ok(Some(captured)) => captured,
                Ok(None) => {
                    report.deferred += 1;
                    continue;
                }
                Err(error) => {
                    report.failed += 1;
                    eprintln!(
                        "native transcript capture failed: {}",
                        error_diagnostic(&error)
                    );
                    continue;
                }
            };
            let result = publish_captured(client, &captured).await;
            match result {
                Ok((false, _)) => report.unchanged += 1,
                Ok((true, bytes)) => {
                    report.published += 1;
                    report.uploaded_bytes += bytes;
                }
                Err(error) if storage_exhausted(&error) => {
                    return Err(error.context(format!(
                        "daemon refused native transcript storage (storage_exhausted) on stream \
                         {} ({:?}/{}); cycle aborted without recording scan completion",
                        captured.snapshot.stream_id, root.source, root.account
                    )));
                }
                Err(error) => {
                    report.failed += 1;
                    eprintln!(
                        "native transcript publication failed: {}",
                        error_diagnostic(&error)
                    );
                }
            }
        }
    }
    let _: SourceContact = client
        .post(
            "contact",
            &ContactRequest {
                scope: config.scope.clone(),
                scan_id,
                completed: Some(report.clone()),
            },
        )
        .await
        .context("Source snapshots may be admitted, but recording the completed scan failed")?;
    Ok(report)
}
async fn publish_captured(client: &Client, captured: &Captured) -> Result<(bool, u64)> {
    let snapshot = &captured.snapshot;
    let status: StreamStatus = client
        .post(
            "status",
            &StreamQuery {
                scope: snapshot.scope.clone(),
                stream_id: snapshot.stream_id.clone(),
            },
        )
        .await?;
    let generation = snapshot.generation()?;
    if status.generation.as_deref() == Some(&generation) {
        return Ok((false, 0));
    }
    let missing: Vec<String> = client.post("missing", snapshot).await?;
    let mut missing: BTreeSet<_> = missing.into_iter().collect();
    let mut uploaded = 0;
    let mut source = std::fs::File::open(&captured.path)?;
    ensure!(
        source.metadata()?.len() == captured.source_length
            && source.metadata()?.modified()? == captured.modified,
        "native source changed before upload; retry capture"
    );
    for (index, chunk) in snapshot.chunks.iter().enumerate() {
        if missing.remove(&chunk.sha256) {
            source.seek(SeekFrom::Start(index as u64 * CHUNK_BYTES as u64))?;
            let mut bytes = vec![0; chunk.size as usize];
            source.read_exact(&mut bytes)?;
            ensure!(
                sha256(&bytes) == chunk.sha256,
                "native source changed during upload; retry capture"
            );
            uploaded += bytes.len() as u64;
            client.chunk(snapshot, &chunk.sha256, bytes).await?;
        }
    }
    ensure!(
        source.metadata()?.len() == captured.source_length
            && source.metadata()?.modified()? == captured.modified,
        "native source changed during upload; retry capture"
    );
    ensure!(
        missing.is_empty(),
        "server requested a chunk outside the offered snapshot"
    );
    let receipt: PublishReceipt = client
        .post(
            "publish",
            &PublishRequest {
                snapshot: snapshot.clone(),
                expected_generation: status.generation,
            },
        )
        .await?;
    ensure!(
        receipt.durable
            && receipt.generation == generation
            && receipt.byte_length == snapshot.byte_length,
        "native source receipt did not confirm the offered snapshot"
    );
    Ok((true, uploaded))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> ConnectorScope {
        ConnectorScope::try_new("csrc_0123456789abcdef", "native_transcript").unwrap()
    }
    #[test]
    fn explicit_root_capture_preserves_session_identity_and_only_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let path = base.join("rollout.jsonl");
        let complete = serde_json::json!({"type":"session_meta", "payload":{"id":"native-fixture", "cwd":"/producer/project"}}).to_string() + "\n";
        std::fs::write(&path, format!("{complete}{{\"unfinished\":")).unwrap();
        let root = RootConfig {
            source: NativeSource::Codex,
            account: "fixture".into(),
            path: base,
        };
        let captured = capture(&root, &path, &scope()).unwrap().unwrap();
        assert_eq!(captured.snapshot.byte_length, complete.len() as u64);
        assert_eq!(
            captured.snapshot.content_sha256,
            sha256(complete.as_bytes())
        );
        assert_eq!(captured.snapshot.session_id, "native-fixture");
        let first_generation = captured.snapshot.generation().unwrap();
        let changed = complete.replace("native-fixture", "native-changed");
        std::fs::write(&path, &changed).unwrap();
        assert_ne!(
            capture(&root, &path, &scope())
                .unwrap()
                .unwrap()
                .snapshot
                .generation()
                .unwrap(),
            first_generation
        );
    }
    #[test]
    fn public_plaintext_and_credential_urls_are_refused() {
        assert!(validate_url("https://corpus.example").is_ok());
        assert!(validate_url("http://127.0.0.1:7264").is_ok());
        assert!(validate_url("http://corpus.example").is_err());
        assert!(validate_url("https://secret@corpus.example").is_err());
        assert!(validate_url("https://corpus.example?token=secret").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_escape_the_explicit_transcript_root() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let allowed = base.join("allowed");
        std::fs::create_dir(&allowed).unwrap();
        let foreign = base.join("outside.jsonl");
        std::fs::write(&foreign, "{}\n").unwrap();
        let link = allowed.join("link.jsonl");
        std::os::unix::fs::symlink(&foreign, &link).unwrap();
        assert!(
            capture(
                &RootConfig {
                    source: NativeSource::Claude,
                    account: "fixture".into(),
                    path: allowed
                },
                &link,
                &scope()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::error_diagnostic;

    #[test]
    fn watch_diagnostic_preserves_context_and_os_cause() {
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(13))
            .context("connection failed")
            .context("recording native transcript scan start failed");
        let diagnostic = error_diagnostic(&error);
        assert!(diagnostic.contains("recording native transcript scan start failed"));
        assert!(diagnostic.contains("connection failed"));
        assert!(diagnostic.contains("os error 13"));
    }

    #[test]
    fn watch_diagnostic_bounds_multibyte_messages() {
        let error = anyhow::anyhow!("界".repeat(5000));
        let diagnostic = error_diagnostic(&error);
        assert_eq!(
            diagnostic.chars().count(),
            4096 + " [diagnostic truncated]".len()
        );
        assert!(diagnostic.ends_with(" [diagnostic truncated]"));
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};

    const ROUTE_PREFIX: &str = "/internal/transcript-source/v1/";

    /// Answer the `nth` (1-based) request to `route` with `status`/`code`.
    #[derive(Clone, Copy)]
    struct Refusal {
        route: &'static str,
        nth: usize,
        status: u16,
        code: &'static str,
    }
    #[derive(Default)]
    struct Recorded {
        /// Every request as (route, stream id), in arrival order.
        requests: Vec<(String, String)>,
        contacts: Vec<ContactRequest>,
    }

    /// Loopback stub of the daemon's transcript source routes. Every stream
    /// is new and asks for all of its chunks, so each stream exercises
    /// status, missing, chunk upload and publish.
    fn stub_daemon(refusal: Option<Refusal>) -> (String, Arc<Mutex<Recorded>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let sink = recorded.clone();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let mut connection = connection.unwrap();
                let mut reader = BufReader::new(connection.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let target = request_line.split(' ').nth(1).unwrap().to_owned();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let path = target.strip_prefix(ROUTE_PREFIX).unwrap();
                let route = path.split('/').next().unwrap().to_owned();
                let mut recorded = sink.lock().unwrap();
                let (stream_id, response) = match route.as_str() {
                    "contact" => {
                        let contact: ContactRequest = serde_json::from_slice(&body).unwrap();
                        let response = serde_json::to_value(SourceContact {
                            last_contact_at: "2026-01-01T00:00:00Z".into(),
                            scan_id: contact.scan_id.clone(),
                            scan_started_at: "2026-01-01T00:00:00Z".into(),
                            scan_in_progress: contact.completed.is_none(),
                            last_completed_scan_at: None,
                            last_completed_scan: contact.completed.clone(),
                        })
                        .unwrap();
                        recorded.contacts.push(contact);
                        (String::new(), response)
                    }
                    "status" => {
                        let query: StreamQuery = serde_json::from_slice(&body).unwrap();
                        let response = serde_json::to_value(StreamStatus {
                            stream_id: query.stream_id.clone(),
                            generation: None,
                            byte_length: 0,
                            published_at: None,
                        })
                        .unwrap();
                        (query.stream_id, response)
                    }
                    "missing" => {
                        let snapshot: StreamSnapshot = serde_json::from_slice(&body).unwrap();
                        let hashes: Vec<_> =
                            snapshot.chunks.iter().map(|chunk| &chunk.sha256).collect();
                        (snapshot.stream_id.clone(), serde_json::json!(hashes))
                    }
                    "chunks" => (
                        path.split('/').nth(2).unwrap().to_owned(),
                        serde_json::Value::Null,
                    ),
                    "publish" => {
                        let request: PublishRequest = serde_json::from_slice(&body).unwrap();
                        let response = serde_json::to_value(PublishReceipt {
                            generation: request.snapshot.generation().unwrap(),
                            locator: "fixture".into(),
                            byte_length: request.snapshot.byte_length,
                            durable: true,
                        })
                        .unwrap();
                        (request.snapshot.stream_id, response)
                    }
                    other => panic!("unexpected collector route {other}"),
                };
                recorded.requests.push((route.clone(), stream_id));
                let seen = recorded
                    .requests
                    .iter()
                    .filter(|(seen, _)| *seen == route)
                    .count();
                drop(recorded);
                let (status, response) = match refusal {
                    Some(refusal) if refusal.route == route && refusal.nth == seen => (
                        refusal.status,
                        serde_json::json!({"code": refusal.code, "message": "refused"}),
                    ),
                    _ => (200, response),
                };
                let response = response.to_string();
                write!(
                    connection,
                    "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
        });
        (url, recorded)
    }

    /// Three Claude streams walked in file-name order, under a canonicalized
    /// per-test root. The token file lives in the same tempdir, never HOME.
    fn fixture(url: String) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(
                root.join(format!("{name}.jsonl")),
                serde_json::json!({"sessionId": format!("session-{name}")}).to_string() + "\n",
            )
            .unwrap();
        }
        let token_file = base.join("token");
        std::fs::write(&token_file, "7".repeat(64)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = Config {
            corpus_url: url,
            token_file,
            scope: ConnectorScope::try_new("csrc_0123456789abcdef", "native_transcript").unwrap(),
            remote_authority: "fixture-host".into(),
            display_name: "fixture".into(),
            roots: vec![RootConfig {
                source: NativeSource::Claude,
                account: "fixture".into(),
                path: root,
            }],
        };
        (dir, config)
    }

    fn streams_requested(recorded: &Recorded, route: &str) -> Vec<String> {
        recorded
            .requests
            .iter()
            .filter(|(seen, _)| seen == route)
            .map(|(_, stream)| stream.clone())
            .collect()
    }

    async fn assert_storage_refusal_aborts_cycle(route: &'static str) {
        let (url, recorded) = stub_daemon(Some(Refusal {
            route,
            nth: 2,
            status: 507,
            code: "storage_exhausted",
        }));
        let (_dir, config) = fixture(url);
        let client = Client::new(&config).unwrap();
        let error = publish_cycle(&config, &client).await.unwrap_err();
        let diagnostic = error_diagnostic(&error);
        {
            let recorded = recorded.lock().unwrap();
            let statuses = streams_requested(&recorded, "status");
            assert_eq!(
                statuses.len(),
                2,
                "no stream after the refused one: {route}"
            );
            let refused = &statuses[1];
            assert_eq!(
                recorded.requests.last().unwrap(),
                &(route.to_owned(), refused.clone()),
                "the refused request is the cycle's last"
            );
            assert_eq!(recorded.contacts.len(), 1, "only the scan-start contact");
            assert!(recorded.contacts[0].completed.is_none());
            assert!(diagnostic.contains("storage_exhausted"), "{diagnostic}");
            assert!(diagnostic.contains("HTTP 507"), "{diagnostic}");
            assert!(diagnostic.contains(refused.as_str()), "{diagnostic}");
            assert!(diagnostic.contains("without recording scan completion"));
        }

        // The next cycle starts a new scan normally once storage admits ingest.
        let error_count = recorded.lock().unwrap().requests.len();
        let report = publish_cycle(&config, &client).await.unwrap();
        assert_eq!(report.published, 3);
        assert_eq!(report.failed, 0);
        let recorded = recorded.lock().unwrap();
        assert!(recorded.requests.len() > error_count);
        assert_eq!(recorded.contacts.len(), 3);
        assert!(recorded.contacts[1].completed.is_none());
        assert_ne!(recorded.contacts[1].scan_id, recorded.contacts[0].scan_id);
        assert_eq!(recorded.contacts[2].scan_id, recorded.contacts[1].scan_id);
        assert_eq!(recorded.contacts[2].completed.as_ref(), Some(&report));
    }

    #[tokio::test]
    async fn storage_refusal_on_publish_aborts_cycle_without_completion() {
        assert_storage_refusal_aborts_cycle("publish").await;
    }

    #[tokio::test]
    async fn storage_refusal_on_any_stream_request_aborts_cycle_without_completion() {
        for route in ["status", "missing", "chunks"] {
            assert_storage_refusal_aborts_cycle(route).await;
        }
    }

    #[tokio::test]
    async fn other_stream_failures_continue_and_record_completion() {
        for (status, code) in [(500, "internal_error"), (507, "unclassified_storage")] {
            let (url, recorded) = stub_daemon(Some(Refusal {
                route: "publish",
                nth: 2,
                status,
                code,
            }));
            let (_dir, config) = fixture(url);
            let client = Client::new(&config).unwrap();
            let report = publish_cycle(&config, &client).await.unwrap();
            assert_eq!(report.discovered, 3);
            assert_eq!(report.published, 2);
            assert_eq!(report.failed, 1);
            let recorded = recorded.lock().unwrap();
            assert_eq!(streams_requested(&recorded, "status").len(), 3);
            assert_eq!(streams_requested(&recorded, "publish").len(), 3);
            assert_eq!(recorded.contacts.len(), 2);
            assert!(recorded.contacts[0].completed.is_none());
            assert_eq!(recorded.contacts[1].completed.as_ref(), Some(&report));
        }
    }
}
