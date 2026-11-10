//! Storage classification and ingest admission. Exhaustion is a typed
//! refusal distinct from transient I/O, and ingest reserves its projected
//! allocation against the filesystem's free bytes before writing.
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Fixed estimate for the metadata a write allocates besides its payload: a
/// new stream directory, its `chunks/` directory, or a temporary file.
pub const METADATA_ALLOWANCE: u64 = 64 * 1024;
/// Lower bound of the default free-space reserve.
pub const DEFAULT_RESERVE_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Share of the filesystem the default reserve keeps, in percent.
pub const DEFAULT_RESERVE_PERCENT: u64 = 3;

/// The store refused or failed a write because the filesystem is full, a
/// quota is exhausted, or admission would consume the configured reserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageExhausted {
    detail: String,
}

impl StorageExhausted {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for StorageExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "transcript storage exhausted: {}", self.detail)
    }
}

impl std::error::Error for StorageExhausted {}

fn exhausted_kind(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded
    )
}

/// True when any cause in the chain is a storage exhaustion, including an
/// I/O error carried inside a tempfile persist error.
pub fn is_storage_exhausted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<StorageExhausted>()
            || cause
                .downcast_ref::<io::Error>()
                .is_some_and(exhausted_kind)
            || cause
                .downcast_ref::<tempfile::PersistError>()
                .is_some_and(|persist| exhausted_kind(&persist.error))
            || cause
                .downcast_ref::<tempfile::PathPersistError>()
                .is_some_and(|persist| exhausted_kind(&persist.error))
    })
}

/// Replace an error whose chain reports storage exhaustion with the typed
/// refusal, so callers classify it by type instead of by message.
pub fn normalize_storage_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<StorageExhausted>().is_some() || !is_storage_exhausted(&error) {
        return error;
    }
    anyhow::Error::new(StorageExhausted::new(format!("{error:#}")))
}

/// Size and free bytes of one filesystem, as seen by an unprivileged writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsSpace {
    pub total: u64,
    pub available: u64,
}

pub type SpaceProbe = Arc<dyn Fn(&Path) -> io::Result<FsSpace> + Send + Sync>;

pub fn probe_filesystem(path: &Path) -> io::Result<FsSpace> {
    let stats = fs2::statvfs(path)?;
    Ok(FsSpace {
        total: stats.total_space(),
        available: stats.available_space(),
    })
}

/// The deepest existing ancestor of `path` (itself when it exists).
pub fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut cursor = path;
    loop {
        if cursor.exists() {
            return cursor.to_path_buf();
        }
        match cursor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
            _ => return PathBuf::from("."),
        }
    }
}

/// How many free bytes ingest keeps on the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservePolicy {
    /// The larger of 2 GiB and 3% of the filesystem.
    Default,
    Fixed(u64),
}

impl ReservePolicy {
    pub fn from_config(min_free_bytes: Option<u64>) -> Self {
        min_free_bytes.map_or(Self::Default, Self::Fixed)
    }
    pub fn reserve_for(self, total: u64) -> u64 {
        match self {
            Self::Fixed(bytes) => bytes,
            Self::Default => DEFAULT_RESERVE_FLOOR_BYTES.max(total / 100 * DEFAULT_RESERVE_PERCENT),
        }
    }
}

/// One reservation ledger per store root. Check-and-reserve is a single step
/// under the ledger mutex; a [`Reservation`] releases its bytes on drop, so
/// every exit path of the reserving operation gives them back.
pub struct StorageAdmission {
    root: PathBuf,
    policy: ReservePolicy,
    probe: SpaceProbe,
    outstanding: Mutex<u64>,
}

impl StorageAdmission {
    pub fn new(root: impl Into<PathBuf>, policy: ReservePolicy) -> Self {
        Self::with_probe(root, policy, Arc::new(probe_filesystem))
    }
    pub fn with_probe(root: impl Into<PathBuf>, policy: ReservePolicy, probe: SpaceProbe) -> Self {
        Self {
            root: root.into(),
            policy,
            probe,
            outstanding: Mutex::new(0),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn policy(&self) -> ReservePolicy {
        self.policy
    }
    /// Bytes reserved by operations still in flight.
    pub fn outstanding(&self) -> u64 {
        *self
            .outstanding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Reserve `bytes` when the filesystem keeps at least the reserve after
    /// subtracting every outstanding reservation and this one.
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> anyhow::Result<Reservation> {
        let mut outstanding = self
            .outstanding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let space = (self.probe)(&nearest_existing_ancestor(&self.root))
            .map_err(|error| normalize_storage_error(error.into()))?;
        let reserve = self.policy.reserve_for(space.total);
        let remaining = space
            .available
            .saturating_sub(*outstanding)
            .checked_sub(bytes);
        if remaining.is_none_or(|remaining| remaining < reserve) {
            return Err(anyhow::Error::new(StorageExhausted::new(format!(
                "admission refused {bytes} projected bytes: {} available, {} reserved by \
                 in-flight ingest, reserve {reserve}",
                space.available, *outstanding
            ))));
        }
        *outstanding += bytes;
        Ok(Reservation {
            ledger: self.clone(),
            bytes,
        })
    }
}

#[must_use = "a reservation is released when dropped"]
pub struct Reservation {
    ledger: Arc<StorageAdmission>,
    bytes: u64,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reservation")
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Reservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut outstanding = self
            .ledger
            .outstanding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *outstanding = outstanding.saturating_sub(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_wrapped_quota_and_full_errors_normalize_to_the_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        for kind in [ErrorKind::StorageFull, ErrorKind::QuotaExceeded] {
            let persist = tempfile::PersistError {
                error: io::Error::from(kind),
                file: tempfile::NamedTempFile::new_in(dir.path()).unwrap(),
            };
            let error = anyhow::Error::from(persist).context("persisting a chunk");
            let normalized = normalize_storage_error(error);
            assert!(normalized.downcast_ref::<StorageExhausted>().is_some());
            let io_error = anyhow::Error::from(io::Error::from(kind)).context("writing");
            assert!(
                normalize_storage_error(io_error)
                    .downcast_ref::<StorageExhausted>()
                    .is_some()
            );
        }
        let transient = normalize_storage_error(anyhow::Error::from(io::Error::from(
            ErrorKind::PermissionDenied,
        )));
        assert!(transient.downcast_ref::<StorageExhausted>().is_none());
        assert!(transient.downcast_ref::<io::Error>().is_some());
    }

    #[test]
    fn default_reserve_is_the_larger_of_the_floor_and_three_percent() {
        assert_eq!(
            ReservePolicy::Default.reserve_for(10 * 1024 * 1024 * 1024),
            DEFAULT_RESERVE_FLOOR_BYTES
        );
        assert_eq!(
            ReservePolicy::Default.reserve_for(1_000_000_000_000),
            30_000_000_000
        );
        assert_eq!(ReservePolicy::Fixed(7).reserve_for(1_000_000), 7);
    }

    #[test]
    fn reservations_count_against_admission_until_dropped() {
        let ledger = Arc::new(StorageAdmission::with_probe(
            "/",
            ReservePolicy::Fixed(100),
            Arc::new(|_: &Path| {
                Ok(FsSpace {
                    total: 1_000,
                    available: 400,
                })
            }),
        ));
        let first = ledger.reserve(200).unwrap();
        assert_eq!(ledger.outstanding(), 200);
        let refused = ledger.reserve(101).unwrap_err();
        assert!(refused.downcast_ref::<StorageExhausted>().is_some());
        assert_eq!(ledger.outstanding(), 200);
        let second = ledger.reserve(100).unwrap();
        drop(first);
        assert_eq!(ledger.outstanding(), 100);
        drop(second);
        assert_eq!(ledger.outstanding(), 0);
        assert!(ledger.reserve(300).is_ok());
        assert!(ledger.reserve(301).is_err());
    }
}
