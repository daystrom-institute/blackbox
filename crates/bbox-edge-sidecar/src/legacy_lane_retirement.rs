//! One-time removal of edge-store families no reader consumes.
//!
//! The daemon keeps no in-memory edge graph, so the transcript-edge split
//! lanes (`observed/`, `explicit/`), the managed project lane directory
//! (`derived/project/`) and the split-lane migration records (`migrations/`)
//! have no reader. Snapshots, the manifest and `derived/git` stay: they are
//! code-source activation and Git overlay authority.
//!
//! A store-level marker records completion, so every later start is a single
//! stat. A pass interrupted before the marker leaves some families removed
//! and others present; the next pass removes whatever is left. Removal never
//! follows a symlink: a retired path that is a link loses the link, never its
//! target.
//!
//! A host that still walks local checkouts regrows `derived/project/` as the
//! staging input of its local snapshots; that is live state, and the marker
//! keeps this pass from removing it again.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::edge_sidecar::writer_temp_sequence;

/// Name of the completion marker under `<edges>/.versions/`.
pub const LEGACY_LANE_RETIREMENT_MARKER: &str = "legacy-edge-lanes-retired-v1";

/// Edge-root-relative paths this pass removes.
pub const RETIRED_EDGE_FAMILIES: &[&str] =
    &["observed", "explicit", "derived/project", "migrations"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LegacyLaneRetirementStats {
    /// The marker was already present; nothing was inspected.
    pub already_complete: bool,
    /// Retired families that existed and were removed.
    pub families_removed: u64,
    /// Regular files under the removed families.
    pub files_removed: u64,
    /// Bytes of those files.
    pub bytes_removed: u64,
}

pub fn legacy_lane_retirement_marker_path(edges_dir: &Path) -> PathBuf {
    edges_dir
        .join(".versions")
        .join(LEGACY_LANE_RETIREMENT_MARKER)
}

/// Remove every retired edge family under `edges_dir`, then stamp the
/// completion marker.
///
/// An absent edge root is a store that never held a lane: the pass reports
/// nothing and creates nothing, not even the marker.
// Startup migration path; runs before the listener binds, off any tokio
// worker.
#[allow(clippy::disallowed_methods)]
pub fn retire_legacy_edge_lanes(edges_dir: &Path) -> Result<LegacyLaneRetirementStats> {
    let mut stats = LegacyLaneRetirementStats::default();
    match fs::symlink_metadata(edges_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!("edge root {} is not a plain directory", edges_dir.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
        Err(error) => return Err(error.into()),
    }
    let marker = legacy_lane_retirement_marker_path(edges_dir);
    if marker.is_file() {
        stats.already_complete = true;
        return Ok(stats);
    }
    for family in RETIRED_EDGE_FAMILIES {
        let path = edges_dir.join(family);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_dir() {
            let (files, bytes) = measure_tree(&path)?;
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing retired edge family {}", path.display()))?;
            stats.files_removed += files;
            stats.bytes_removed += bytes;
        } else {
            if metadata.is_file() {
                stats.files_removed += 1;
                stats.bytes_removed += metadata.len();
            }
            fs::remove_file(&path)
                .with_context(|| format!("removing retired edge family {}", path.display()))?;
        }
        stats.families_removed += 1;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    stamp_marker(&marker)?;
    Ok(stats)
}

/// Regular files and their bytes under `dir`, without following links.
#[allow(clippy::disallowed_methods)]
fn measure_tree(dir: &Path) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files += 1;
                bytes += entry.metadata()?.len();
            }
        }
    }
    Ok((files, bytes))
}

#[allow(clippy::disallowed_methods)]
fn stamp_marker(marker: &Path) -> Result<()> {
    let parent = marker.parent().context("retirement marker has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = marker.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        writer_temp_sequence()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        file.write_all(LEGACY_LANE_RETIREMENT_MARKER.as_bytes())?;
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
    use super::*;

    const PROJECT: &str = "p_00000000000000000000000000000001";

    fn root() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let edges = directory.path().canonicalize().unwrap().join("edges");
        (directory, edges)
    }

    fn write(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in fs::read_dir(&current).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(path);
                } else {
                    names.push(path.strip_prefix(dir).unwrap().display().to_string());
                }
            }
        }
        names.sort();
        names
    }

    /// A store that never held a lane: nothing to remove, and the pass must
    /// not provision the root or stamp a marker into it.
    #[test]
    fn absent_edge_root_is_left_unprovisioned() {
        let (_directory, edges) = root();

        let stats = retire_legacy_edge_lanes(&edges).unwrap();

        assert_eq!(stats, LegacyLaneRetirementStats::default());
        assert!(!edges.exists(), "the pass must not create the edge root");
    }

    /// A never-provisioned family set on an existing root: nothing is removed
    /// and the marker still records completion.
    #[test]
    fn empty_edge_root_is_only_marked() {
        let (_directory, edges) = root();
        fs::create_dir_all(&edges).unwrap();

        let stats = retire_legacy_edge_lanes(&edges).unwrap();

        assert_eq!(stats.families_removed, 0);
        assert!(legacy_lane_retirement_marker_path(&edges).is_file());
    }

    /// The legacy-shaped store: every retired family goes, with its oversized
    /// and malformed contents, and every live family (snapshots, manifest,
    /// Git source lanes, top-level legacy lanes, other markers) keeps its
    /// bytes. A second start opens nothing.
    #[test]
    fn legacy_shaped_store_loses_only_the_retired_families() {
        let (_directory, edges) = root();
        let big = vec![b'x'; 256 * 1024];
        write(
            &edges.join("observed").join(format!("{PROJECT}.jsonl")),
            &big,
        );
        write(
            &edges.join("observed").join("orphan-project.jsonl"),
            b"not json\n",
        );
        write(
            &edges.join("explicit").join(format!("{PROJECT}.jsonl")),
            b"",
        );
        write(
            &edges
                .join("derived/project")
                .join(format!("{PROJECT}.jsonl")),
            b"{\"kind\":\"CALLS\"}\n",
        );
        write(
            &edges.join("migrations/migrate-p1/staging/observed.jsonl"),
            b"residue\n",
        );
        write(&edges.join("migrations/migrate-p1/manifest.json"), b"{}");
        let kept = [
            format!("derived/git/{PROJECT}.jsonl"),
            "materialized/manifest-index.json".to_string(),
            format!("materialized/workspace/{PROJECT}/snap/project.jsonl"),
            format!("{PROJECT}.jsonl"),
            ".versions/file-touch-edges-purged-v1".to_string(),
        ];
        for path in &kept {
            write(&edges.join(path), b"live\n");
        }

        let stats = retire_legacy_edge_lanes(&edges).unwrap();

        assert_eq!(stats.families_removed, 4);
        assert_eq!(stats.files_removed, 6);
        assert_eq!(
            stats.bytes_removed,
            big.len() as u64 + 9 + 17 + 8 + 2,
            "{stats:?}"
        );
        let mut expected = kept.to_vec();
        expected.push(format!(".versions/{LEGACY_LANE_RETIREMENT_MARKER}"));
        expected.sort();
        assert_eq!(listing(&edges), expected);
        assert!(!edges.join("derived/project").exists());
        assert!(edges.join("derived").is_dir());

        let second = retire_legacy_edge_lanes(&edges).unwrap();
        assert!(second.already_complete);
        assert_eq!(second.families_removed, 0);
        assert_eq!(listing(&edges), expected);
    }

    /// A pass interrupted before the marker leaves some families present; the
    /// next pass removes the rest and marks completion.
    #[test]
    fn interrupted_pass_is_finished_by_the_next_start() {
        let (_directory, edges) = root();
        write(
            &edges.join("explicit").join(format!("{PROJECT}.jsonl")),
            b"row\n",
        );
        fs::create_dir_all(&edges).unwrap();

        let stats = retire_legacy_edge_lanes(&edges).unwrap();

        assert_eq!(stats.families_removed, 1);
        assert!(!edges.join("explicit").exists());
        assert!(legacy_lane_retirement_marker_path(&edges).is_file());
    }

    /// A retired family that is a symlink loses the link; the target it
    /// points at is never read or removed.
    #[test]
    fn a_symlinked_family_loses_the_link_not_its_target() {
        let (directory, edges) = root();
        fs::create_dir_all(&edges).unwrap();
        let outside = directory.path().canonicalize().unwrap().join("outside");
        write(&outside.join("keep.jsonl"), b"outside\n");
        std::os::unix::fs::symlink(&outside, edges.join("observed")).unwrap();

        let stats = retire_legacy_edge_lanes(&edges).unwrap();

        assert_eq!(stats.families_removed, 1);
        assert!(fs::symlink_metadata(edges.join("observed")).is_err());
        assert_eq!(fs::read(outside.join("keep.jsonl")).unwrap(), b"outside\n");
    }
}
