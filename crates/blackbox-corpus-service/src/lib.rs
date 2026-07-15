//! Dependency-clean blackbox corpus service.
//!
//! This crate is deliberately below every live-execution and operational
//! implementation. It owns retained records and corpus retrieval, and exposes
//! only typed capability and record-ingest endpoints.

mod records;
mod service;

pub use records::RecordStore;
pub use service::{CorpusService, RoutedCapabilityRequest, router, serve, unix_time_ms};

use std::path::PathBuf;

/// Filesystem roots required by the corpus service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusServicePaths {
    /// Existing or newly-created Tantivy index directory.
    pub index_path: PathBuf,
    /// Root containing the private `record-ingest` snapshot directory.
    pub record_root: PathBuf,
    /// Canonicalizable roots containing fleet-owned harness event logs.
    pub transcript_roots: Vec<PathBuf>,
    /// Shared same-host daemon bearer credential.
    pub service_token_path: PathBuf,
}

impl CorpusServicePaths {
    pub fn new(index_path: impl Into<PathBuf>, record_root: impl Into<PathBuf>) -> Self {
        let record_root = record_root.into();
        let token_root = record_root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(&record_root);
        Self {
            index_path: index_path.into(),
            service_token_path: token_root.join("service.token"),
            record_root,
            transcript_roots: Vec::new(),
        }
    }

    pub fn with_transcript_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.transcript_roots = roots;
        self
    }

    pub fn with_service_token_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.service_token_path = path.into();
        self
    }
}
