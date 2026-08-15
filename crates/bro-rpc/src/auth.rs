//! Same-host authentication for the daemon<->fleetd UDS channel.
//!
//! Two independent, complementary checks:
//!
//! - [`ServiceToken`]: a 64-lowercase-hex-char shared secret, held in a
//!   private (owner-only, non-symlink, single-hardlink) file. Whichever
//!   differentiated service starts first creates it; the other loads it.
//!   Comparison is constant-time so a byte-by-byte side channel cannot leak
//!   the value.
//! - [`verify_peer_uid`]: a `SO_PEERCRED` (Linux) / `LOCAL_PEERCRED`+
//!   `getpeereid` (macOS) check on the connected `UnixStream`, confirming the
//!   peer process runs as this process's own effective uid. Filesystem
//!   permissions on the socket path are a coarser version of the same
//!   guarantee; this is the wire-level belt to that belt-and-suspenders.

use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

const TOKEN_HEX_BYTES: usize = 64;
const TOKEN_RANDOM_BYTES: usize = TOKEN_HEX_BYTES / 2;

#[derive(Clone)]
pub struct ServiceToken {
    secret: String,
}

impl std::fmt::Debug for ServiceToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ServiceToken")
            .field(&"REDACTED")
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
    #[error("service token must be exactly 64 lowercase hexadecimal characters")]
    InvalidToken,
    #[error("UDS peer uid {peer_uid} does not match this process's effective uid {expected_uid}")]
    UnsafePeer { peer_uid: u32, expected_uid: u32 },
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
        Ok(Self { secret })
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
        let secret = random_hex_token()?;
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

    /// Constant-time comparison against a candidate secret received over the
    /// wire (e.g. the handshake `Hello.bearer_token` field).
    pub fn verify(&self, candidate: &str) -> bool {
        constant_time_eq(self.secret.as_bytes(), candidate.as_bytes())
    }

    /// The raw secret, for building an outgoing handshake payload. Never log
    /// or `Display` this; `Debug` on `ServiceToken` itself stays redacted.
    pub fn expose_secret(&self) -> &str {
        &self.secret
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

/// Rejects symlinks, non-regular files, files owned by another uid, files
/// with any group/other bit set, and hardlinked files (an attacker could
/// otherwise plant a hardlink pointing at a file they control and swap its
/// content underneath an already-open descriptor).
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

/// 32 bytes of OS randomness, hex-encoded to exactly 64 lowercase characters.
/// Reads `/dev/urandom` directly rather than pulling in a random-number
/// crate: both macOS and Linux (fleetd's two targets) expose it, and a
/// same-host bearer token has no need for anything fancier.
#[allow(clippy::disallowed_methods)]
fn random_hex_token() -> Result<String, ServiceTokenError> {
    let mut bytes = [0_u8; TOKEN_RANDOM_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
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

/// An ordered, overlap-tolerant set of accepted bearer tokens for one
/// producer. Index 0 is conventionally the oldest still-accepted token;
/// staging a rotation appends a new slot at the end, and retiring the old
/// token removes index 0 once every publisher has moved off it. Every slot
/// is compared in constant time and the comparison loop never
/// short-circuits on a match, so the number of comparisons performed does
/// not depend on whether, or where, a candidate matches -- the same
/// per-slot guarantee [`ServiceToken::verify`] already gives a single
/// token, extended across the whole staged list.
#[derive(Clone)]
pub struct ServiceTokenSet {
    tokens: Vec<ServiceToken>,
}

impl std::fmt::Debug for ServiceTokenSet {
    /// Only the slot COUNT is ever rendered; no token value is reachable
    /// through this type's `Debug`, matching the `ServiceToken` precedent
    /// this type wraps.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceTokenSet")
            .field("slots", &self.tokens.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceTokenSetError {
    #[error("a producer token list must name at least one token")]
    Empty,
    #[error(transparent)]
    Token(#[from] ServiceTokenError),
}

impl ServiceTokenSet {
    /// Load one [`ServiceToken`] per path, in order. Fails closed on the
    /// first unsafe or malformed file, exactly like a single
    /// [`ServiceToken::load`], and on an empty path list.
    pub fn load(paths: &[PathBuf]) -> Result<Self, ServiceTokenSetError> {
        if paths.is_empty() {
            return Err(ServiceTokenSetError::Empty);
        }
        let tokens = paths
            .iter()
            .map(|path| ServiceToken::load(path))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { tokens })
    }

    /// Build a set from already-parsed tokens (in-memory secrets, no file
    /// I/O). Public rather than `#[cfg(test)]` because callers outside this
    /// crate build `ServiceTokenSet`s from [`ServiceToken::parse`] in their
    /// own tests, the same way [`ServiceToken::parse`] itself is already
    /// public for that purpose.
    pub fn from_tokens(tokens: Vec<ServiceToken>) -> Result<Self, ServiceTokenSetError> {
        if tokens.is_empty() {
            return Err(ServiceTokenSetError::Empty);
        }
        Ok(Self { tokens })
    }

    /// Verify a candidate against every staged slot without short-
    /// circuiting, returning the index of the slot that matched. Every slot
    /// is checked even after a match is found, so the number of
    /// constant-time comparisons performed never varies with which slot (if
    /// any) matched.
    pub fn verify(&self, candidate: &str) -> Option<usize> {
        let mut matched = None;
        for (index, token) in self.tokens.iter().enumerate() {
            if token.verify(candidate) {
                matched = Some(index);
            }
        }
        matched
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Every staged token, for cross-producer digest/uniqueness checks at
    /// grant-table build time. Never log, persist, or `Debug`-print these.
    pub fn tokens(&self) -> &[ServiceToken] {
        &self.tokens
    }
}

/// Confirm a connected [`tokio::net::UnixStream`] peer is running as this
/// process's own effective uid, via `SO_PEERCRED` on Linux and
/// `LOCAL_PEERCRED`/`getpeereid` on macOS. `tokio::net::UnixStream::peer_cred`
/// already implements exactly that platform split internally, so this is a
/// thin, auditable wrapper rather than a hand-rolled duplicate of it: the
/// portability requirement is satisfied by delegating to tokio's tested
/// per-platform implementation instead of re-deriving unsafe FFI for both
/// targets here.
#[cfg(unix)]
pub fn verify_peer_uid(stream: &tokio::net::UnixStream) -> Result<u32, ServiceTokenError> {
    let credentials = stream.peer_cred()?;
    let peer_uid = credentials.uid();
    let expected_uid = effective_uid();
    if peer_uid != expected_uid {
        return Err(ServiceTokenError::UnsafePeer {
            peer_uid,
            expected_uid,
        });
    }
    Ok(peer_uid)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;

    #[test]
    fn token_is_private_redacted_and_verified_constant_time() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("auth/service.token");
        let token = ServiceToken::load_or_create(&path).unwrap();
        let secret = std::fs::read_to_string(&path).unwrap();
        let secret = secret.trim();
        assert_eq!(secret.len(), TOKEN_HEX_BYTES);
        assert!(!format!("{token:?}").contains(secret));
        assert!(format!("{token:?}").contains("REDACTED"));
        assert!(token.verify(secret));
        assert!(!token.verify("wrong"));
        assert!(!token.verify(&"a".repeat(TOKEN_HEX_BYTES)));

        let loaded = ServiceToken::load(&path).unwrap();
        assert!(loaded.verify(token.expose_secret()));
    }

    #[test]
    fn load_or_create_is_idempotent_across_first_and_second_service() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("auth/service.token");
        let first = ServiceToken::load_or_create(&path).unwrap();
        let second = ServiceToken::load_or_create(&path).unwrap();
        assert!(first.verify(second.expose_secret()));
    }

    #[test]
    fn parse_rejects_wrong_length_and_uppercase() {
        assert!(matches!(
            ServiceToken::parse("a".repeat(63)),
            Err(ServiceTokenError::InvalidToken)
        ));
        assert!(matches!(
            ServiceToken::parse("A".repeat(64)),
            Err(ServiceTokenError::InvalidToken)
        ));
        assert!(matches!(
            ServiceToken::parse("g".repeat(64)),
            Err(ServiceTokenError::InvalidToken)
        ));
        assert!(ServiceToken::parse("a".repeat(64)).is_ok());
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

    #[test]
    fn token_set_is_redacted_and_verifies_every_staged_slot_constant_time() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path_a = root.join("auth/producer.token");
        let path_b = root.join("auth/producer.token.next");
        let token_a = ServiceToken::load_or_create(&path_a).unwrap();
        let token_b = ServiceToken::load_or_create(&path_b).unwrap();
        let secret_a = token_a.expose_secret().to_string();
        let secret_b = token_b.expose_secret().to_string();

        let set = ServiceTokenSet::load(&[path_a.clone(), path_b.clone()]).unwrap();
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());

        // Debug never renders either secret, only the slot count.
        let rendered = format!("{set:?}");
        assert!(!rendered.contains(&secret_a));
        assert!(!rendered.contains(&secret_b));
        assert!(rendered.contains("slots"));
        assert!(rendered.contains('2'));

        // Every staged slot is independently accepted, by index.
        assert_eq!(set.verify(&secret_a), Some(0));
        assert_eq!(set.verify(&secret_b), Some(1));
        assert_eq!(set.verify("wrong"), None);
    }

    #[test]
    fn token_set_refuses_an_empty_list() {
        assert!(matches!(
            ServiceTokenSet::load(&[]),
            Err(ServiceTokenSetError::Empty)
        ));
        assert!(matches!(
            ServiceTokenSet::from_tokens(Vec::new()),
            Err(ServiceTokenSetError::Empty)
        ));
    }

    #[test]
    fn token_set_from_tokens_matches_the_same_slot_by_index() {
        let a = ServiceToken::parse("a".repeat(TOKEN_HEX_BYTES)).unwrap();
        let b = ServiceToken::parse("b".repeat(TOKEN_HEX_BYTES)).unwrap();
        let set = ServiceTokenSet::from_tokens(vec![a, b]).unwrap();
        assert_eq!(set.verify(&"a".repeat(TOKEN_HEX_BYTES)), Some(0));
        assert_eq!(set.verify(&"b".repeat(TOKEN_HEX_BYTES)), Some(1));
        assert_eq!(set.verify(&"c".repeat(TOKEN_HEX_BYTES)), None);
    }

    #[tokio::test]
    async fn verify_peer_uid_accepts_same_process_pair() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let socket_path = root.join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let client = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let server = accept.await.unwrap();

        let expected = effective_uid();
        assert_eq!(verify_peer_uid(&client).unwrap(), expected);
        assert_eq!(verify_peer_uid(&server).unwrap(), expected);
    }
}
