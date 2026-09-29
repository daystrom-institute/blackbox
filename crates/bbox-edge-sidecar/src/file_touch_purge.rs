//! One-time removal of retired transcript file-touch rows from the durable
//! edge lanes.
//!
//! The durable lanes are the top-level combined `<project>.jsonl` lanes and
//! the `observed/` and `explicit/` split lanes. A row whose kind is a retired
//! file-touch kind is dropped. Every other line, including malformed lines and
//! a final line without a newline, is kept byte for byte, so surviving row
//! identities and catalog backfill evidence do not move. A lane without a
//! file-touch row is never rewritten.
//!
//! A store-level marker records completion. The indexer does not emit these
//! kinds, so a store that carries the marker needs no further pass. A pass
//! interrupted before the marker leaves a partially purged store that the
//! next pass finishes: each lane rewrite is one atomic rename under the
//! project's edge mutation lock, so no lane is ever half written.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::edge_sidecar::{
    RETIRED_FILE_TOUCH_EDGE_KINDS, lock_project_edge_mutation, sidecar_file_stem,
    writer_temp_sequence,
};

/// Name of the completion marker under `<edges>/.versions/`.
pub const FILE_TOUCH_PURGE_MARKER: &str = "file-touch-edges-purged-v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileTouchPurgeStats {
    /// The marker was already present; no lane was opened.
    pub already_complete: bool,
    pub lanes_scanned: u64,
    pub lanes_rewritten: u64,
    pub rows_removed: u64,
    pub bytes_removed: u64,
}

pub fn file_touch_purge_marker_path(edges_dir: &Path) -> PathBuf {
    edges_dir.join(".versions").join(FILE_TOUCH_PURGE_MARKER)
}

/// Remove every retired file-touch row from the durable lanes under
/// `edges_dir`, then stamp the completion marker.
///
/// An absent edge root is a store that never held a lane: the pass reports
/// nothing and creates nothing, not even the marker.
// Startup migration path; runs before the listener binds, off any tokio
// worker.
#[allow(clippy::disallowed_methods)]
pub fn purge_retired_file_touch_edges(edges_dir: &Path) -> Result<FileTouchPurgeStats> {
    let mut stats = FileTouchPurgeStats::default();
    match fs::symlink_metadata(edges_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!("edge root {} is not a plain directory", edges_dir.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
        Err(error) => return Err(error.into()),
    }
    let marker = file_touch_purge_marker_path(edges_dir);
    if marker.is_file() {
        stats.already_complete = true;
        return Ok(stats);
    }
    for lane in durable_lanes(edges_dir)? {
        stats.lanes_scanned += 1;
        let (rows, bytes) = purge_lane(edges_dir, &lane)
            .with_context(|| format!("purging file-touch rows from {}", lane.display()))?;
        if rows > 0 {
            stats.lanes_rewritten += 1;
            stats.rows_removed += rows;
            stats.bytes_removed += bytes;
        }
    }
    stamp_marker(&marker)?;
    Ok(stats)
}

/// Regular `.jsonl` files in the three durable lane directories. Symlinks,
/// nested directories, and every other family (derived, materialized,
/// quarantine, migration staging) are not lanes this pass touches.
#[allow(clippy::disallowed_methods)]
fn durable_lanes(edges_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut lanes = Vec::new();
    for dir in [
        edges_dir.to_path_buf(),
        edges_dir.join("observed"),
        edges_dir.join("explicit"),
    ] {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
                lanes.push(path);
            }
        }
    }
    lanes.sort();
    Ok(lanes)
}

#[derive(Deserialize)]
struct KindProbe<'a> {
    #[serde(borrow)]
    kind: std::borrow::Cow<'a, str>,
}

/// Whether one lane line is a retired file-touch row. Lines that do not parse
/// as an edge are not file-touch rows: the purge keeps them untouched.
fn is_file_touch_row(line: &[u8]) -> bool {
    let named = RETIRED_FILE_TOUCH_EDGE_KINDS.iter().any(|kind| {
        let needle = format!("\"{kind}\"");
        line.windows(needle.len())
            .any(|window| window == needle.as_bytes())
    });
    if !named {
        return false;
    }
    serde_json::from_slice::<KindProbe<'_>>(line)
        .is_ok_and(|probe| RETIRED_FILE_TOUCH_EDGE_KINDS.contains(&probe.kind.as_ref()))
}

/// Visit each line of a lane, newline included, through one descriptor.
fn for_each_line(path: &Path, mut visit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        visit(&line)?;
    }
}

/// Rewrite one lane without its file-touch rows. Returns the rows and bytes
/// removed; a lane with none is left as it is.
#[allow(clippy::disallowed_methods)]
fn purge_lane(edges_dir: &Path, path: &Path) -> Result<(u64, u64)> {
    let _mutation_lock = match sidecar_file_stem(path) {
        Some(stem)
            if bbox_corpus_core::project_catalog::ProjectId::parse(stem.to_owned()).is_ok() =>
        {
            Some(lock_project_edge_mutation(edges_dir, stem)?)
        }
        _ => None,
    };
    let mut holds_file_touch_rows = false;
    for_each_line(path, |line| {
        holds_file_touch_rows |= is_file_touch_row(line);
        Ok(())
    })?;
    if !holds_file_touch_rows {
        return Ok((0, 0));
    }

    let dir = path.parent().context("edge lane has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("edge lane filename is not UTF-8")?;
    // Deliberately not a `.jsonl` name: a crash between creation and rename
    // leaves a partial copy that no lane reader may mistake for a lane.
    let tmp = dir.join(format!(
        "{name}.file-touch-purge.{}.{}.tmp",
        std::process::id(),
        writer_temp_sequence()
    ));
    let result = (|| -> Result<(u64, u64)> {
        let output = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        let mut writer = BufWriter::new(output);
        let mut rows_removed = 0u64;
        let mut bytes_removed = 0u64;
        for_each_line(path, |line| {
            if is_file_touch_row(line) {
                rows_removed += 1;
                bytes_removed += line.len() as u64;
            } else {
                writer.write_all(line)?;
            }
            Ok(())
        })?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        fs::File::open(dir)?.sync_all()?;
        Ok((rows_removed, bytes_removed))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[allow(clippy::disallowed_methods)]
fn stamp_marker(marker: &Path) -> Result<()> {
    let parent = marker.parent().context("purge marker has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = marker.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        writer_temp_sequence()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        file.write_all(FILE_TOUCH_PURGE_MARKER.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, marker)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_corpus_core::entity_ref::EntityRef;

    use super::*;
    use crate::edge_sidecar::Edge;

    const PROJECT: &str = "p_00000000000000000000000000000001";

    fn row(kind: &str, event_idx: u32) -> String {
        let edge = Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "sess-1".into(),
                line_offset: 10,
                event_idx,
            },
            kind: kind.into(),
            target: EntityRef::ProjectFile {
                project_id: PROJECT.into(),
                rel_path_hash: "h1".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::from([
                ("anchor.file_path".to_string(), "src/lib.rs".to_string()),
                ("cwd".to_string(), "/synthetic".to_string()),
            ]),
            project_id: None,
        };
        serde_json::to_string(&edge).unwrap()
    }

    fn root() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let edges = directory.path().canonicalize().unwrap().join("edges");
        (directory, edges)
    }

    fn write_lane(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        for entry in walk(dir) {
            names.push(entry.strip_prefix(dir).unwrap().display().to_string());
        }
        names.sort();
        names
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                paths.extend(walk(&path));
            } else {
                paths.push(path);
            }
        }
        paths
    }

    /// A store that never held a lane: nothing to purge, and the pass must
    /// not provision the root or stamp a marker into it.
    #[test]
    fn absent_edge_root_is_left_unprovisioned() {
        let (_directory, edges) = root();
        assert!(!edges.exists());

        let stats = purge_retired_file_touch_edges(&edges).unwrap();

        assert_eq!(stats, FileTouchPurgeStats::default());
        assert!(!edges.exists(), "the purge must not create the edge root");
    }

    /// Lanes written with file-touch rows lose exactly those rows. Every
    /// surviving line, malformed lines and a final unterminated line
    /// included, keeps its bytes and order; lanes without file-touch rows,
    /// derived lanes, and non-lane files are not touched.
    #[test]
    fn legacy_shaped_store_loses_only_file_touch_rows() {
        let (_directory, edges) = root();
        let bash = row("RAN_BASH", 0);
        let read = row("READ_FILE", 1);
        let edited = row("EDITED_FILE", 2);
        let explicit = row("SUPERSEDES", 3);
        let observed_lane = edges.join("observed").join(format!("{PROJECT}.jsonl"));
        write_lane(
            &observed_lane,
            &format!("{read}\n{bash}\n{edited}\nnot json with \"READ_FILE\"\n{bash}"),
        );
        let explicit_lane = edges.join("explicit").join(format!("{PROJECT}.jsonl"));
        write_lane(&explicit_lane, &format!("{explicit}\n{edited}\n"));
        let legacy_lane = edges.join(format!("{PROJECT}.jsonl"));
        write_lane(&legacy_lane, &format!("{read}\n{explicit}\n"));
        let untouched_lane = edges.join("explicit").join("agents.jsonl");
        let untouched = format!("{explicit}\n");
        write_lane(&untouched_lane, &untouched);
        let derived_lane = edges
            .join("derived")
            .join("project")
            .join(format!("{PROJECT}.jsonl"));
        let derived = format!("{read}\n");
        write_lane(&derived_lane, &derived);
        let backup = edges.join(format!("{PROJECT}.jsonl.bak-migrated-1"));
        write_lane(&backup, &format!("{edited}\n"));
        let untouched_modified = fs::metadata(&untouched_lane).unwrap().modified().unwrap();
        assert!(
            fs::read_to_string(&observed_lane)
                .unwrap()
                .contains("READ_FILE"),
            "the fixture must genuinely carry file-touch rows"
        );

        let stats = purge_retired_file_touch_edges(&edges).unwrap();

        assert!(!stats.already_complete);
        assert_eq!(stats.lanes_scanned, 4);
        assert_eq!(stats.lanes_rewritten, 3);
        assert_eq!(stats.rows_removed, 4);
        assert_eq!(
            fs::read_to_string(&observed_lane).unwrap(),
            format!("{bash}\nnot json with \"READ_FILE\"\n{bash}")
        );
        assert_eq!(
            fs::read_to_string(&explicit_lane).unwrap(),
            format!("{explicit}\n")
        );
        assert_eq!(
            fs::read_to_string(&legacy_lane).unwrap(),
            format!("{explicit}\n")
        );
        assert_eq!(fs::read_to_string(&untouched_lane).unwrap(), untouched);
        assert_eq!(
            fs::metadata(&untouched_lane).unwrap().modified().unwrap(),
            untouched_modified,
            "a lane without file-touch rows is not rewritten"
        );
        assert_eq!(fs::read_to_string(&derived_lane).unwrap(), derived);
        assert_eq!(fs::read_to_string(&backup).unwrap(), format!("{edited}\n"));
        assert!(file_touch_purge_marker_path(&edges).is_file());
        assert!(
            !listing(&edges).iter().any(|name| name.ends_with(".tmp")),
            "no temporary may survive a completed pass: {:?}",
            listing(&edges)
        );
    }

    /// A store with no file-touch rows is scanned once and rewritten
    /// nowhere; once marked, a later pass opens no lane at all and leaves
    /// every byte where it was.
    #[test]
    fn clean_and_already_purged_stores_are_left_byte_identical() {
        let (_directory, edges) = root();
        let bash = row("RAN_BASH", 0);
        let lane = edges.join("observed").join(format!("{PROJECT}.jsonl"));
        write_lane(&lane, &format!("{bash}\n"));
        let before = listing(&edges);
        assert_eq!(before.len(), 1, "the clean fixture must carry a real lane");

        let first = purge_retired_file_touch_edges(&edges).unwrap();
        assert_eq!(first.lanes_scanned, 1);
        assert_eq!(first.lanes_rewritten, 0);
        assert_eq!(first.rows_removed, 0);
        assert_eq!(fs::read_to_string(&lane).unwrap(), format!("{bash}\n"));

        // A file-touch row appearing after completion is not the purge's
        // concern: the marker short-circuits, and readers skip the row.
        let late = format!("{bash}\n{}\n", row("READ_FILE", 9));
        fs::write(&lane, &late).unwrap();
        let second = purge_retired_file_touch_edges(&edges).unwrap();
        assert!(second.already_complete);
        assert_eq!(second.lanes_scanned, 0);
        assert_eq!(fs::read_to_string(&lane).unwrap(), late);
        let mut after = listing(&edges);
        after.retain(|name| !name.starts_with(".versions") && !name.starts_with(".locks"));
        assert_eq!(after, before);
    }

    /// An interrupted pass leaves no marker; the next pass finishes the
    /// remaining lanes and then marks the store.
    #[test]
    fn a_pass_without_the_marker_resumes_over_partially_purged_lanes() {
        let (_directory, edges) = root();
        let bash = row("RAN_BASH", 0);
        let read = row("READ_FILE", 1);
        let purged = edges.join("observed").join(format!("{PROJECT}.jsonl"));
        write_lane(&purged, &format!("{bash}\n"));
        let pending = edges
            .join("observed")
            .join("p_00000000000000000000000000000002.jsonl");
        write_lane(&pending, &format!("{read}\n{bash}\n"));

        let stats = purge_retired_file_touch_edges(&edges).unwrap();

        assert_eq!(stats.lanes_scanned, 2);
        assert_eq!(stats.lanes_rewritten, 1);
        assert_eq!(stats.rows_removed, 1);
        assert_eq!(fs::read_to_string(&pending).unwrap(), format!("{bash}\n"));
        assert!(file_touch_purge_marker_path(&edges).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_lanes_are_not_followed() {
        let (_directory, edges) = root();
        let outside = edges.parent().unwrap().join("outside.jsonl");
        let read = row("READ_FILE", 1);
        fs::write(&outside, format!("{read}\n")).unwrap();
        fs::create_dir_all(edges.join("observed")).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            edges.join("observed").join(format!("{PROJECT}.jsonl")),
        )
        .unwrap();

        let stats = purge_retired_file_touch_edges(&edges).unwrap();

        assert_eq!(stats.lanes_scanned, 0);
        assert_eq!(fs::read_to_string(&outside).unwrap(), format!("{read}\n"));
    }
}
