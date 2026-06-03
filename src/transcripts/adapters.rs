use crate::index::ReindexConfig;
use crate::orchestration::providers::Provider;

use super::types::{
    TranscriptBatch, TranscriptCursor, TranscriptLocation, TranscriptReadError, TranscriptSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptScanTarget {
    Sessions,
    History,
}

pub(crate) trait TranscriptReadAdapter: Send + Sync {
    fn provider(&self) -> Provider;

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

    pub(crate) fn from_reindex_config(_config: &ReindexConfig) -> Self {
        Self::new(Vec::new())
    }

    pub(crate) fn from_runtime_config() -> Self {
        Self::new(Vec::new())
    }

    pub(crate) fn adapters(&self) -> impl Iterator<Item = &dyn TranscriptReadAdapter> {
        self.adapters.iter().map(|adapter| adapter.as_ref())
    }

    pub(crate) fn adapter(&self, provider: Provider) -> Option<&dyn TranscriptReadAdapter> {
        self.adapters()
            .find(|adapter| adapter.provider() == provider)
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

