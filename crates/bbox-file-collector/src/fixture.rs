//! A filesystem-backed fixture connector: no network, no OAuth, no vendor
//! code (design phase 1).
//!
//! Its "remote" is a local directory. That is what makes phase 1 a safe
//! substrate proof: every other connector differs from this one only in how
//! bytes and change signals are acquired, so exercising the satellite against
//! this adapter exercises the whole publication path with no external-service
//! dependency and no credential anywhere.
//!
//! It is a real connector, not a test double. Per the design's non-goals, "a
//! fixture connector runs in the satellite like every other connector" -- the
//! daemon gains no fixture-specific branch and no fetch path.
//!
//! The fixture deliberately models the three things that make real stores
//! hard, so the substrate is proven against them rather than against an easy
//! case:
//!
//! - **synthetic stable ids and versions**, with the version derived from
//!   CONTENT so that touching a file's metadata does not republish it and
//!   changing its bytes does, even at identical size;
//! - **native documents**: a `.fixturedoc` has no canonical bytes and must be
//!   EXPORTED before it has any, exactly like a Google Doc;
//! - **checkpoint invalidation on demand**, so convergence after a cursor
//!   expiry is a deterministic test rather than a wait for a vendor to expire
//!   something.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connector::{
    ChangeBatch, CheckpointInvalidation, CheckpointSet, FetchOutcome, FetchedContent, Observation,
    RemoteEntry, RemoteEntryKind, RemoteInfo, RemoteSourceConnector,
};

pub const FIXTURE_KIND: &str = "fixture";
/// The single enumeration stream this connector keeps. Named, because the
/// checkpoint set is a named set: a store with several drives would keep
/// several, and one expiring would not invalidate the others.
pub const FIXTURE_CHECKPOINT: &str = "root";
/// Extension marking a provider-native document: content with no canonical
/// bytes, which the connector renders on demand.
pub const NATIVE_DOCUMENT_EXTENSION: &str = "fixturedoc";
/// The media type the fixture reports for a native document. It maps through
/// the export map onto `pdf`, a format the chunker registry already claims.
pub const NATIVE_DOCUMENT_MEDIA_TYPE: &str = "application/vnd.bbox-fixture.drawing";

const CONTROL_FILE: &str = ".fixture-state.json";

/// Operator/test-controlled fixture state, read fresh on every observation.
///
/// It lives in the fixture "remote" rather than in the satellite because it
/// models VENDOR behavior: a real store decides when to expire a delta token,
/// and the satellite only reacts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FixtureControl {
    /// Stable remote ids by scope-relative path. Absent paths derive an id
    /// from the path itself. Supplying ids explicitly is what lets a fixture
    /// model a RENAME (same id, new path) rather than a delete plus an add.
    pub ids: BTreeMap<String, String>,
    /// When set, the next observation reports THIS checkpoint stream as
    /// invalidated instead of returning a batch.
    pub invalidate_checkpoint: Option<String>,
    pub invalidate_cause: Option<String>,
    /// The vendor tenant this fixture claims, checked against the grant.
    pub remote_authority: Option<String>,
    pub remote_root_id: Option<String>,
    pub remote_display_name: Option<String>,
}

pub struct FixtureConnector {
    root: PathBuf,
}

impl FixtureConnector {
    /// Open a fixture connector over a local directory.
    ///
    /// The root is canonicalized once here, and every later read is confined
    /// beneath it.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalizing fixture root {}", root.as_ref().display()))?;
        if !root.is_dir() {
            bail!("fixture root must be a directory: {}", root.display());
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn control(&self) -> Result<FixtureControl> {
        let path = self.root.join(CONTROL_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(FixtureControl::default())
            }
            Err(error) => Err(error).context(format!("reading {}", path.display())),
        }
    }

    /// Walk the fixture root into remote entries.
    ///
    /// Symlinks are never followed and special files are never read, matching
    /// the code collector's own walk discipline: a connector must not be a
    /// path by which a producer host's filesystem escapes its configured
    /// scope.
    fn enumerate(&self, control: &FixtureControl) -> Result<Vec<RemoteEntry>> {
        let mut entries = Vec::new();
        self.walk(&self.root, &mut Vec::new(), control, &mut entries)?;
        entries.sort_by(|left, right| left.remote_id.cmp(&right.remote_id));
        Ok(entries)
    }

    fn walk(
        &self,
        dir: &Path,
        name_path: &mut Vec<String>,
        control: &FixtureControl,
        out: &mut Vec<RemoteEntry>,
    ) -> Result<()> {
        let mut children: Vec<_> = std::fs::read_dir(dir)
            .with_context(|| format!("listing {}", dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            if name == CONTROL_FILE {
                continue;
            }
            let metadata = std::fs::symlink_metadata(child.path())?;
            if metadata.file_type().is_symlink() {
                // Never followed: a symlink in the fixture "remote" is not a
                // document, and following one would let the scope escape.
                continue;
            }
            name_path.push(name);
            if metadata.is_dir() {
                self.walk(&child.path(), name_path, control, out)?;
            } else if metadata.is_file() {
                out.push(self.entry_for(&child.path(), name_path, control)?);
            }
            name_path.pop();
        }
        Ok(())
    }

    fn entry_for(
        &self,
        path: &Path,
        name_path: &[String],
        control: &FixtureControl,
    ) -> Result<RemoteEntry> {
        let relative = name_path.join("/");
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let native = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case(NATIVE_DOCUMENT_EXTENSION));
        let remote_id = control
            .ids
            .get(&relative)
            .cloned()
            .unwrap_or_else(|| synthetic_remote_id(&relative));
        Ok(RemoteEntry {
            remote_id: remote_id.clone(),
            name_path: name_path.to_vec(),
            kind: if native {
                RemoteEntryKind::NativeDocument
            } else {
                RemoteEntryKind::File
            },
            // The version is derived from CONTENT, which is what makes the
            // freshness contract honest: touching a file's mtime or renaming
            // it does not move the version, and changing its bytes does even
            // at an identical size.
            remote_version: format!("v{}", &hex::encode(Sha256::digest(&bytes))[..16]),
            // A native document has NO canonical bytes, so its export size is
            // genuinely unknown before fetching. Reporting `None` is what
            // forces the streamed cap to be the one that bounds it.
            size: if native {
                None
            } else {
                Some(bytes.len() as u64)
            },
            media_type: native.then(|| NATIVE_DOCUMENT_MEDIA_TYPE.to_string()),
            remote_url: Some(format!(
                "https://fixture.invalid/d/{}",
                percent_ish(&remote_id)
            )),
            deleted: false,
        })
    }

    /// Confined read: resolve under the canonical root and refuse anything
    /// that escapes it or is not a regular file.
    fn read_confined(&self, name_path: &[String]) -> Result<Vec<u8>> {
        let mut path = self.root.clone();
        for component in name_path {
            if component.is_empty() || component == "." || component == ".." {
                bail!("fixture name path has a non-normal component");
            }
            path.push(component);
        }
        let metadata =
            std::fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("fixture entry is not a regular file");
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&self.root) {
            bail!("fixture entry escaped its configured root");
        }
        std::fs::read(&canonical).with_context(|| format!("reading {}", canonical.display()))
    }
}

/// A deterministic synthetic id for a path with no explicit id.
pub fn synthetic_remote_id(relative_path: &str) -> String {
    let digest = Sha256::digest(relative_path.as_bytes());
    format!("fx-{}", &hex::encode(digest)[..16])
}

fn percent_ish(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// A checkpoint token for the current enumeration state.
///
/// It digests the whole `(remote_id, remote_version)` set, so "nothing
/// changed" is representable and an incremental observation can return an
/// empty batch honestly rather than re-reporting every entry.
fn checkpoint_token(entries: &[RemoteEntry]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-fixture-checkpoint-v1");
    for entry in entries {
        hasher.update((entry.remote_id.len() as u64).to_be_bytes());
        hasher.update(entry.remote_id.as_bytes());
        hasher.update((entry.remote_version.len() as u64).to_be_bytes());
        hasher.update(entry.remote_version.as_bytes());
    }
    hex::encode(hasher.finalize())
}

#[async_trait]
impl RemoteSourceConnector for FixtureConnector {
    fn kind(&self) -> &'static str {
        FIXTURE_KIND
    }

    async fn validate(&self) -> Result<RemoteInfo> {
        if !self.root.is_dir() {
            bail!(
                "fixture root {} is not a directory; create it or correct the source config",
                self.root.display()
            );
        }
        let control = self.control()?;
        Ok(RemoteInfo {
            remote_authority: control
                .remote_authority
                .unwrap_or_else(|| "fixture.invalid".to_string()),
            remote_root_id: control.remote_root_id,
            remote_display_name: control.remote_display_name,
        })
    }

    async fn observe(&self, checkpoints: &CheckpointSet) -> Result<Observation> {
        let control = self.control()?;
        if let Some(name) = control.invalidate_checkpoint.as_deref() {
            // Only invalidate a stream the caller actually holds; invalidating
            // a stream nobody resumed would be a no-op dressed as a
            // degradation.
            if checkpoints.get(name).is_some() {
                return Ok(Observation::CheckpointInvalidated(CheckpointInvalidation {
                    checkpoint_name: name.to_string(),
                    cause: control
                        .invalidate_cause
                        .unwrap_or_else(|| "checkpoint_expired".to_string()),
                }));
            }
        }
        let entries = self.enumerate(&control)?;
        let token = checkpoint_token(&entries);
        let mut next_checkpoints = CheckpointSet::full_enumeration();
        next_checkpoints.insert(FIXTURE_CHECKPOINT, token.clone())?;

        let resumed = checkpoints.get(FIXTURE_CHECKPOINT);
        if resumed == Some(token.as_str()) {
            // Resumed at the current state: a genuinely empty delta. Not
            // complete, so orphan detection stays disabled for this batch.
            return Ok(Observation::Batch(ChangeBatch {
                entries: Vec::new(),
                next_checkpoints,
                complete: false,
            }));
        }
        // Any other resume point re-enumerates the whole scope. A store with a
        // real delta API would return only changes here; the fixture is
        // honest that it computed a COMPLETE view, which is what licenses
        // orphan detection.
        Ok(Observation::Batch(ChangeBatch {
            entries,
            next_checkpoints,
            complete: true,
        }))
    }

    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<FetchOutcome> {
        let bytes = self.read_confined(&entry.name_path)?;
        if entry.kind != RemoteEntryKind::NativeDocument {
            if bytes.len() as u64 > max_bytes {
                return Ok(FetchOutcome::Skipped(crate::policy::REASON_OVERSIZE.into()));
            }
            return Ok(FetchOutcome::Fetched(FetchedContent {
                bytes,
                export_format: None,
                media_type: entry.media_type.clone(),
            }));
        }
        // Native export. The connector asks its "vendor" to RENDER the
        // document and receives ordinary bytes in a format the corpus already
        // claims. It performs no chunking, no format sniffing, and holds no
        // knowledge of the chunker registry beyond the static export map.
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let rendered = render_pdf(&text);
        // The one cap that can fire mid-transfer: export size is unknown
        // before fetching, so it cannot be screened on metadata.
        if rendered.len() as u64 > max_bytes {
            return Ok(FetchOutcome::Skipped(
                crate::policy::REASON_EXPORT_OVERSIZE.into(),
            ));
        }
        Ok(FetchOutcome::Fetched(FetchedContent {
            bytes: rendered,
            export_format: Some("pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
        }))
    }
}

/// Render text into a minimal, spec-valid PDF.
///
/// Assembled byte-for-byte with computed xref offsets rather than shipped as a
/// committed binary fixture: a synthetic PDF built in code is reviewable in
/// the diff, cannot carry anything private, and stays honest about what the
/// corpus receives.
pub fn render_pdf(text: &str) -> Vec<u8> {
    fn push_obj(buf: &mut Vec<u8>, offsets: &mut Vec<usize>, num: u32, body: &str) {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        buf.extend_from_slice(body.as_bytes());
        buf.extend_from_slice(b"\nendobj\n");
    }

    // PDF string literals escape backslash and both parentheses; anything
    // outside printable ASCII is dropped rather than encoded, which keeps the
    // fixture's renderer trivial and its output valid.
    let escaped: String = text
        .chars()
        .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        .flat_map(|ch| match ch {
            '\\' => vec!['\\', '\\'],
            '(' => vec!['\\', '('],
            ')' => vec!['\\', ')'],
            other => vec![other],
        })
        .collect();

    let mut buf: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = vec![0];
    buf.extend_from_slice(b"%PDF-1.4\n");
    push_obj(
        &mut buf,
        &mut offsets,
        1,
        "<< /Type /Catalog /Pages 2 0 R >>",
    );
    push_obj(
        &mut buf,
        &mut offsets,
        2,
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    );
    push_obj(
        &mut buf,
        &mut offsets,
        3,
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
         /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
    );
    push_obj(
        &mut buf,
        &mut offsets,
        4,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    );
    let stream = format!("BT /F1 12 Tf 20 100 Td ({escaped}) Tj ET");
    push_obj(
        &mut buf,
        &mut offsets,
        5,
        &format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ),
    );

    let xref_offset = buf.len();
    buf.extend_from_slice(b"xref\n");
    buf.extend_from_slice(format!("0 {}\n", offsets.len()).as_bytes());
    buf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets[1..] {
        buf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF",
            offsets.len()
        )
        .as_bytes(),
    );
    buf
}

/// A minimal, valid single-pixel PNG.
///
/// Built in code for the same reason [`render_pdf`] is: the multimodal gate
/// clause needs a real image the `ximg` chunker claims by magic bytes, and a
/// synthetic one keeps the repo free of committed binaries.
pub fn render_png() -> Vec<u8> {
    fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 12);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        let mut crc_input = Vec::with_capacity(payload.len() + 4);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(payload);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        out
    }

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    // 1x1, 8-bit, colour type 2 (truecolour), no interlace.
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&1_u32.to_be_bytes());
    ihdr.extend_from_slice(&1_u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));
    // One zlib stream (stored deflate block) holding a filter byte plus one
    // RGB pixel.
    let raw = [0_u8, 0x7F, 0x3F, 0x1F];
    let mut zlib = vec![0x78, 0x01, 0x01, 0x04, 0x00, 0xFB, 0xFF];
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    png.extend_from_slice(&chunk(b"IDAT", &zlib));
    png.extend_from_slice(&chunk(b"IEND", &[]));
    png
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in bytes {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, FixtureConnector) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("Ops")).unwrap();
        std::fs::write(root.join("Ops/plan.md"), b"# Plan\nquarterly targets\n").unwrap();
        std::fs::write(root.join("Ops/Runbook.fixturedoc"), b"restart the widget").unwrap();
        std::fs::write(root.join("Ops/diagram.png"), render_png()).unwrap();
        let connector = FixtureConnector::open(&root).unwrap();
        (dir, connector)
    }

    fn write_control(root: &Path, control: &FixtureControl) {
        std::fs::write(
            root.join(CONTROL_FILE),
            serde_json::to_vec_pretty(control).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_first_observation_is_a_complete_enumeration() {
        let (_dir, connector) = fixture();
        let Observation::Batch(batch) = connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        else {
            panic!("a full enumeration must not report invalidation");
        };
        assert!(batch.complete, "a full walk licenses orphan detection");
        assert_eq!(batch.entries.len(), 3);
        assert!(!batch.next_checkpoints.is_full_enumeration());
        assert!(batch.next_checkpoints.get(FIXTURE_CHECKPOINT).is_some());
    }

    #[tokio::test]
    async fn an_unchanged_resume_reports_an_empty_non_complete_delta() {
        let (_dir, connector) = fixture();
        let Observation::Batch(first) = connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        else {
            panic!("expected a batch");
        };
        let Observation::Batch(second) = connector.observe(&first.next_checkpoints).await.unwrap()
        else {
            panic!("expected a batch");
        };
        assert!(
            second.entries.is_empty(),
            "nothing changed, nothing reported"
        );
        assert!(
            !second.complete,
            "an empty DELTA must never license orphan detection"
        );
    }

    #[tokio::test]
    async fn a_content_change_moves_the_version_but_metadata_alone_does_not() {
        let (dir, connector) = fixture();
        let root = dir.path().canonicalize().unwrap();
        let before = match connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        {
            Observation::Batch(batch) => batch.entries,
            _ => panic!("expected a batch"),
        };
        let plan_before = before
            .iter()
            .find(|entry| entry.name_path == ["Ops", "plan.md"])
            .unwrap()
            .clone();

        // Same size, different bytes: the version MUST move.
        std::fs::write(root.join("Ops/plan.md"), b"# Plan\nQUARTERLY targets\n").unwrap();
        let after = match connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        {
            Observation::Batch(batch) => batch.entries,
            _ => panic!("expected a batch"),
        };
        let plan_after = after
            .iter()
            .find(|entry| entry.name_path == ["Ops", "plan.md"])
            .unwrap();
        assert_eq!(plan_before.size, plan_after.size, "same size");
        assert_ne!(
            plan_before.remote_version, plan_after.remote_version,
            "identical size with different bytes must still republish"
        );
    }

    #[tokio::test]
    async fn checkpoint_invalidation_is_reported_not_absorbed() {
        let (dir, connector) = fixture();
        let root = dir.path().canonicalize().unwrap();
        let Observation::Batch(first) = connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        else {
            panic!("expected a batch");
        };
        write_control(
            &root,
            &FixtureControl {
                invalidate_checkpoint: Some(FIXTURE_CHECKPOINT.into()),
                invalidate_cause: Some("token_expired".into()),
                ..FixtureControl::default()
            },
        );
        match connector.observe(&first.next_checkpoints).await.unwrap() {
            Observation::CheckpointInvalidated(invalidation) => {
                assert_eq!(invalidation.checkpoint_name, FIXTURE_CHECKPOINT);
                assert_eq!(invalidation.cause, "token_expired");
            }
            other => panic!("expected an invalidation, got {other:?}"),
        }
        // Discarding the set converges: a full walk always succeeds.
        let Observation::Batch(recovered) = connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        else {
            panic!("a full enumeration must always converge");
        };
        assert!(recovered.complete);
        assert_eq!(recovered.entries.len(), 3);
    }

    #[tokio::test]
    async fn a_native_document_has_no_size_and_exports_to_a_claimed_format() {
        let (_dir, connector) = fixture();
        let Observation::Batch(batch) = connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        else {
            panic!("expected a batch");
        };
        let native = batch
            .entries
            .iter()
            .find(|entry| entry.kind == RemoteEntryKind::NativeDocument)
            .unwrap();
        assert!(
            native.size.is_none(),
            "an export cannot be bounded on metadata"
        );

        let FetchOutcome::Fetched(content) = connector.fetch(native, 1 << 20).await.unwrap() else {
            panic!("expected an export");
        };
        assert_eq!(content.export_format.as_deref(), Some("pdf"));
        assert!(content.bytes.starts_with(b"%PDF-"));
        assert!(content.bytes.ends_with(b"%%EOF"));
    }

    #[tokio::test]
    async fn an_oversized_export_aborts_and_is_counted_rather_than_truncated() {
        let (_dir, connector) = fixture();
        let Observation::Batch(batch) = connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        else {
            panic!("expected a batch");
        };
        let native = batch
            .entries
            .iter()
            .find(|entry| entry.kind == RemoteEntryKind::NativeDocument)
            .unwrap();
        match connector.fetch(native, 8).await.unwrap() {
            FetchOutcome::Skipped(reason) => {
                assert_eq!(reason, crate::policy::REASON_EXPORT_OVERSIZE)
            }
            other => panic!("an oversized export must be skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explicit_ids_model_a_rename_as_one_stable_entry() {
        let (dir, connector) = fixture();
        let root = dir.path().canonicalize().unwrap();
        write_control(
            &root,
            &FixtureControl {
                ids: BTreeMap::from([
                    ("Ops/plan.md".to_string(), "doc-1".to_string()),
                    ("Ops/renamed.md".to_string(), "doc-1".to_string()),
                ]),
                ..FixtureControl::default()
            },
        );
        let before = match connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        {
            Observation::Batch(batch) => batch.entries,
            _ => panic!("expected a batch"),
        };
        let version_before = before
            .iter()
            .find(|entry| entry.remote_id == "doc-1")
            .unwrap()
            .remote_version
            .clone();

        std::fs::rename(root.join("Ops/plan.md"), root.join("Ops/renamed.md")).unwrap();
        let after = match connector
            .observe(&CheckpointSet::full_enumeration())
            .await
            .unwrap()
        {
            Observation::Batch(batch) => batch.entries,
            _ => panic!("expected a batch"),
        };
        let renamed = after
            .iter()
            .find(|entry| entry.remote_id == "doc-1")
            .unwrap();
        assert_eq!(renamed.name_path, vec!["Ops", "renamed.md"]);
        assert_eq!(
            renamed.remote_version, version_before,
            "a rename moves the path, not the bytes, so no refetch is owed"
        );
    }

    #[tokio::test]
    async fn validate_reports_identity_facts_and_refuses_a_missing_root() {
        let (dir, connector) = fixture();
        let root = dir.path().canonicalize().unwrap();
        write_control(
            &root,
            &FixtureControl {
                remote_authority: Some("tenant.example".into()),
                remote_root_id: Some("root-1".into()),
                remote_display_name: Some("Ops shared folder".into()),
                ..FixtureControl::default()
            },
        );
        let info = connector.validate().await.unwrap();
        assert_eq!(info.remote_authority, "tenant.example");
        assert_eq!(info.remote_root_id.as_deref(), Some("root-1"));

        assert!(
            FixtureConnector::open(root.join("does-not-exist")).is_err(),
            "a missing root fails closed with remediation text"
        );
    }

    #[tokio::test]
    async fn symlinks_are_never_enumerated_or_followed() {
        let (dir, connector) = fixture();
        let root = dir.path().canonicalize().unwrap();
        let outside = dir.path().parent().unwrap().join("outside-secret.md");
        std::fs::write(&outside, b"not for the corpus").ok();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("Ops/link.md")).unwrap();
        #[cfg(unix)]
        {
            let Observation::Batch(batch) = connector
                .observe(&CheckpointSet::full_enumeration())
                .await
                .unwrap()
            else {
                panic!("expected a batch");
            };
            assert!(
                batch
                    .entries
                    .iter()
                    .all(|entry| entry.name_path != ["Ops", "link.md"]),
                "a symlink is not a document and is never followed"
            );
        }
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn the_synthetic_png_is_claimed_by_its_magic_bytes() {
        let png = render_png();
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(png.ends_with(&crc32(b"IEND").to_be_bytes()));
    }

    #[test]
    fn the_synthetic_pdf_escapes_string_literals() {
        let pdf = render_pdf("a (b) \\ c");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("\\(b\\)"), "{text}");
        assert!(pdf.starts_with(b"%PDF-1.4"));
    }
}
