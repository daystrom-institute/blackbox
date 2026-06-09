use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct TextEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FileEdit {
    pub path: String,
    pub original_sha256: String,
    pub edits: Vec<TextEdit>,
    /// Pre-computed would-be file content (RX-A2). Populated for target files in
    /// `Blocked` plans so callers can grep for `FIXME(refactor-plan-only):` markers
    /// without re-applying the edits. Never written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}
