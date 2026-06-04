//! Cross-turn diagnostics baselines persisted on the session `side` cell.
//!
//! For each tracked file the harness records the last analyzer pass: the file
//! content sha256 it was computed against, a monotonic version, and the
//! opaque diagnostics payload itself. A future LSP-driven differ uses each
//! baseline to surface only NEW/CHANGED findings on the next edit ("did I
//! just break this, or was it already broken?").
//!
//! The diagnostics payload is intentionally `serde_json::Value` so this
//! substrate has zero LSP dependency: swapping it for typed
//! `Vec<lsp_types::Diagnostic>` later is a small change.
//!
//! The `from_side` / `to_side` round-trip mirrors the other session side cells:
//! malformed or absent state yields an empty store rather than failing a resume.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One file's last-known diagnostic state. `diagnostics` is opaque — the
/// caller decides whether it's a `lsp_types::PublishDiagnosticsParams`, a raw
/// `Vec<Diagnostic>`, or some condensed projection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Baseline {
    /// sha256 of the file contents the diagnostics were computed against.
    pub sha256: String,
    /// Monotonic per-file version, bumped each time the baseline is updated.
    /// Lets a differ tell two equal-hash baselines apart when an analyzer is
    /// re-run on identical content.
    pub version: u64,
    /// Opaque diagnostics payload. `Value::Null` when no diagnostics have
    /// been recorded yet.
    pub diagnostics: Value,
}

/// The full set of per-file baselines. Keyed by the file's worktree-relative
/// path (the producer chooses; the cell does not enforce a layout) — using
/// the path as-is keeps the cell legible across resumes.
#[derive(Debug, Clone, Default)]
pub struct LspBaselines {
    pub files: BTreeMap<String, Baseline>,
}

impl LspBaselines {
    /// Seed from the persisted `side.lsp_baselines` value. Tolerant of
    /// malformed/absent state, like [`bro_tools::TodoList::from_side`].
    pub fn from_side(v: &Value) -> Self {
        let files: BTreeMap<String, Baseline> = v
            .get("files")
            .and_then(|f| serde_json::from_value(f.clone()).ok())
            .unwrap_or_default();
        Self { files }
    }

    /// Serialize back into the `side` cell.
    pub fn to_side(&self) -> Value {
        json!({ "files": self.files })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn baseline(sha: &str, version: u64, diag: Value) -> Baseline {
        Baseline {
            sha256: sha.into(),
            version,
            diagnostics: diag,
        }
    }

    #[test]
    fn round_trip_through_side() {
        let mut bl = LspBaselines::default();
        bl.files.insert(
            "src/lib.rs".into(),
            baseline("abc", 1, json!([{"range": "...", "message": "unused"}])),
        );
        bl.files
            .insert("src/main.rs".into(), baseline("def", 4, Value::Null));

        let side = bl.to_side();
        let back = LspBaselines::from_side(&side);

        assert_eq!(back.files.len(), 2);
        assert_eq!(back.files["src/lib.rs"], bl.files["src/lib.rs"]);
        assert_eq!(back.files["src/main.rs"], bl.files["src/main.rs"]);
    }

    #[test]
    fn tolerates_null_and_garbage() {
        assert!(LspBaselines::from_side(&Value::Null).files.is_empty());
        assert!(LspBaselines::from_side(&json!("nope")).files.is_empty());
        assert!(
            LspBaselines::from_side(&json!({"files": "garbage"}))
                .files
                .is_empty()
        );
    }
}
