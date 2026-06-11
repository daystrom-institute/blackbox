use crate::index::ReindexConfig;
use bro_core::Provider;

use super::types::{
    TranscriptBatch, TranscriptCursor, TranscriptLocation, TranscriptReadError, TranscriptSnapshot,
    TranscriptSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptScanTarget {
    Sessions,
    History,
}

pub(crate) trait TranscriptReadAdapter: Send + Sync {
    fn source(&self) -> TranscriptSource;

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError>;

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError>;

    fn load_snapshot(
        &self,
        location: &TranscriptLocation,
    ) -> Result<TranscriptSnapshot, TranscriptReadError> {
        let batch = self.read_since(location, None)?;
        Ok(TranscriptSnapshot {
            location: location.clone(),
            events: batch.events,
            cursor: batch.cursor,
        })
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError>;
}

pub(crate) struct TranscriptAdapterRegistry {
    adapters: Vec<Box<dyn TranscriptReadAdapter>>,
}

impl TranscriptAdapterRegistry {
    pub(crate) fn new(adapters: Vec<Box<dyn TranscriptReadAdapter>>) -> Self {
        Self { adapters }
    }

    /// Registry for index-time scans. Every source root must be explicit in
    /// the config — harness sessions dir, interactive claude roots, codex
    /// root, gemini tmp root are all `None`/empty in hermetic test indexes —
    /// so reindex never silently scans the operator's real state.
    pub(crate) fn from_reindex_config(config: &ReindexConfig) -> Self {
        let mut adapters: Vec<Box<dyn TranscriptReadAdapter>> = Vec::new();
        if let Some(dir) = &config.harness_sessions_dir {
            adapters.extend(super::harness_sessions::HarnessSessionsAdapter::all_for_dir(dir));
        }
        if !config.roots.is_empty() {
            adapters.push(Box::new(super::interactive::ClaudeTranscriptAdapter::new(
                config.roots.clone(),
            )));
        }
        if let Some(codex_root) = config.codex_root.clone() {
            adapters.push(Box::new(super::interactive::CodexTranscriptAdapter::new(
                codex_root,
            )));
        }
        if let Some(tmp_root) = config.gemini_tmp_root.clone() {
            adapters.push(Box::new(super::interactive::GeminiTranscriptAdapter::new(
                tmp_root,
            )));
        }
        Self::new(adapters)
    }

    /// Registry for runtime lookups (`locate` for task transcript handles).
    /// Resolves the harness sessions dir from the live environment — the
    /// daemon exports `BRO_HOME` during startup, matching where in-process
    /// harness sessions write. Interactive sources are index-time only:
    /// tasks are never dispatched to them, so they have no runtime handles.
    pub(crate) fn from_runtime_config() -> Self {
        let dir = super::harness_sessions::env_sessions_dir();
        Self::new(super::harness_sessions::HarnessSessionsAdapter::all_for_dir(&dir))
    }

    pub(crate) fn adapters(&self) -> impl Iterator<Item = &dyn TranscriptReadAdapter> {
        self.adapters.iter().map(|adapter| adapter.as_ref())
    }

    pub(crate) fn adapter_for(
        &self,
        source: TranscriptSource,
    ) -> Option<&dyn TranscriptReadAdapter> {
        self.adapters().find(|adapter| adapter.source() == source)
    }

    /// Lookup by dispatch provider — the shape runtime callers (task
    /// transcript handles) think in.
    pub(crate) fn adapter(&self, provider: Provider) -> Option<&dyn TranscriptReadAdapter> {
        self.adapter_for(TranscriptSource::Harness(provider))
    }

    pub(crate) fn locate(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        let Some(adapter) = self.adapter(provider) else {
            return Ok(None);
        };
        adapter.locate(session_id)
    }
}
