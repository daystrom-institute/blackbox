//! Explicit-root native transcript producer. No corpus or daemon dependency.
//! Raw complete JSONL snapshots are source-owned; the daemon alone parses and
//! indexes them. Losing local working state never advances server authority.
use anyhow::{Context, Result, bail, ensure};
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
pub type CycleReport = ScanSummary;
/// No mtime checkpoint or local cursor can suppress server reconciliation:
/// every discovered stream compares with the durable server generation.
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
        .await?;
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
                    eprintln!("native transcript capture failed: {error}");
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
                Err(error) => {
                    report.failed += 1;
                    eprintln!("native transcript publication failed: {error}");
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
        .map_err(|error| {
            anyhow::anyhow!(
                "Source snapshots may be admitted, but recording the completed scan failed: {error}"
            )
        })?;
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
