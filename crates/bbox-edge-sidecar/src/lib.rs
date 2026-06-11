//! bbox-edge-sidecar — daemon-internal edge sidecar persistence floor.
//!
//! Extracted from the root `blackbox` crate (root-crate-peels arc). Owns the
//! on-disk shape of the edge sidecar: the workspace/materialization manifest
//! (`manifest`), snapshot directory layout and clean/dirty-overlay switching
//! (`snapshot`), and the JSONL edge-lane persistence primitives
//! (`edge_sidecar`). Store-agnostic: the store->edge emitters stay in the
//! root crate's `edge_index` module and call down into this crate.

pub mod edge_sidecar;
pub mod manifest;
pub mod snapshot;
