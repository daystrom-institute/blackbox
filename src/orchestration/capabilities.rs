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
use bro_capabilities::{
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, CorpusCapability, CorpusHit,
    CorpusLookup,
};
use bro_core::BroError;

use crate::server::state::{BlackboxServer, SharedState};
use crate::tools::bro_params::AtomInvokeParams;

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

/// Atom capability backed by the daemon's real invocation path.
pub(crate) struct DaemonAtoms {
    pub(crate) state: Arc<SharedState>,
}

#[async_trait]
impl AtomCapability for DaemonAtoms {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        // Reuse the exact server-side invocation path the MCP `atom_invoke`
        // tool uses, so policy (RX-V1/V2 governance, supervision, runtime
        // allocation) is identical for in-process and wire callers. The seam
        // stays input-JSON → output-JSON; edit-effect semantics (tx vs saga)
        // live behind this impl, not in the trait.
        let server = BlackboxServer::new(self.state.clone());
        let params = AtomInvokeParams {
            atom: invocation.atom.as_str().to_string(),
            args: invocation.input_json,
            project_dir: None,
            owner: None,
            parent_invocation_id: None,
            runtime: None,
            supervision_override: None,
            suppress_auto_supervision: false,
        };
        let output = server
            .atom_invoke_value(params, None)
            .await
            .map_err(|e| BroError::new("atom_invoke_failed", e))?;
        Ok(AtomOutput {
            output_json: output,
        })
    }
}

/// Install the daemon's in-memory capability implementations into the harness
/// capability slots. Called once at startup; the standalone harness binary
/// never calls this, so those surfaces fail closed by absence.
pub(crate) fn install(state: &Arc<SharedState>) {
    bro_harness::capabilities::install_corpus(Arc::new(DaemonCorpus {
        state: state.clone(),
    }));
    bro_harness::capabilities::install_atoms(Arc::new(DaemonAtoms {
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

    /// Invoking an unknown atom drives the real server-side invocation path and
    /// surfaces its error — proving the trait dispatch reaches live atom
    /// machinery (not a stub), without needing a full atom + agent dispatch.
    #[tokio::test]
    async fn daemon_atoms_reaches_real_invocation_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let atoms = DaemonAtoms {
            state: state.clone(),
        };
        let result = atoms
            .invoke_atom(AtomInvocation {
                atom: bro_core::AtomRef::new("atom:does-not-exist@v1"),
                input_json: serde_json::json!({}),
            })
            .await;
        let err = result.expect_err("unknown atom must error");
        assert_eq!(err.code, "atom_invoke_failed");
    }
}
