//! bbox-edge-index: storage health, retention planning and garbage
//! collection over the on-disk edge sidecar (snapshots, git overlays and
//! lane files). The daemon keeps no in-memory edge graph; the sidecar
//! remains the code-source activation and git-overlay authority.

pub mod migration;
pub mod storage_health;

#[cfg(test)]
mod sidecar_tests;
