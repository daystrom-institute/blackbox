//! bbox-edge-sidecar — daemon-internal edge sidecar persistence floor.
//!
//! Extracted from the root `blackbox` crate (root-crate-peels arc). Owns the
//! on-disk shape of the edge sidecar: the workspace/materialization manifest
//! (`manifest`), snapshot directory layout and clean/dirty-overlay switching
//! (`snapshot`), the JSONL edge-lane persistence primitives
//! (`edge_sidecar`), and the one-time purge of retired file-touch rows
//! (`file_touch_purge`). Store-agnostic: the store->edge emitters stay in the
//! root crate's `edge_index` module and call down into this crate.

#[cfg(not(unix))]
compile_error!(
    "bbox-edge-sidecar requires Unix descriptor confinement and file-lock semantics; no non-Unix persistence fallback is supported"
);

#[cfg(unix)]
pub mod edge_sidecar;
#[cfg(unix)]
pub mod file_touch_purge;
#[cfg(unix)]
pub mod manifest;
#[cfg(unix)]
pub mod migration_inventory;
#[cfg(unix)]
pub mod snapshot;
