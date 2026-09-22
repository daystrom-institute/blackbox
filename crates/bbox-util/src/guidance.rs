//! Content-addressed instruction satellites. Publish before their entrypoints.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Component, Path},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuidanceFile {
    /// Relative to the host common-file directory, or the project's .bbox directory.
    pub path: String,
    pub body: String,
}

/// Give a complete, deterministically ordered set an immutable generation.
pub fn generation_files(mut bodies: Vec<(String, String)>) -> Vec<GuidanceFile> {
    bodies.sort_by(|a, b| a.0.cmp(&b.0));
    let generation = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&bodies).expect("string pairs"))
    );
    bodies
        .into_iter()
        .map(|(name, body)| GuidanceFile {
            path: format!("guidance/{generation}/{name}.md"),
            body,
        })
        .collect()
}

pub fn validate_files(files: &[GuidanceFile]) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut groups = BTreeMap::<String, Vec<(String, String)>>::new();
    for file in files {
        let parts: Vec<_> = Path::new(&file.path).components().collect();
        if parts.len() != 3 || parts.iter().any(|p| !matches!(p, Component::Normal(_))) {
            bail!("invalid guidance path: {}", file.path);
        }
        let fields: Vec<_> = file.path.split('/').collect();
        if fields.len() != 3
            || fields[0] != "guidance"
            || fields[1].len() != 64
            || !fields[1].bytes().all(|c| c.is_ascii_hexdigit())
            || !fields[2].ends_with(".md")
            || fields[2].strip_suffix(".md").unwrap_or("").is_empty()
            || fields[2].strip_suffix(".md").unwrap_or("").contains('.')
            || !fields[2]
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
            || !names.insert(file.path.clone())
        {
            bail!("invalid or duplicate guidance path: {}", file.path);
        }
        groups.entry(fields[1].to_string()).or_default().push((
            fields[2].strip_suffix(".md").unwrap_or("").to_string(),
            file.body.clone(),
        ));
    }
    for (generation, bodies) in groups {
        if generation_files(bodies)
            .iter()
            .any(|file| !file.path.starts_with(&format!("guidance/{generation}/")))
        {
            bail!("guidance generation does not match its contents");
        }
    }
    Ok(())
}

/// Inspect every destination before creating anything. Never follow satellite symlinks.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and CLI callers are synchronous"
)]
pub fn preflight_files(root: &Path, files: &[GuidanceFile]) -> Result<()> {
    validate_files(files)?;
    for file in files {
        let target = root.join(&file.path);
        let mut current = std::path::PathBuf::new();
        for part in target.components() {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    bail!("guidance path is a symlink: {}", current.display())
                }
                Ok(meta) if current != target && !meta.is_dir() => {
                    bail!("guidance parent is not a directory: {}", current.display())
                }
                Ok(meta) if current == target && !meta.is_file() => {
                    bail!("guidance target is not a file: {}", current.display())
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).context("checking guidance destination"),
            }
        }
        match fs::read_to_string(&target) {
            Ok(existing) if existing != file.body => {
                bail!("immutable guidance file differs: {}", target.display())
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("reading guidance destination"),
        }
    }
    Ok(())
}

#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and CLI callers are synchronous"
)]
pub fn publish_files(root: &Path, files: &[GuidanceFile], dry_run: bool) -> Result<()> {
    preflight_files(root, files)?;
    if dry_run {
        return Ok(());
    }
    for file in files {
        let target = root.join(&file.path);
        if target.is_file() {
            continue;
        }
        let parent = target.parent().context("guidance target has no parent")?;
        fs::create_dir_all(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(file.body.as_bytes())?;
        staged.as_file().sync_all()?;
        match staged.persist_noclobber(&target) {
            Ok(_) => {}
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::read_to_string(&target)? != file.body {
                    bail!("concurrent guidance file differs");
                }
            }
            Err(err) => return Err(err.into()),
        }
        fs::File::open(parent)?.sync_all()?;
    }
    if !files.is_empty() {
        fs::File::open(root.join("guidance"))?.sync_all()?;
        fs::File::open(root)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous fixture IO isolated to canonical tempdirs"
)]
mod tests {
    use super::*;
    #[test]
    fn generations_are_immutable_and_preflight_prevents_partial_publication() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = generation_files(vec![("retrieval".into(), "exact evidence".into())]);
        publish_files(&root, &files, true).unwrap();
        assert!(!root.join("guidance").exists());
        publish_files(&root, &files, false).unwrap();
        publish_files(&root, &files, false).unwrap();
        let next = generation_files(vec![("retrieval".into(), "new evidence".into())]);
        publish_files(&root, &next, false).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(&files[0].path)).unwrap(),
            "exact evidence"
        );
        let mut tampered = files.clone();
        tampered[0].body = "replacement".into();
        assert!(publish_files(&root, &tampered, false).is_err());
        tampered[0].path = "../outside.md".into();
        assert!(validate_files(&tampered).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn satellite_symlinks_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("guidance")).unwrap();
        let files = generation_files(vec![("retrieval".into(), "evidence".into())]);
        assert!(publish_files(&root, &files, false).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
