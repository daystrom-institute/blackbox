//! One-time archive of retired cutover state.
//!
//! The render and code-source locality cutovers and the Git transport cutover
//! are retired: no daemon loads their markers, receipts, proofs, or evidence.
//! A state directory that ran one of those ceremonies still holds the files,
//! so the first start of a daemon that no longer reads them moves each
//! cutover's files into its own
//! `<state_dir>/cutover-artifacts/<archive prefix><label>/`, where they stay
//! for the operator. Nothing is deleted, parsed, or read back.
//!
//! The pass is idempotent: an archived file has left its live location, so a
//! later start finds nothing and creates nothing, not even the archive
//! directory. A pass interrupted after some moves leaves the rest in place,
//! and the next start archives those into a fresh directory. An existing
//! archive directory or file is never overwritten. A retired path that is a
//! symbolic link moves as the link; its target is untouched.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Directory under the state directory that holds archived cutover state.
pub const CUTOVER_ARTIFACTS_DIR: &str = "cutover-artifacts";

/// The files one retired cutover left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredCutover {
    /// Operator-facing name, for logs.
    pub name: &'static str,
    /// Prefix of each archive directory under [`CUTOVER_ARTIFACTS_DIR`].
    pub archive_prefix: &'static str,
    /// State-directory entries whose name starts with one of these are
    /// retired ceremony state: the installed markers and any receipt, proof,
    /// backup, or report the operator kept beside them.
    pub state_prefixes: &'static [&'static str],
    /// State-directory files that only the retired cutover read.
    pub state_files: &'static [&'static str],
    /// Bro-home files that only the retired cutover read.
    pub bro_home_files: &'static [&'static str],
}

/// The render, code-source, and blame locality cutovers.
pub const RETIRED_LOCALITY_CUTOVERS: RetiredCutover = RetiredCutover {
    name: "locality",
    archive_prefix: "retired-locality-",
    state_prefixes: &[
        "render-locality-cutover",
        "code-source-locality-cutover",
        "blame-locality-cutover",
    ],
    state_files: &["code-source-locality-observations.json"],
    bro_home_files: &["render-locality-observations.json"],
};

/// The Git transport cutover: its marker, receipt, checkout parity proof,
/// and any backup of them the operator kept in the state directory.
pub const RETIRED_GIT_TRANSPORT_CUTOVER: RetiredCutover = RetiredCutover {
    name: "Git transport",
    archive_prefix: "retired-git-transport-",
    state_prefixes: &["git-transport-cutover", "git-transport-checkout-parity"],
    state_files: &[],
    bro_home_files: &[],
};

/// Every retired cutover, in the order a start archives them.
pub const RETIRED_CUTOVERS: &[RetiredCutover] =
    &[RETIRED_LOCALITY_CUTOVERS, RETIRED_GIT_TRANSPORT_CUTOVER];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CutoverRetirement {
    /// The archive directory this pass created; `None` when nothing was
    /// found.
    pub archive_dir: Option<PathBuf>,
    /// File names moved into the archive, sorted.
    pub archived: Vec<String>,
}

/// Move every file `cutover` left under `state_dir` and `bro_home` into a new
/// `<archive prefix><label>` archive directory.
///
/// `label` names the archive (the caller passes a UTC timestamp). An absent
/// `state_dir` or `bro_home` holds nothing to archive.
// Startup migration path; runs before the listener binds, off any tokio
// worker.
#[allow(clippy::disallowed_methods)]
pub fn archive_retired_cutover_state(
    cutover: &RetiredCutover,
    state_dir: &Path,
    bro_home: &Path,
    label: &str,
) -> Result<CutoverRetirement> {
    let mut found = retired_state_entries(cutover, state_dir)?;
    for name in cutover.bro_home_files {
        let path = bro_home.join(name);
        if exists_without_following(&path)? {
            found.push(((*name).to_string(), path));
        }
    }
    if found.is_empty() {
        return Ok(CutoverRetirement::default());
    }
    found.sort();

    let artifacts = state_dir.join(CUTOVER_ARTIFACTS_DIR);
    fs::create_dir_all(&artifacts).with_context(|| format!("creating {}", artifacts.display()))?;
    let archive_dir = create_fresh_archive_dir(&artifacts, cutover.archive_prefix, label)?;
    let mut archived = Vec::with_capacity(found.len());
    for (name, source) in found {
        let target = archive_dir.join(&name);
        move_without_overwrite(&source, &target)
            .with_context(|| format!("archiving {}", source.display()))?;
        sync_directory(source.parent().context("retired file has no parent")?)?;
        archived.push(name);
    }
    sync_directory(&archive_dir)?;
    sync_directory(&artifacts)?;
    Ok(CutoverRetirement {
        archive_dir: Some(archive_dir),
        archived,
    })
}

#[allow(clippy::disallowed_methods)]
fn retired_state_entries(
    cutover: &RetiredCutover,
    state_dir: &Path,
) -> Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", state_dir.display()));
        }
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let retired = cutover
            .state_prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix))
            || cutover.state_files.contains(&name.as_str());
        if retired {
            found.push((name, entry.path()));
        }
    }
    Ok(found)
}

fn exists_without_following(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

#[allow(clippy::disallowed_methods)]
fn create_fresh_archive_dir(artifacts: &Path, prefix: &str, label: &str) -> Result<PathBuf> {
    for attempt in 0..1_000u32 {
        let name = if attempt == 0 {
            format!("{prefix}{label}")
        } else {
            format!("{prefix}{label}-{attempt}")
        };
        let path = artifacts.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    anyhow::bail!(
        "no free {prefix} archive directory name for label {label} under {}",
        artifacts.display()
    )
}

/// Rename `source` to `target`, never replacing an existing `target`. A
/// regular file on another filesystem is copied, synced, and only then
/// removed from its live location.
#[allow(clippy::disallowed_methods)]
fn move_without_overwrite(source: &Path, target: &Path) -> Result<()> {
    if exists_without_following(target)? {
        anyhow::bail!("{} already exists", target.display());
    }
    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            let metadata = fs::symlink_metadata(source)?;
            if !metadata.is_file() {
                return Err(error).context("a non-file retired path is on another filesystem");
            }
            let mut input = fs::File::open(source)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            drop(output);
            fs::remove_file(source)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[allow(clippy::disallowed_methods)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_FILES: &[&str] = &[
        "render-locality-cutover-marker.json",
        "render-locality-cutover-receipt.json",
        "code-source-locality-cutover-marker.json",
        "code-source-locality-cutover-receipt.json",
        "blame-locality-cutover-marker.json",
        "code-source-locality-observations.json",
    ];

    /// State files the retirement must never touch.
    const LIVE_FILES: &[&str] = &[
        "knowledge-transport-cutover-marker.json",
        "git-transport-cutover-marker.json",
        "projects.json",
    ];

    fn archive_locality(state: &Path, bro: &Path, label: &str) -> Result<CutoverRetirement> {
        archive_retired_cutover_state(&RETIRED_LOCALITY_CUTOVERS, state, bro, label)
    }

    fn archive_git(state: &Path, bro: &Path, label: &str) -> Result<CutoverRetirement> {
        archive_retired_cutover_state(&RETIRED_GIT_TRANSPORT_CUTOVER, state, bro, label)
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        state: PathBuf,
        bro: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let bro = state.join("bro");
        fs::create_dir_all(&bro).unwrap();
        Fixture {
            _temp: temp,
            state,
            bro,
        }
    }

    fn archive_contents(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn never_provisioned_state_archives_nothing_and_creates_nothing() {
        let fixture = fixture();
        for name in LIVE_FILES {
            fs::write(fixture.state.join(name), b"{}").unwrap();
        }
        let result = archive_locality(&fixture.state, &fixture.bro, "t").unwrap();
        assert_eq!(result, CutoverRetirement::default());
        assert!(!fixture.state.join(CUTOVER_ARTIFACTS_DIR).exists());
        for name in LIVE_FILES {
            assert!(fixture.state.join(name).is_file(), "{name}");
        }

        // An absent state root and bro home hold nothing either.
        let missing = fixture.state.join("missing");
        let result = archive_locality(&missing, &missing.join("bro"), "t").unwrap();
        assert_eq!(result, CutoverRetirement::default());
        assert!(!missing.exists());
    }

    /// A legacy-shaped state root (markers, the operator's receipts, the
    /// code-source and render evidence stores) moves byte-for-byte into one
    /// archive, even when a marker would no longer parse, and the next pass
    /// finds nothing.
    #[test]
    fn legacy_state_is_archived_once_byte_for_byte() {
        let fixture = fixture();
        for name in LEGACY_FILES {
            fs::write(fixture.state.join(name), format!("{name} bytes")).unwrap();
        }
        // A corrupt marker is archived as bytes, never parsed.
        fs::write(
            fixture.state.join("render-locality-cutover-marker.json"),
            b"{ not json",
        )
        .unwrap();
        // An oversized evidence store moves the same way.
        let oversized = vec![b'x'; 17 * 1024 * 1024];
        fs::write(
            fixture.bro.join("render-locality-observations.json"),
            &oversized,
        )
        .unwrap();
        for name in LIVE_FILES {
            fs::write(fixture.state.join(name), b"{}").unwrap();
        }

        let first = archive_locality(&fixture.state, &fixture.bro, "20260930T000000Z").unwrap();
        let archive = first.archive_dir.clone().unwrap();
        assert_eq!(
            archive,
            fixture
                .state
                .join("cutover-artifacts/retired-locality-20260930T000000Z")
        );
        let mut expected = LEGACY_FILES
            .iter()
            .chain(RETIRED_LOCALITY_CUTOVERS.bro_home_files)
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(first.archived, expected);
        assert_eq!(archive_contents(&archive), expected);
        for name in LEGACY_FILES {
            assert!(!fixture.state.join(name).exists(), "{name} left in place");
        }
        assert!(
            !fixture
                .bro
                .join("render-locality-observations.json")
                .exists()
        );
        assert_eq!(
            fs::read(archive.join("render-locality-cutover-marker.json")).unwrap(),
            b"{ not json"
        );
        assert_eq!(
            fs::read(archive.join("code-source-locality-cutover-receipt.json")).unwrap(),
            b"code-source-locality-cutover-receipt.json bytes"
        );
        assert_eq!(
            fs::read(archive.join("render-locality-observations.json")).unwrap(),
            oversized
        );
        for name in LIVE_FILES {
            assert!(fixture.state.join(name).is_file(), "{name}");
        }

        let second = archive_locality(&fixture.state, &fixture.bro, "20260930T000000Z").unwrap();
        assert_eq!(second, CutoverRetirement::default());
        assert_eq!(
            archive_contents(&fixture.state.join(CUTOVER_ARTIFACTS_DIR)),
            ["retired-locality-20260930T000000Z"]
        );
    }

    /// A file that reappears later (a rollback binary re-ran a ceremony)
    /// archives into a fresh directory; the earlier archive and its files
    /// are never overwritten.
    #[test]
    fn a_later_pass_never_overwrites_an_earlier_archive() {
        let fixture = fixture();
        let marker = "code-source-locality-cutover-marker.json";
        fs::write(fixture.state.join(marker), b"first").unwrap();
        let first = archive_locality(&fixture.state, &fixture.bro, "same")
            .unwrap()
            .archive_dir
            .unwrap();
        fs::write(fixture.state.join(marker), b"second").unwrap();
        let second = archive_locality(&fixture.state, &fixture.bro, "same")
            .unwrap()
            .archive_dir
            .unwrap();
        assert_ne!(first, second);
        assert!(second.ends_with("retired-locality-same-1"));
        assert_eq!(fs::read(first.join(marker)).unwrap(), b"first");
        assert_eq!(fs::read(second.join(marker)).unwrap(), b"second");
        assert!(!fixture.state.join(marker).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_marker_moves_as_the_link_and_keeps_its_target() {
        let fixture = fixture();
        let target = fixture.state.join("elsewhere.json");
        fs::write(&target, b"target").unwrap();
        let link = fixture.state.join("render-locality-cutover-marker.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let archive = archive_locality(&fixture.state, &fixture.bro, "t")
            .unwrap()
            .archive_dir
            .unwrap();
        let moved = archive.join("render-locality-cutover-marker.json");
        assert!(
            fs::symlink_metadata(&moved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"target");
    }

    const GIT_LEGACY_FILES: &[&str] = &[
        "git-transport-cutover-marker.json",
        "git-transport-cutover-receipt.json",
        "git-transport-checkout-parity-proof.json",
        "git-transport-cutover-marker.json.bak",
    ];

    /// A Git transport state root (marker, receipt, parity proof, and an
    /// operator backup) moves byte-for-byte into its own archive. The
    /// knowledge transport marker, the catalog, and reviewed reports already
    /// under `cutover-artifacts/` stay where they are, and the next pass
    /// finds nothing.
    #[test]
    fn git_transport_state_is_archived_once_byte_for_byte() {
        let fixture = fixture();
        for name in GIT_LEGACY_FILES {
            fs::write(fixture.state.join(name), format!("{name} bytes")).unwrap();
        }
        // A marker whose receipt no longer matches is archived as bytes.
        fs::write(
            fixture.state.join("git-transport-cutover-receipt.json"),
            b"{ not json",
        )
        .unwrap();
        let report = fixture
            .state
            .join(CUTOVER_ARTIFACTS_DIR)
            .join("git-20260930/report.json");
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        fs::write(&report, b"report").unwrap();
        for name in ["knowledge-transport-cutover-marker.json", "projects.json"] {
            fs::write(fixture.state.join(name), b"{}").unwrap();
        }

        let first = archive_git(&fixture.state, &fixture.bro, "20260930T000000Z").unwrap();
        let archive = first.archive_dir.clone().unwrap();
        assert_eq!(
            archive,
            fixture
                .state
                .join("cutover-artifacts/retired-git-transport-20260930T000000Z")
        );
        let mut expected = GIT_LEGACY_FILES
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(first.archived, expected);
        assert_eq!(archive_contents(&archive), expected);
        for name in GIT_LEGACY_FILES {
            assert!(!fixture.state.join(name).exists(), "{name} left in place");
        }
        assert_eq!(
            fs::read(archive.join("git-transport-cutover-receipt.json")).unwrap(),
            b"{ not json"
        );
        assert_eq!(
            fs::read(archive.join("git-transport-checkout-parity-proof.json")).unwrap(),
            b"git-transport-checkout-parity-proof.json bytes"
        );
        assert_eq!(fs::read(&report).unwrap(), b"report");
        for name in ["knowledge-transport-cutover-marker.json", "projects.json"] {
            assert!(fixture.state.join(name).is_file(), "{name}");
        }

        let second = archive_git(&fixture.state, &fixture.bro, "20260930T000000Z").unwrap();
        assert_eq!(second, CutoverRetirement::default());
        assert_eq!(
            archive_contents(&fixture.state.join(CUTOVER_ARTIFACTS_DIR)),
            ["git-20260930", "retired-git-transport-20260930T000000Z"]
        );
    }

    #[test]
    fn never_provisioned_git_transport_state_archives_nothing() {
        let fixture = fixture();
        fs::write(fixture.state.join("projects.json"), b"{}").unwrap();
        let result = archive_git(&fixture.state, &fixture.bro, "t").unwrap();
        assert_eq!(result, CutoverRetirement::default());
        assert!(!fixture.state.join(CUTOVER_ARTIFACTS_DIR).exists());
    }

    /// Each retired cutover archives into its own directory, and neither
    /// pass takes the other's files.
    #[test]
    fn each_retired_cutover_archives_only_its_own_files() {
        let fixture = fixture();
        fs::write(
            fixture.state.join("render-locality-cutover-marker.json"),
            b"render",
        )
        .unwrap();
        fs::write(
            fixture.state.join("git-transport-cutover-marker.json"),
            b"git",
        )
        .unwrap();
        let mut archives = Vec::new();
        for cutover in RETIRED_CUTOVERS {
            archives.push(
                archive_retired_cutover_state(cutover, &fixture.state, &fixture.bro, "t").unwrap(),
            );
        }
        assert_eq!(
            archives[0].archived,
            ["render-locality-cutover-marker.json"]
        );
        assert_eq!(archives[1].archived, ["git-transport-cutover-marker.json"]);
        assert!(
            archives[0]
                .archive_dir
                .as_ref()
                .unwrap()
                .ends_with("retired-locality-t")
        );
        assert!(
            archives[1]
                .archive_dir
                .as_ref()
                .unwrap()
                .ends_with("retired-git-transport-t")
        );
    }
}
