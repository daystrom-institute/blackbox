//! Single-cockpit instance guard (gap-ddfde5c3).
//!
//! Two `bro fleet` cockpits over one fleet store dir duel: both subscribe to
//! `/control/roster/stream` and issue overlapping `/control/exec` +
//! `/control/steer`, corrupting each other's display state. An advisory
//! `flock` on `<store_dir>/cockpit.lock` (via `fs2`, same primitive as
//! `composer_history`) makes the second cockpit refuse to start, naming the
//! holder's PID. The kernel drops the lock when the holding process dies, so
//! a stale lockfile from a crashed cockpit is taken over without any cleanup
//! pass — the PID in the file is advisory display data, never a liveness
//! source.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use fs2::FileExt;

/// Held for the whole cockpit lifetime; dropping it (closing the fd) releases
/// the flock. The lockfile itself is left in place — unlinking it would race
/// a concurrent acquirer still holding the old inode open.
#[derive(Debug)]
pub(super) struct CockpitInstanceLock {
    _file: File,
}

impl CockpitInstanceLock {
    // Acquired synchronously during cockpit startup, before the TUI event loop begins.
    #[allow(clippy::disallowed_methods)]
    pub(super) fn acquire(store_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(store_dir)?;
        let path = store_dir.join("cockpit.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if file.try_lock_exclusive().is_err() {
            let holder = read_holder_pid(&mut file)
                .map(|pid| format!("pid {pid}"))
                .unwrap_or_else(|| "unknown pid".to_string());
            anyhow::bail!(
                "another bro fleet cockpit ({holder}) already holds {} — two cockpits over one \
                 fleet store fight over the roster stream; close the other cockpit first, or \
                 re-run with --force to bypass the guard",
                path.display()
            );
        }
        // Lock held: stamp our PID for the next contender's error message.
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(Self { _file: file })
    }
}

fn read_holder_pid(file: &mut File) -> Option<u32> {
    let mut buf = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise the process-level lock contract.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn acquire_stamps_pid_and_blocks_second_instance() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().canonicalize().unwrap().join("fleet");

        let _lock = CockpitInstanceLock::acquire(&store).expect("first acquire");
        let stamped = std::fs::read_to_string(store.join("cockpit.lock")).unwrap();
        assert_eq!(stamped.trim(), std::process::id().to_string());

        // flock is per open-file-description, so a second open in the same
        // process conflicts just like a second cockpit process would.
        let err = CockpitInstanceLock::acquire(&store).expect_err("second acquire must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("pid {}", std::process::id())),
            "error must name the holding pid: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "error must mention the override: {msg}"
        );
    }

    #[test]
    fn drop_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().canonicalize().unwrap().join("fleet");

        let lock = CockpitInstanceLock::acquire(&store).expect("first acquire");
        drop(lock);
        // Clean exit (Drop) released the flock; the next cockpit starts fine.
        CockpitInstanceLock::acquire(&store).expect("re-acquire after drop");
    }

    #[test]
    fn stale_lockfile_from_dead_process_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().canonicalize().unwrap().join("fleet");
        std::fs::create_dir_all(&store).unwrap();
        // A leftover lockfile with no live flock holder (crashed cockpit):
        // the kernel released the lock with the process, so acquire wins.
        std::fs::write(store.join("cockpit.lock"), "999999\n").unwrap();

        let _lock = CockpitInstanceLock::acquire(&store).expect("stale lock takeover");
        let stamped = std::fs::read_to_string(store.join("cockpit.lock")).unwrap();
        assert_eq!(stamped.trim(), std::process::id().to_string());
    }
}
