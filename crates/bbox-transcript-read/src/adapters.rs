use bro_core::Provider;

use crate::types::{
    TranscriptBatch, TranscriptCursor, TranscriptLocation, TranscriptReadError, TranscriptSnapshot,
    TranscriptSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptScanTarget {
    Sessions,
    History,
}

pub trait TranscriptReadAdapter: Send + Sync {
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

pub struct TranscriptAdapterRegistry {
    adapters: Vec<Box<dyn TranscriptReadAdapter>>,
}

impl TranscriptAdapterRegistry {
    pub fn new(adapters: Vec<Box<dyn TranscriptReadAdapter>>) -> Self {
        Self { adapters }
    }

    /// Registry for runtime lookups (`locate` for task transcript handles).
    /// Resolves the harness sessions dir from the live environment — the
    /// daemon exports `BRO_HOME` during startup, matching where in-process
    /// harness sessions write. Interactive sources are index-time only:
    /// tasks are never dispatched to them, so they have no runtime handles.
    ///
    /// The index-time registry (`from_reindex_config`) is config-dependent and
    /// lives in `bbox-corpus-index` (see
    /// `bbox_corpus_index::transcripts::registry_from_reindex_config`) so this
    /// leaf crate stays config-agnostic.
    pub fn from_runtime_config() -> Self {
        let dir = crate::harness_sessions::env_sessions_dir();
        Self::new(crate::harness_sessions::HarnessSessionsAdapter::all_for_dir(&dir))
    }

    pub fn adapters(&self) -> impl Iterator<Item = &dyn TranscriptReadAdapter> {
        self.adapters.iter().map(|adapter| adapter.as_ref())
    }

    pub fn adapter_for(&self, source: TranscriptSource) -> Option<&dyn TranscriptReadAdapter> {
        self.adapters().find(|adapter| adapter.source() == source)
    }

    /// Lookup by dispatch provider — the shape runtime callers (task
    /// transcript handles) think in.
    pub fn adapter(&self, provider: Provider) -> Option<&dyn TranscriptReadAdapter> {
        self.adapter_for(TranscriptSource::Harness(provider))
    }

    pub fn locate(
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
