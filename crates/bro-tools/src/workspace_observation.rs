//! Shared observation accounting for bounded workspace search and glob walks.
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(super) const VISIT_LIMIT: usize = 20_000;
pub(super) const FILE_BYTES: u64 = 2_000_000;

#[derive(Default)]
pub(super) struct Observation {
    pub visited: usize,
    pub inspected_files: usize,
    pub oversized: usize,
    pub invalid_utf8: usize,
    pub binary: usize,
    pub read_errors: usize,
    pub walk_errors: usize,
    pub pruned_dirs: usize,
    pub skipped_symlinks: usize,
    pub traversal_limit: bool,
    pub result_limit: bool,
    pub output_limit: bool,
    pub cancelled: bool,
    examples: Vec<String>,
}

impl Observation {
    pub fn skip(&mut self, path: &Path, reason: &str) {
        if self.examples.len() < 3 {
            let path: String = path.display().to_string().chars().take(80).collect();
            self.examples.push(format!("{reason}={path}"));
        }
    }

    fn footer(&self) -> String {
        let complete = self.oversized
            + self.invalid_utf8
            + self.binary
            + self.read_errors
            + self.walk_errors
            + self.pruned_dirs
            + self.skipped_symlinks
            == 0
            && !self.traversal_limit
            && !self.result_limit
            && !self.output_limit
            && !self.cancelled;
        let examples = if self.examples.is_empty() {
            String::new()
        } else {
            format!("; examples={}", self.examples.join(", "))
        };
        format!(
            "[observation output_limit={}; complete_within_scope={}; inspected_files={}; oversized={}; invalid_utf8={}; binary={}; read_errors={}; walk_errors={}; pruned_dirs={}; skipped_symlinks={}; visited={}; traversal_limit={}; result_limit={}; cancelled={}; scope=visible unignored regular files, descendant build/dependency directories excluded{}]",
            self.output_limit,
            complete,
            self.inspected_files,
            self.oversized,
            self.invalid_utf8,
            self.binary,
            self.read_errors,
            self.walk_errors,
            self.pruned_dirs,
            self.skipped_symlinks,
            self.visited,
            self.traversal_limit,
            self.result_limit,
            self.cancelled,
            examples
        )
    }
}

pub(super) struct ObservedWalk {
    walk: ignore::Walk,
    observation: Arc<Mutex<Observation>>,
    limit: usize,
    done: bool,
}

impl ObservedWalk {
    pub fn new(base: &Path, observation: Arc<Mutex<Observation>>, limit: usize) -> Self {
        let capture = observation.clone();
        let walk = ignore::WalkBuilder::new(base)
            .require_git(false)
            .filter_entry(move |entry| {
                // A caller-supplied root named build/target is still a target.
                // The heuristic applies only to descendant directories.
                if entry.depth() > 0
                    && entry.file_type().is_some_and(|kind| kind.is_dir())
                    && super::PRUNE_DIRS.contains(&entry.file_name().to_str().unwrap_or_default())
                {
                    let mut observation = capture.lock().unwrap();
                    observation.pruned_dirs += 1;
                    observation.skip(entry.path(), "pruned");
                    return false;
                }
                true
            })
            .build();
        Self {
            walk,
            observation,
            limit,
            done: false,
        }
    }
}

impl Iterator for ObservedWalk {
    type Item = ignore::DirEntry;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let next = self.walk.next()?;
            let mut observation = self.observation.lock().unwrap();
            if observation.visited >= self.limit {
                observation.traversal_limit = true;
                self.done = true;
                return None;
            }
            observation.visited += 1;
            match next {
                Ok(entry) => {
                    if entry.error().is_some() {
                        observation.walk_errors += 1;
                        observation.skip(entry.path(), "ignore_error");
                    }
                    if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
                        observation.skipped_symlinks += 1;
                        observation.skip(entry.path(), "symlink");
                    }
                    return Some(entry);
                }
                Err(_) => {
                    observation.walk_errors += 1;
                }
            }
        }
    }
}

// Invoked only from the owning tools' blocking closures.
#[allow(clippy::disallowed_methods)]
pub(super) fn read_text(path: &Path, observation: &mut Observation) -> Option<String> {
    let read = || -> std::io::Result<Option<Vec<u8>>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A path swapped to a FIFO after traversal must not block open.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        if metadata.len() > FILE_BYTES {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take(FILE_BYTES + 1).read_to_end(&mut bytes)?;
        Ok((bytes.len() as u64 <= FILE_BYTES).then_some(bytes))
    };
    // Opening once and bounding the actual read prevents a file that grows
    // after metadata inspection from bypassing the memory cap.
    let bytes = match read() {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            observation.oversized += 1;
            observation.skip(path, "oversized");
            return None;
        }
        Err(_) => {
            observation.read_errors += 1;
            observation.skip(path, "read_error");
            return None;
        }
    };
    if bytes.contains(&0) {
        observation.binary += 1;
        observation.skip(path, "binary");
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(text) => {
            observation.inspected_files += 1;
            Some(text)
        }
        Err(_) => {
            observation.invalid_utf8 += 1;
            observation.skip(path, "invalid_utf8");
            None
        }
    }
}

pub(super) fn finish(body: &str, observation: &mut Observation, host_budget: usize) -> String {
    let budget = if host_budget == 0 {
        8000
    } else {
        host_budget.min(8000)
    };
    let mut footer = observation.footer();
    if body.len() + footer.len() + 1 > budget {
        observation.output_limit = true;
        footer = observation.footer();
    }
    let available = budget.saturating_sub(footer.len() + 1);
    if available == 0 {
        return crate::output::truncate_text(&footer, budget);
    }
    format!(
        "{}\n{}",
        crate::output::truncate_text(body, available),
        footer
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn walk_limit_counts_nonmatching_entries_and_discloses_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["one", "two", "three"] {
            std::fs::write(root.join(name), b"text").unwrap();
        }
        let observation = Arc::new(Mutex::new(Observation::default()));
        let entries: Vec<_> = ObservedWalk::new(&root, observation.clone(), 2).collect();
        assert_eq!(entries.len(), 2);
        assert!(observation.lock().unwrap().traversal_limit);
        assert_eq!(observation.lock().unwrap().visited, 2);
    }

    #[test]
    fn read_errors_and_traversal_errors_are_not_empty_successes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut observation = Observation::default();
        assert!(read_text(&root.join("missing"), &mut observation).is_none());
        assert_eq!(observation.read_errors, 1);
        let observed = Arc::new(Mutex::new(Observation::default()));
        assert_eq!(
            ObservedWalk::new(&root.join("missing"), observed.clone(), 10).count(),
            0
        );
        assert_eq!(observed.lock().unwrap().walk_errors, 1);
    }
}
