use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NONCE: AtomicU64 = AtomicU64::new(0);

/// An exclusive guard for the canonical `<store>.json.lock` inode.
#[derive(Debug)]
pub struct StoreLockGuard {
    file: File,
}

impl Drop for StoreLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// A held directory descriptor opened component-by-component without following
/// symlinks.
///
/// Relative reads and atomic replacements stay bound to this exact directory
/// inode even if an ancestor is concurrently renamed. Callers that publish a
/// path-based result should call [`Self::ensure_still_current`] before
/// returning.
#[derive(Debug)]
pub struct NofollowDirectory {
    file: File,
    path: PathBuf,
}

impl NofollowDirectory {
    pub fn open_existing(path: &Path) -> Result<Option<Self>> {
        Self::open_inner(path, false)
    }

    pub fn open_or_create(path: &Path) -> Result<Self> {
        Self::open_inner(path, true)?.ok_or_else(|| {
            anyhow::anyhow!(
                "directory disappeared while opening without following links: {}",
                path.display()
            )
        })
    }

    fn open_inner(path: &Path, create_missing: bool) -> Result<Option<Self>> {
        #[cfg(unix)]
        {
            Self::open_inner_unix(path, create_missing)
        }
        #[cfg(not(unix))]
        {
            if create_missing {
                fs::create_dir_all(path)
                    .with_context(|| format!("failed to create directory {}", path.display()))?;
            }
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect directory {}", path.display())
                    });
                }
            };
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                anyhow::bail!("refusing non-directory or symlink {}", path.display());
            }
            let file = File::open(path)
                .with_context(|| format!("failed to open directory {}", path.display()))?;
            Ok(Some(Self {
                file,
                path: path.to_path_buf(),
            }))
        }
    }

    #[cfg(unix)]
    fn open_inner_unix(path: &Path, create_missing: bool) -> Result<Option<Self>> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        use std::path::Component;

        #[cfg(target_os = "macos")]
        let opened_path = normalize_macos_system_alias(path);
        #[cfg(not(target_os = "macos"))]
        let opened_path = path.to_path_buf();
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(if opened_path.is_absolute() { "/" } else { "." })
            .with_context(|| {
                format!(
                    "failed to open directory root without following links for {}",
                    path.display()
                )
            })?;
        for component in opened_path.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::Prefix(_) | Component::ParentDir => {
                    anyhow::bail!(
                        "directory path contains an unsafe component: {}",
                        path.display()
                    );
                }
            };
            let name =
                CString::new(name.as_bytes()).context("directory component contains a NUL byte")?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let mut descriptor =
                unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if descriptor < 0 {
                let open_error = std::io::Error::last_os_error();
                if open_error.raw_os_error() != Some(libc::ENOENT) {
                    return Err(open_error).with_context(|| {
                        format!(
                            "failed to open directory component without following links for {}",
                            path.display()
                        )
                    });
                }
                if !create_missing {
                    return Ok(None);
                }
                let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if created < 0 {
                    let create_error = std::io::Error::last_os_error();
                    if create_error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(create_error).with_context(|| {
                            format!(
                                "failed to create directory component for {}",
                                path.display()
                            )
                        });
                    }
                } else {
                    directory.sync_all().with_context(|| {
                        format!("failed to fsync directory parent for {}", path.display())
                    })?;
                }
                descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
                if descriptor < 0 {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!(
                            "failed to reopen created directory without following links for {}",
                            path.display()
                        )
                    });
                }
            }
            directory = unsafe { File::from_raw_fd(descriptor) };
        }
        Ok(Some(Self {
            file: directory,
            path: opened_path,
        }))
    }

    pub fn lock_exclusive(&self) -> Result<()> {
        self.file
            .lock_exclusive()
            .with_context(|| format!("failed to lock directory {}", self.path.display()))
    }

    pub fn read_regular(
        &self,
        name: &str,
        max_bytes: usize,
        label: &str,
    ) -> Result<Option<Vec<u8>>> {
        validate_relative_basename(name)?;
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};

            let name = CString::new(name).context("relative filename contains a NUL byte")?;
            let descriptor = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOENT) {
                    return Ok(None);
                }
                return Err(error)
                    .with_context(|| format!("failed to open {label} in {}", self.path.display()));
            }
            let mut file = unsafe { File::from_raw_fd(descriptor) };
            if !file
                .metadata()
                .with_context(|| format!("failed to inspect {label}"))?
                .file_type()
                .is_file()
            {
                anyhow::bail!("refusing non-regular {label}");
            }
            return read_bounded(&mut file, max_bytes, label).map(Some);
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            let mut file = match OpenOptions::new().read(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to open {label} {}", path.display()));
                }
            };
            if !file.metadata()?.file_type().is_file() {
                anyhow::bail!("refusing non-regular {label}");
            }
            read_bounded(&mut file, max_bytes, label).map(Some)
        }
    }

    pub fn atomic_replace(&self, name: &str, bytes: &[u8]) -> Result<()> {
        validate_relative_basename(name)?;
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};

            let destination =
                CString::new(name).context("relative filename contains a NUL byte")?;
            let pid = std::process::id();
            for _ in 0..128 {
                let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
                let temp_name = CString::new(format!(".bbox-store-{pid}-{nonce}.tmp"))
                    .expect("code-owned temporary filename has no NUL byte");
                let descriptor = unsafe {
                    libc::openat(
                        self.file.as_raw_fd(),
                        temp_name.as_ptr(),
                        libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_RDWR
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        0o600,
                    )
                };
                if descriptor < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EEXIST) {
                        continue;
                    }
                    return Err(error).with_context(|| {
                        format!("failed to create temporary file in {}", self.path.display())
                    });
                }
                let mut file = unsafe { File::from_raw_fd(descriptor) };
                let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
                drop(file);
                if let Err(error) = write_result {
                    unsafe {
                        libc::unlinkat(self.file.as_raw_fd(), temp_name.as_ptr(), 0);
                    }
                    return Err(error).context("failed to write and fsync temporary file");
                }
                let renamed = unsafe {
                    libc::renameat(
                        self.file.as_raw_fd(),
                        temp_name.as_ptr(),
                        self.file.as_raw_fd(),
                        destination.as_ptr(),
                    )
                };
                if renamed < 0 {
                    let error = std::io::Error::last_os_error();
                    unsafe {
                        libc::unlinkat(self.file.as_raw_fd(), temp_name.as_ptr(), 0);
                    }
                    return Err(error).with_context(|| {
                        format!("failed to atomically replace {label}", label = name)
                    });
                }
                return self
                    .file
                    .sync_all()
                    .with_context(|| format!("failed to fsync directory {}", self.path.display()));
            }
            anyhow::bail!("temporary filename retries exhausted");
        }
        #[cfg(not(unix))]
        {
            atomic_write_bytes_from_dir_locked(&self.path.join(name), &self.path, bytes)
        }
    }

    pub fn sync_all(&self) -> Result<()> {
        self.file
            .sync_all()
            .with_context(|| format!("failed to fsync directory {}", self.path.display()))
    }

    pub fn ensure_still_current(&self) -> Result<()> {
        let Some(current) = Self::open_existing(&self.path)? else {
            anyhow::bail!("directory disappeared: {}", self.path.display());
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let held = self.file.metadata()?;
            let reopened = current.file.metadata()?;
            if held.dev() != reopened.dev() || held.ino() != reopened.ino() {
                anyhow::bail!("directory was replaced: {}", self.path.display());
            }
        }
        #[cfg(not(unix))]
        {
            if self.file.metadata()?.file_type() != current.file.metadata()?.file_type() {
                anyhow::bail!("directory was replaced: {}", self.path.display());
            }
        }
        Ok(())
    }

    pub(crate) fn path_for_diagnostics(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    pub(crate) fn duplicate_descriptor(&self) -> Result<File> {
        self.file
            .try_clone()
            .context("duplicating held no-follow directory descriptor")
    }
}

fn validate_relative_basename(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
        || name
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        anyhow::bail!("invalid relative filename");
    }
    Ok(())
}

fn read_bounded(file: &mut File, max_bytes: usize, label: &str) -> Result<Vec<u8>> {
    let limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("{label} byte limit overflow"))?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {label}"))?;
    if bytes.len() > max_bytes {
        anyhow::bail!("{label} exceeds byte limit");
    }
    Ok(bytes)
}

/// Acquire the canonical store lock without following a lock-file symlink.
///
/// The lock file is never truncated. Keeping its inode stable is load-bearing:
/// all compatible store writers must contend on the same advisory lock.
pub fn acquire_store_lock_nofollow(store_path: &Path) -> Result<StoreLockGuard> {
    let lock_path = canonical_store_lock_path(store_path);
    let file = open_lock_path_nofollow(&lock_path)?;
    if !file
        .metadata()
        .with_context(|| format!("failed to inspect lock file {}", lock_path.display()))?
        .file_type()
        .is_file()
    {
        anyhow::bail!("store lock is not a regular file: {}", lock_path.display());
    }

    file.lock_exclusive().with_context(|| {
        format!(
            "failed to acquire exclusive lock on {}",
            lock_path.display()
        )
    })?;
    Ok(StoreLockGuard { file })
}

pub fn canonical_store_lock_path(store_path: &Path) -> std::path::PathBuf {
    store_path.with_extension("json.lock")
}

/// Open or create a lock file through no-follow directory descriptors.
///
/// Unix callers get component-by-component `openat`/`mkdirat` handling, so a
/// symlink in any parent component is rejected. Other platforms retain the
/// final-component no-follow check where the platform exposes it.
pub fn open_lock_path_nofollow(lock_path: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        open_lock_path_nofollow_unix(lock_path)
    }
    #[cfg(not(unix))]
    {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        if !file.metadata()?.file_type().is_file() {
            anyhow::bail!("store lock is not a regular file: {}", lock_path.display());
        }
        Ok(file)
    }
}

#[cfg(unix)]
fn open_lock_path_nofollow_unix(lock_path: &Path) -> Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    #[cfg(target_os = "macos")]
    let normalized_lock_path = normalize_macos_system_alias(lock_path);
    #[cfg(not(target_os = "macos"))]
    let normalized_lock_path = lock_path.to_path_buf();
    let parent = normalized_lock_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("lock path has no parent: {}", lock_path.display()))?;
    let filename = normalized_lock_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("lock path has no filename: {}", lock_path.display()))?;
    let mut directory = if parent.is_absolute() {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?
    } else {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(".")?
    };
    for component in parent.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::Prefix(_) | Component::ParentDir => {
                anyhow::bail!(
                    "lock path contains an unsafe parent component: {}",
                    lock_path.display()
                );
            }
        };
        let name = CString::new(name.as_bytes())
            .context("lock directory component contains a NUL byte")?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if created < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error).context("failed to create lock directory component");
                }
            }
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to open lock directory component without following links");
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }

    let filename =
        CString::new(filename.as_bytes()).context("lock filename contains a NUL byte")?;
    let fd = loop {
        let existing = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                filename.as_ptr(),
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if existing >= 0 {
            break existing;
        }
        let open_error = std::io::Error::last_os_error();
        if open_error.raw_os_error() != Some(libc::ENOENT) {
            return Err(open_error)
                .with_context(|| format!("failed to open lock file {}", lock_path.display()));
        }

        let created = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                filename.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if created >= 0 {
            break created;
        }
        let create_error = std::io::Error::last_os_error();
        if create_error.raw_os_error() != Some(libc::EEXIST) {
            return Err(create_error)
                .with_context(|| format!("failed to create lock file {}", lock_path.display()));
        }
    };
    let file = unsafe { File::from_raw_fd(fd) };
    if !file
        .metadata()
        .with_context(|| format!("failed to inspect lock file {}", lock_path.display()))?
        .file_type()
        .is_file()
    {
        anyhow::bail!("store lock is not a regular file: {}", lock_path.display());
    }
    Ok(file)
}

#[cfg(target_os = "macos")]
fn normalize_macos_system_alias(path: &Path) -> std::path::PathBuf {
    for (alias, canonical) in [
        (Path::new("/var"), Path::new("/private/var")),
        (Path::new("/tmp"), Path::new("/private/tmp")),
        (Path::new("/etc"), Path::new("/private/etc")),
    ] {
        if let Ok(suffix) = path.strip_prefix(alias) {
            return canonical.join(suffix);
        }
    }
    path.to_path_buf()
}

/// Acquire an exclusive lock on `<store_path>.json.lock` and execute `f`.
/// The lock is released after `f` returns.
pub fn with_store_lock<T>(store_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = acquire_store_lock_nofollow(store_path)?;
    f()
}

/// Atomically write `value` to `store_path` using a unique temporary file.
/// This function does NOT acquire the lock; callers should wrap it in `with_store_lock`.
pub fn atomic_write_json_locked<T: serde::Serialize>(store_path: &Path, value: &T) -> Result<()> {
    let bytes = to_vec_pretty_newline(value)?;
    atomic_write_bytes_locked(store_path, &bytes)
}

/// Atomically replace `store_path` with caller-provided bytes.
///
/// This shares the JSON store's unique temporary-file and fsync discipline but
/// performs no serialization. Callers must hold [`with_store_lock`] when the
/// write participates in a read-modify-write transaction.
pub fn atomic_write_bytes_locked(store_path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_dir = store_path.parent().unwrap_or_else(|| Path::new("."));
    atomic_write_bytes_from_dir_locked(store_path, temp_dir, bytes)
}

/// Atomically replace `store_path` using a temporary file in `temp_dir`.
/// `temp_dir` must be on the same filesystem as the destination. This lets a
/// caller keep crash debris in ignored operational state while preserving an
/// atomic cross-directory rename into a committed path.
pub fn atomic_write_bytes_from_dir_locked(
    store_path: &Path,
    temp_dir: &Path,
    bytes: &[u8],
) -> Result<()> {
    let pid = std::process::id();
    fs::create_dir_all(temp_dir)?;
    let (tmp_path, mut file) =
        (0..128)
            .find_map(|_| {
                let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
                let tmp_path = temp_dir.join(format!(".bbox-store-{pid}-{nonce}.tmp"));
                let mut options = OpenOptions::new();
                options.create_new(true).read(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
                }
                match options.open(&tmp_path) {
                    Ok(file) => Some(Ok((tmp_path, file))),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error).with_context(|| {
                        format!("failed to create temp file {}", tmp_path.display())
                    })),
                }
            })
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("temporary JSON store filename retries exhausted"))?;
    use std::io::Write;
    if let Err(error) = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write and fsync temp file {}", tmp_path.display()))
    {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    drop(file);

    if let Err(error) = fs::rename(&tmp_path, store_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            store_path.display()
        )
    }) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    let store_parent = store_path.parent().unwrap_or_else(|| Path::new("."));
    File::open(store_parent)
        .with_context(|| format!("failed to open store parent {}", store_parent.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync store parent {}", store_parent.display()))?;
    if temp_dir != store_parent {
        File::open(temp_dir)
            .with_context(|| format!("failed to open temp directory {}", temp_dir.display()))?
            .sync_all()
            .with_context(|| format!("failed to fsync temp directory {}", temp_dir.display()))?;
    }

    Ok(())
}

/// Pretty JSON bytes with the repo convention of exactly one trailing newline.
pub fn to_vec_pretty_newline<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn json_store_unique_tmp_names_do_not_collide() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("store.json");

        let n1 = NONCE.load(Ordering::SeqCst);
        atomic_write_json_locked(&store_path, &serde_json::json!({})).unwrap();
        let n2 = NONCE.load(Ordering::SeqCst);
        assert!(n2 > n1);

        atomic_write_json_locked(&store_path, &serde_json::json!({})).unwrap();
        let n3 = NONCE.load(Ordering::SeqCst);
        assert!(n3 > n2);
    }

    #[test]
    fn json_store_writes_trailing_newline() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("store.json");

        atomic_write_json_locked(&store_path, &serde_json::json!({"a": 1})).unwrap();

        let text = fs::read_to_string(&store_path).unwrap();
        assert!(text.ends_with('\n'));
        assert!(!text.ends_with("\n\n"));
    }

    #[test]
    fn byte_store_preserves_exact_content() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("config.toml");
        atomic_write_bytes_locked(&store_path, b"# keep\n[project]\nname = \"x\"\n").unwrap();
        assert_eq!(
            fs::read(&store_path).unwrap(),
            b"# keep\n[project]\nname = \"x\"\n"
        );
    }

    #[test]
    fn json_store_lock_serializes_concurrent_writes() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_path = root.join("store.json");

        // Initialize with empty array
        fs::write(&store_path, "[]").unwrap();

        let path_clone = store_path.clone();
        let t1 = thread::spawn(move || {
            for i in 0..10 {
                with_store_lock(&path_clone, || {
                    let text = fs::read_to_string(&path_clone)?;
                    let mut vec: Vec<i32> = serde_json::from_str(&text)?;
                    vec.push(i);
                    atomic_write_json_locked(&path_clone, &vec)?;
                    Ok(())
                })
                .unwrap();
            }
        });

        let path_clone2 = store_path.clone();
        let t2 = thread::spawn(move || {
            for i in 10..20 {
                with_store_lock(&path_clone2, || {
                    let text = fs::read_to_string(&path_clone2)?;
                    let mut vec: Vec<i32> = serde_json::from_str(&text)?;
                    vec.push(i);
                    atomic_write_json_locked(&path_clone2, &vec)?;
                    Ok(())
                })
                .unwrap();
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let text = fs::read_to_string(&store_path).unwrap();
        let vec: Vec<i32> = serde_json::from_str(&text).unwrap();
        assert_eq!(vec.len(), 20);
        for i in 0..20 {
            assert!(vec.contains(&i));
        }
    }

    #[cfg(unix)]
    #[test]
    fn json_store_lock_refuses_symlink_and_preserves_inode() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_path = root.join("store.json");
        let lock_path = store_path.with_extension("json.lock");

        let first = acquire_store_lock_nofollow(&store_path).unwrap();
        let inode = fs::metadata(&lock_path).unwrap().ino();
        drop(first);
        let second = acquire_store_lock_nofollow(&store_path).unwrap();
        assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
        drop(second);

        fs::remove_file(&lock_path).unwrap();
        let target = root.join("target");
        fs::write(&target, b"do not follow").unwrap();
        symlink(&target, &lock_path).unwrap();
        assert!(acquire_store_lock_nofollow(&store_path).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"do not follow");

        let real_parent = root.join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = root.join("linked-parent");
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(acquire_store_lock_nofollow(&linked_parent.join("store.json")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn held_directory_writes_cannot_be_redirected_by_path_replacement() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let lane_path = root.join("lane");
        let moved_path = root.join("moved-lane");
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        let lane = NofollowDirectory::open_or_create(&lane_path).unwrap();
        lane.lock_exclusive().unwrap();

        fs::rename(&lane_path, &moved_path).unwrap();
        symlink(&outside, &lane_path).unwrap();
        lane.atomic_replace("value", b"held inode").unwrap();

        assert_eq!(fs::read(moved_path.join("value")).unwrap(), b"held inode");
        assert!(!outside.join("value").exists());
        assert!(lane.ensure_still_current().is_err());
    }
}
