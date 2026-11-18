//! Child-process spawning for Git commands that must start inside a held
//! directory handle.
//!
//! Stable repository reads pin the child's working directory to a captured
//! objects-directory inode. The portable way is a `pre_exec` closure that
//! calls `fchdir`, but any `pre_exec` closure forces the standard library to
//! use `fork` + `exec` instead of `posix_spawn`. On macOS, `fork` in a
//! multi-gigabyte, many-threaded daemon is very expensive. The kernel copies
//! the whole VM map and the parent then takes copy-on-write faults, while
//! libmalloc's fork handlers lock every zone and stall each allocating
//! thread. A daemon that spawns these reads in a loop can spend nearly all
//! of its CPU in the kernel.
//!
//! macOS 10.15+ offers `posix_spawn_file_actions_addfchdir_np`, which
//! performs the same `fchdir` in the child without `fork`. On macOS this
//! module spawns anchored commands through `posix_spawnp` with that file
//! action. Every other platform, and every unanchored command, keeps the
//! standard-library spawn.

use std::io;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

/// Raw descriptor of a directory the child must start in.
#[cfg(unix)]
pub(crate) type DirectoryAnchor = std::os::fd::RawFd;
#[cfg(not(unix))]
pub(crate) type DirectoryAnchor = i32;

/// A spawned Git child with its pipes split out.
pub(crate) struct SpawnedGit {
    pub(crate) child: GitChild,
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

/// Child handle with the subset of `std::process::Child` that the Git
/// helpers use.
pub(crate) enum GitChild {
    Std(std::process::Child),
    #[cfg(target_os = "macos")]
    Spawned(macos::PosixSpawnedChild),
}

impl GitChild {
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            GitChild::Std(child) => child.try_wait(),
            #[cfg(target_os = "macos")]
            GitChild::Spawned(child) => child.try_wait(),
        }
    }

    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match self {
            GitChild::Std(child) => child.kill(),
            #[cfg(target_os = "macos")]
            GitChild::Spawned(child) => child.kill(),
        }
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        match self {
            GitChild::Std(child) => child.wait(),
            #[cfg(target_os = "macos")]
            GitChild::Spawned(child) => child.wait(),
        }
    }
}

/// Spawn `command` with stdout and stderr piped and stdin either piped or
/// null. With an `anchor`, the child starts in that directory handle;
/// without one, the command's own working directory applies.
///
/// The caller must keep `anchor` open until this returns.
pub(crate) fn spawn_git(
    mut command: Command,
    anchor: Option<DirectoryAnchor>,
    pipe_stdin: bool,
) -> io::Result<SpawnedGit> {
    #[cfg(target_os = "macos")]
    if let Some(anchor) = anchor {
        return macos::spawn_anchored(&command, anchor, pipe_stdin);
    }
    #[cfg(not(target_os = "macos"))]
    if anchor.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory-anchored spawn is only implemented on macOS; \
             other platforms anchor through pre_exec",
        ));
    }
    command
        .stdin(if pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok(SpawnedGit {
        child: GitChild::Std(child),
        stdin,
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::BTreeMap;
    use std::ffi::{CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};

    use super::{GitChild, SpawnedGit};

    unsafe extern "C" {
        // Available since macOS 10.15; not yet exported by the libc crate.
        fn posix_spawn_file_actions_addfchdir_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            fd: libc::c_int,
        ) -> libc::c_int;
    }

    /// A child started through `posix_spawnp`. Like `std::process::Child`,
    /// dropping it neither kills nor reaps the process.
    pub(crate) struct PosixSpawnedChild {
        pid: libc::pid_t,
        status: Option<ExitStatus>,
    }

    impl PosixSpawnedChild {
        pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            if let Some(status) = self.status {
                return Ok(Some(status));
            }
            self.wait_with_flags(libc::WNOHANG)
        }

        pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
            if let Some(status) = self.status {
                return Ok(status);
            }
            match self.wait_with_flags(0)? {
                Some(status) => Ok(status),
                None => Err(io::Error::other("blocking waitpid returned no status")),
            }
        }

        pub(crate) fn kill(&mut self) -> io::Result<()> {
            // Mirror std: a reaped child must not signal a recycled pid.
            if self.status.is_some() {
                return Ok(());
            }
            if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn wait_with_flags(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
            loop {
                let mut raw_status: libc::c_int = 0;
                let reaped = unsafe { libc::waitpid(self.pid, &mut raw_status, flags) };
                if reaped == self.pid {
                    let status = ExitStatus::from_raw(raw_status);
                    self.status = Some(status);
                    return Ok(Some(status));
                }
                if reaped == 0 {
                    return Ok(None);
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
        }
    }

    struct FileActions(libc::posix_spawn_file_actions_t);

    impl FileActions {
        fn new() -> io::Result<Self> {
            let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
            check(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
            Ok(Self(actions))
        }
    }

    impl Drop for FileActions {
        fn drop(&mut self) {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut self.0);
            }
        }
    }

    struct Attributes(libc::posix_spawnattr_t);

    impl Attributes {
        fn new() -> io::Result<Self> {
            let mut attributes: libc::posix_spawnattr_t = std::ptr::null_mut();
            check(unsafe { libc::posix_spawnattr_init(&mut attributes) })?;
            Ok(Self(attributes))
        }
    }

    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe {
                libc::posix_spawnattr_destroy(&mut self.0);
            }
        }
    }

    /// posix_spawn family functions return the error number directly.
    fn check(code: libc::c_int) -> io::Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    }

    fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        for fd in [fds[0], fds[1]] {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok((read, write))
    }

    fn c_string(value: &OsStr) -> io::Result<CString> {
        CString::new(value.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "spawn argument or environment entry contains a NUL byte",
            )
        })
    }

    /// The child environment: this process's environment with the
    /// command's explicit sets and removals applied. The Git helpers never
    /// call `env_clear`, which `Command` does not expose for inspection.
    fn child_environment(command: &Command) -> io::Result<Vec<CString>> {
        let mut environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => {
                    environment.insert(key.to_owned(), value.to_owned());
                }
                None => {
                    environment.remove(key);
                }
            }
        }
        environment
            .into_iter()
            .map(|(key, value)| {
                let mut entry = key;
                entry.push("=");
                entry.push(value);
                c_string(&entry)
            })
            .collect()
    }

    pub(super) fn spawn_anchored(
        command: &Command,
        anchor: RawFd,
        pipe_stdin: bool,
    ) -> io::Result<SpawnedGit> {
        let program = c_string(command.get_program())?;
        let mut argv_owned = vec![program.clone()];
        for argument in command.get_args() {
            argv_owned.push(c_string(argument)?);
        }
        let mut argv: Vec<*mut libc::c_char> = argv_owned
            .iter()
            .map(|argument| argument.as_ptr() as *mut libc::c_char)
            .collect();
        argv.push(std::ptr::null_mut());
        let envp_owned = child_environment(command)?;
        let mut envp: Vec<*mut libc::c_char> = envp_owned
            .iter()
            .map(|entry| entry.as_ptr() as *mut libc::c_char)
            .collect();
        envp.push(std::ptr::null_mut());

        let stdin_pipe = if pipe_stdin {
            Some(cloexec_pipe()?)
        } else {
            None
        };
        let (stdout_read, stdout_write) = cloexec_pipe()?;
        let (stderr_read, stderr_write) = cloexec_pipe()?;
        let dev_null = c"/dev/null";

        let mut actions = FileActions::new()?;
        unsafe {
            use std::os::fd::AsRawFd;
            match &stdin_pipe {
                Some((read, _)) => check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions.0,
                    read.as_raw_fd(),
                    0,
                ))?,
                None => check(libc::posix_spawn_file_actions_addopen(
                    &mut actions.0,
                    0,
                    dev_null.as_ptr(),
                    libc::O_RDONLY,
                    0,
                ))?,
            }
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions.0,
                stdout_write.as_raw_fd(),
                1,
            ))?;
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions.0,
                stderr_write.as_raw_fd(),
                2,
            ))?;
            // File actions run in order in the child, before exec closes the
            // close-on-exec descriptors, so the anchor is still open here.
            check(posix_spawn_file_actions_addfchdir_np(
                &mut actions.0,
                anchor,
            ))?;
        }

        let mut attributes = Attributes::new()?;
        unsafe {
            // Match std's spawned-child signal state: default SIGPIPE and an
            // empty mask. CLOEXEC_DEFAULT keeps every descriptor not named by
            // a file action, including peers' pipes, out of the child.
            let mut default_signals: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut default_signals);
            libc::sigaddset(&mut default_signals, libc::SIGPIPE);
            check(libc::posix_spawnattr_setsigdefault(
                &mut attributes.0,
                &default_signals,
            ))?;
            let mut empty_mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty_mask);
            check(libc::posix_spawnattr_setsigmask(
                &mut attributes.0,
                &empty_mask,
            ))?;
            let flags = libc::POSIX_SPAWN_SETSIGDEF
                | libc::POSIX_SPAWN_SETSIGMASK
                | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
            check(libc::posix_spawnattr_setflags(
                &mut attributes.0,
                flags as libc::c_short,
            ))?;
        }

        let mut pid: libc::pid_t = 0;
        // posix_spawnp searches this process's PATH, matching std's lookup
        // for a command that leaves PATH untouched.
        check(unsafe {
            libc::posix_spawnp(
                &mut pid,
                program.as_ptr(),
                &actions.0,
                &attributes.0,
                argv.as_ptr(),
                envp.as_ptr(),
            )
        })?;
        drop(stdout_write);
        drop(stderr_write);
        let stdin = stdin_pipe.map(|(read, write)| {
            drop(read);
            ChildStdin::from(write)
        });
        Ok(SpawnedGit {
            child: GitChild::Spawned(PosixSpawnedChild { pid, status: None }),
            stdin,
            stdout: Some(ChildStdout::from(stdout_read)),
            stderr: Some(ChildStderr::from(stderr_read)),
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    fn read_all(mut pipe: impl Read) -> String {
        let mut text = String::new();
        pipe.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn anchored_spawn_starts_in_the_held_directory_even_after_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let original = root.join("original");
        std::fs::create_dir(&original).unwrap();
        let handle = std::fs::File::open(&original).unwrap();
        let renamed = root.join("renamed");
        std::fs::rename(&original, &renamed).unwrap();
        // A decoy at the old path must not be where the child lands.
        std::fs::create_dir(&original).unwrap();

        let mut command = Command::new("pwd");
        command.arg("-P").current_dir("/");
        let mut spawned = spawn_git(command, Some(handle.as_raw_fd()), false).unwrap();
        let stdout = read_all(spawned.stdout.take().unwrap());
        let status = spawned.child.wait().unwrap();
        assert!(status.success());
        assert_eq!(stdout.trim(), renamed.to_str().unwrap());
    }

    #[test]
    fn anchored_spawn_applies_environment_changes_and_pipes_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();
        // SAFETY: test-only process environment mutation; the variable name
        // is unique to this test.
        unsafe { std::env::set_var("BBOX_GIT_SPAWN_TEST_REMOVED", "present") };
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "read line; printf '%s|%s|%s' \"$line\" \"$BBOX_GIT_SPAWN_TEST_SET\" \"${BBOX_GIT_SPAWN_TEST_REMOVED:-absent}\"; echo oops >&2; exit 3",
            ])
            .env("BBOX_GIT_SPAWN_TEST_SET", "set")
            .env_remove("BBOX_GIT_SPAWN_TEST_REMOVED");
        let mut spawned = spawn_git(command, Some(handle.as_raw_fd()), true).unwrap();
        let mut stdin = spawned.stdin.take().unwrap();
        stdin.write_all(b"hello\n").unwrap();
        drop(stdin);
        let stdout = read_all(spawned.stdout.take().unwrap());
        let stderr = read_all(spawned.stderr.take().unwrap());
        let status = spawned.child.wait().unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(stdout, "hello|set|absent");
        assert_eq!(stderr.trim(), "oops");
        // A reaped child reports the cached status again.
        assert_eq!(spawned.child.try_wait().unwrap(), Some(status));
    }

    #[test]
    fn anchored_spawn_does_not_leak_unrelated_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();
        // A descriptor without close-on-exec, as a careless peer might hold.
        let leaked = unsafe { libc::dup(handle.as_raw_fd()) };
        assert!(leaked >= 0);
        unsafe { libc::fcntl(leaked, libc::F_SETFD, 0) };
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("test -e /dev/fd/{leaked} && echo leaked || echo closed"),
        ]);
        let mut spawned = spawn_git(command, Some(handle.as_raw_fd()), false).unwrap();
        let stdout = read_all(spawned.stdout.take().unwrap());
        spawned.child.wait().unwrap();
        unsafe { libc::close(leaked) };
        assert_eq!(stdout.trim(), "closed");
    }

    #[test]
    fn anchored_spawn_kill_and_try_wait_bound_a_hung_child() {
        let dir = tempfile::tempdir().unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut spawned = spawn_git(command, Some(handle.as_raw_fd()), false).unwrap();
        assert!(spawned.child.try_wait().unwrap().is_none());
        spawned.child.kill().unwrap();
        let started = Instant::now();
        let status = loop {
            if let Some(status) = spawned.child.try_wait().unwrap() {
                break status;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(!status.success());
        // Killing an already-reaped child is a no-op, not a stray signal.
        spawned.child.kill().unwrap();
    }

    #[test]
    fn anchored_spawn_reports_a_missing_program() {
        let dir = tempfile::tempdir().unwrap();
        let handle = std::fs::File::open(dir.path()).unwrap();
        let command = Command::new("bbox-definitely-not-a-real-program");
        let error = spawn_git(command, Some(handle.as_raw_fd()), false)
            .err()
            .expect("missing program must fail to spawn");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
