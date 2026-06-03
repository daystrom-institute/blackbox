//! In-memory `bro_capabilities` implementations the daemon injects into the
//! in-process harness (harness-daemon-boundary.md §2/§6).
//!
//! These wrap existing daemon stores so a harness agent's corpus lookup is a
//! direct trait call against the live index — no MCP round-trip, no wire
//! serialization. The daemon implements the contract-bottom traits; the harness
//! consumes them; neither crate depends on the other (the dependency-inversion
//! shape — the traits live in `bro-capabilities`, below both).

use std::sync::Arc;

use async_trait::async_trait;
use bro_capabilities::{CapabilityResult, CorpusCapability, CorpusHit, CorpusLookup};
use bro_core::BroError;

use crate::server::state::SharedState;

/// Corpus capability backed by the daemon's live transcript index.
pub(crate) struct DaemonCorpus {
    pub(crate) state: Arc<SharedState>,
}

#[async_trait]
impl CorpusCapability for DaemonCorpus {
    async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        // Direct in-memory call against the live tantivy reader. The read guard
        // is held only for the synchronous search; no await happens under it.
        let hits = self
            .state
            .idx
            .read()
            .hybrid_bm25_hits(&lookup.query, lookup.limit, None)
            .map_err(|e| BroError::new("corpus_search_failed", e.to_string()))?;
        Ok(hits
            .into_iter()
            .map(|h| CorpusHit {
                id: h.entity_id,
                text: match h.title {
                    Some(title) => format!("{title}\n{}", h.excerpt),
                    None => h.excerpt,
                },
            })
            .collect())
    }
}

/// Install the daemon's in-memory capability implementations into the harness
/// capability slots. Called once at startup; the standalone harness binary
/// never calls this, so those surfaces fail closed by absence.
pub(crate) fn install(state: &Arc<SharedState>) {
    bro_harness::capabilities::install_corpus(Arc::new(DaemonCorpus {
        state: state.clone(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability call reaches the daemon's real in-memory tantivy reader
    /// (not a stub, not a wire). An empty test index returns no hits — the point
    /// is that the trait dispatch lands on the live reader and returns Ok.
    #[tokio::test]
    async fn daemon_corpus_searches_live_index() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let corpus = DaemonCorpus {
            state: state.clone(),
        };
        let hits = corpus
            .search_corpus(CorpusLookup {
                query: "anything".to_string(),
                limit: 5,
            })
            .await
            .expect("search against live empty index should succeed");
        assert!(hits.is_empty(), "empty test index yields no hits");
    }
}
