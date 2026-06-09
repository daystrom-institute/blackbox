//! The codex `apply_patch` edit format: a lenient parser for the
//! `*** Begin Patch` envelope plus a fuzzy-context apply layer.
//!
//! Vendored and adapted from openai/codex (Apache-2.0); see `NOTICE`. The
//! parser and the `seek_sequence` fuzzy locator are vendored near-verbatim; the
//! apply layer (`apply.rs`) is a Blackbox-authored synchronous port over
//! `std::fs`.

mod apply;
mod parser;
mod seek_sequence;

use std::path::PathBuf;

pub use apply::{ApplyError, ApplyOutcome, FileAction, FileChange, apply_patch};
pub use parser::{Hunk, ParseError, UpdateFileChunk, parse_patch};

/// The Lark grammar that constrains `apply_patch` output on grammar-capable
/// transports (the Responses custom-tool `format`). Vendored verbatim from
/// openai/codex `core/src/tools/handlers/apply_patch.lark` (Apache-2.0, base
/// variant without the `environment_id` extension).
pub const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

/// The result of parsing a patch: the ordered hunks plus the normalized patch
/// text. (`workdir` / `environment_id` are retained from the upstream parser
/// shape; Blackbox does not currently consume them.)
#[derive(Debug, PartialEq, Clone)]
pub struct ApplyPatchArgs {
    pub hunks: Vec<Hunk>,
    pub patch: String,
    pub workdir: Option<PathBuf>,
    pub environment_id: Option<String>,
}
