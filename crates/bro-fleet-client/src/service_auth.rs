//! Private bearer loading for the thin local HTTP client.
//!
//! This deliberately stays client-local so `bro-fleet-client` does not gain a
//! compile edge to the service-side `bro-rpc` transport implementation.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use reqwest::header::HeaderValue;

const TOKEN_HEX_BYTES: usize = 64;

#[allow(clippy::disallowed_methods)]
pub(crate) fn load_or_create(path: &Path) -> anyhow::Result<HeaderValue> {
    if path.exists() {
        return load(path);
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("service token path has no parent"))?;
    ensure_private_directory(parent)?;
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(secret.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::File::open(parent)?.sync_all()?;
            validate_private_file(path)?;
            parse(&secret)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => load(path),
        Err(error) => Err(error).context("creating service token"),
    }
}

#[allow(clippy::disallowed_methods)]
fn load(path: &Path) -> anyhow::Result<HeaderValue> {
    validate_private_file(path)?;
    let secret = std::fs::read_to_string(path).context("reading service token")?;
    parse(secret.trim())
}

pub(crate) fn parse(secret: &str) -> anyhow::Result<HeaderValue> {
    if secret.len() != TOKEN_HEX_BYTES
        || !secret
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("service token must be exactly 64 lowercase hexadecimal bytes");
    }
    let mut header = HeaderValue::from_str(&format!("Bearer {secret}"))
        .context("service token is not a valid authorization header")?;
    header.set_sensitive(true);
    Ok(header)
}

#[allow(clippy::disallowed_methods)]
fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path).context("creating service token directory")?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("service token directory is not a private real directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_uid() {
            bail!("service token directory is owned by another user");
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_private_file(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("service token path is not a private regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_uid()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!("service token path is not an owner-only, singly linked file");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and only reads process credentials.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;

    #[test]
    fn creates_and_reloads_a_sensitive_private_header() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("state/service.token");
        let first = load_or_create(&path).unwrap();
        let second = load_or_create(&path).unwrap();
        assert_eq!(first, second);
        assert!(first.is_sensitive());
        assert_eq!(std::fs::read_to_string(path).unwrap().trim().len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_broad_symlinked_and_hardlinked_files() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let broad = root.join("broad.token");
        std::fs::write(&broad, "a".repeat(TOKEN_HEX_BYTES)).unwrap();
        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&broad).is_err());

        let link = root.join("link.token");
        symlink(&broad, &link).unwrap();
        assert!(load(&link).is_err());

        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&broad, root.join("hardlink.token")).unwrap();
        assert!(load(&broad).is_err());
    }
}
