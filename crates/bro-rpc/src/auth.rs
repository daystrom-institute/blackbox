//! Same-host HTTP bearer used between differentiated service processes.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use http::header::{AUTHORIZATION, HeaderMap, HeaderValue};

const TOKEN_HEX_BYTES: usize = 64;

#[derive(Clone)]
pub struct ServiceToken {
    authorization: HeaderValue,
}

impl std::fmt::Debug for ServiceToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ServiceToken")
            .field(&"[redacted]")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceTokenError {
    #[error("service token I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("service token path is not a private regular file")]
    UnsafeFile,
    #[error("service token directory is not a private real directory")]
    UnsafeDirectory,
    #[error("service token must be exactly 64 lowercase hexadecimal bytes")]
    InvalidToken,
    #[error("service token cannot be represented as an HTTP authorization header")]
    InvalidHeader,
}

impl ServiceToken {
    pub fn parse(secret: impl Into<String>) -> Result<Self, ServiceTokenError> {
        let secret = secret.into();
        if secret.len() != TOKEN_HEX_BYTES
            || !secret
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ServiceTokenError::InvalidToken);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {secret}"))
            .map_err(|_| ServiceTokenError::InvalidHeader)?;
        authorization.set_sensitive(true);
        Ok(Self { authorization })
    }

    /// Load the shared service token, atomically creating a private one when
    /// this process is the first differentiated service to start.
    #[allow(clippy::disallowed_methods)]
    pub fn load_or_create(path: &Path) -> Result<Self, ServiceTokenError> {
        if path.exists() {
            return Self::load(path);
        }
        let parent = path.parent().ok_or(ServiceTokenError::UnsafeDirectory)?;
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
                Self::parse(secret)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Self::load(path),
            Err(error) => Err(error.into()),
        }
    }

    #[allow(clippy::disallowed_methods)]
    pub fn load(path: &Path) -> Result<Self, ServiceTokenError> {
        validate_private_file(path)?;
        Self::parse(std::fs::read_to_string(path)?.trim().to_string())
    }

    pub fn authorization_header(&self) -> HeaderValue {
        self.authorization.clone()
    }

    pub fn authorizes(&self, headers: &HeaderMap) -> bool {
        let Some(provided) = headers.get(AUTHORIZATION) else {
            return false;
        };
        constant_time_eq(self.authorization.as_bytes(), provided.as_bytes())
    }
}

#[allow(clippy::disallowed_methods)]
fn ensure_private_directory(path: &Path) -> Result<(), ServiceTokenError> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServiceTokenError::UnsafeDirectory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_uid() {
            return Err(ServiceTokenError::UnsafeDirectory);
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[allow(clippy::disallowed_methods)]
fn validate_private_file(path: &Path) -> Result<(), ServiceTokenError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServiceTokenError::UnsafeFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_uid()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(ServiceTokenError::UnsafeFile);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and only reads process credentials.
    unsafe { libc::geteuid() }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let max = left.len().max(right.len());
    for index in 0..max {
        difference |= usize::from(left.get(index).copied().unwrap_or_default())
            ^ usize::from(right.get(index).copied().unwrap_or_default());
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;

    #[test]
    fn token_is_private_redacted_and_required_as_bearer() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("auth/service.token");
        let token = ServiceToken::load_or_create(&path).unwrap();
        let secret = std::fs::read_to_string(&path).unwrap();
        assert!(!format!("{token:?}").contains(secret.trim()));
        let mut headers = HeaderMap::new();
        assert!(!token.authorizes(&headers));
        headers.insert(AUTHORIZATION, token.authorization_header());
        assert!(token.authorizes(&headers));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert!(!token.authorizes(&headers));
        let loaded = ServiceToken::load(&path).unwrap();
        headers.insert(AUTHORIZATION, loaded.authorization_header());
        assert!(token.authorizes(&headers));
    }

    #[cfg(unix)]
    #[test]
    fn broad_symlink_or_hardlinked_token_files_fail_closed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let broad = root.join("broad.token");
        std::fs::write(&broad, "a".repeat(TOKEN_HEX_BYTES)).unwrap();
        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            ServiceToken::load(&broad),
            Err(ServiceTokenError::UnsafeFile)
        ));
        let link = root.join("link.token");
        symlink(&broad, &link).unwrap();
        assert!(matches!(
            ServiceToken::load(&link),
            Err(ServiceTokenError::UnsafeFile)
        ));
        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = root.join("hardlink.token");
        std::fs::hard_link(&broad, &hardlink).unwrap();
        assert!(matches!(
            ServiceToken::load(&broad),
            Err(ServiceTokenError::UnsafeFile)
        ));
    }
}
