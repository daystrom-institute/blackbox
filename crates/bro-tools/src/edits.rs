//! Per-turn capture of file mutations for window-0 diagnostics.
//!
//! File-mutating tools (`file_write`, `file_edit`, …) push an [`EditEvent`]
//! into [`EditSink`] at mutation time, recording the pre-image bytes plus
//! pre/post sha256 and the absolute path. The harness's edit loop drains the
//! sink after each tool-call dispatch, so an analyzer pass — LSP or
//! otherwise — can answer "which file changed, content before, content after"
//! without re-reading and re-diffing the worktree from scratch.
//!
//! This sink rides `ToolCx` like the other shared per-turn cells, but it is
//! NOT persisted to the session `side` blob: it is ephemeral substrate the
//! loop consumes within the same turn and discards. Persistent diagnostics
//! state lives in the harness's `LspBaselines` cell.

use crate::slice_core::sha256_hex;
use std::path::PathBuf;

/// One file-mutating tool call recorded at mutation time.
#[derive(Debug, Clone)]
pub struct EditEvent {
    /// Absolute path of the mutated file, as resolved against the worktree
    /// root.
    pub path: PathBuf,
    /// Bytes the file held immediately before the mutation; empty when the
    /// file did not exist (a fresh `file_write`).
    pub pre_image: Vec<u8>,
    /// sha256 of `pre_image`. Empty pre-image hashes to the empty-input
    /// digest, so callers can distinguish "new file" from "unchanged" by
    /// comparing against `post_sha256`.
    pub pre_sha256: String,
    /// sha256 of the bytes the file holds after the mutation succeeded.
    pub post_sha256: String,
}

impl EditEvent {
    /// Build an event from pre/post byte slices, hashing both.
    pub fn from_bytes(path: PathBuf, pre: &[u8], post: &[u8]) -> Self {
        Self {
            path,
            pre_image: pre.to_vec(),
            pre_sha256: sha256_hex(pre),
            post_sha256: sha256_hex(post),
        }
    }
}

/// In-memory sink of edits recorded during a dispatch round. The edit loop
/// drains it between tool dispatches.
#[derive(Debug, Clone, Default)]
pub struct EditSink {
    events: Vec<EditEvent>,
}

impl EditSink {
    pub fn push(&mut self, event: EditEvent) {
        self.events.push(event);
    }

    /// Borrow the events without consuming them.
    pub fn events(&self) -> &[EditEvent] {
        &self.events
    }

    /// Move every recorded event out and reset the sink. Used by the loop
    /// after each tool dispatch.
    pub fn drain(&mut self) -> Vec<EditEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}
