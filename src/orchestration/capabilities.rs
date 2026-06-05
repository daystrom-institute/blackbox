//! In-memory `bro_capabilities` implementations the daemon injects into the
//! in-process harness (harness-daemon-boundary.md §2/§6).
//!
//! These wrap existing daemon stores so a harness agent's corpus lookup is a
//! direct trait call against the live index — no MCP round-trip, no wire
//! serialization. The daemon implements the contract-bottom traits; the harness
//! consumes them; neither crate depends on the other (the dependency-inversion
//! shape — the traits live in `bro-capabilities`, below both).

use std::sync::Arc;

use std::collections::HashMap;

use async_trait::async_trait;
use bro_capabilities::{
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, CellLoadOutput, CellLoadRequest,
    CellRegisterOutput, CellRegisterRequest, CellRegistryCapability, CellScheduleOutput,
    CellScheduleRequest, CorpusCapability, CorpusHit, CorpusLookup, DurableCellCapability,
    DurableCellRegisterOutput, DurableCellRegisterRequest, RefactorCapability, RefactorPlanHandle,
    RefactorRequest,
};
use bro_core::BroError;
use parking_lot::RwLock;

use crate::refactor::{self, RefactorPlanParams};
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

/// Cell registry backed by the daemon's artifact catalog. This is deliberately
/// separate from `orchestration::atoms`: cells replace the atom backend taxonomy;
/// `atom:` survives only as an exact handle shape.
pub(crate) struct DaemonCells {
    pub(crate) state: Arc<SharedState>,
}

#[async_trait]
impl CellRegistryCapability for DaemonCells {
    async fn register_cell(
        &self,
        request: CellRegisterRequest,
    ) -> CapabilityResult<CellRegisterOutput> {
        crate::cells::register_cell(&self.state.artifacts.read(), request)
            .map_err(|e| BroError::new("cell_register_failed", e.to_string()))
    }

    async fn load_cell(&self, request: CellLoadRequest) -> CapabilityResult<CellLoadOutput> {
        crate::cells::load_cell(&self.state.artifacts.read(), request)
            .map_err(|e| BroError::new("cell_load_failed", e.to_string()))
    }
}

#[async_trait]
impl DurableCellCapability for DaemonCells {
    async fn register_durable_cell(
        &self,
        request: DurableCellRegisterRequest,
    ) -> CapabilityResult<DurableCellRegisterOutput> {
        crate::cells::register_durable_cell(&self.state.artifacts.read(), request)
            .map_err(|e| BroError::new("durable_cell_register_failed", e.to_string()))
    }

    async fn schedule_cell(
        &self,
        request: CellScheduleRequest,
    ) -> CapabilityResult<CellScheduleOutput> {
        crate::cells::schedule_cell(self.state.clone(), request)
            .map_err(|e| BroError::new("cell_schedule_failed", e.to_string()))
    }
}

/// Refactor capability backed by the daemon's real plan path. Produced plans
/// are kept host-side keyed by handle id; only the handle (id + preview)
/// crosses into the agent's context (§6/§9 ref-handle model).
pub(crate) struct DaemonRefactor {
    pub(crate) state: Arc<SharedState>,
    plans: RwLock<HashMap<String, serde_json::Value>>,
}

impl DaemonRefactor {
    pub(crate) fn new(state: Arc<SharedState>) -> Self {
        Self {
            state,
            plans: RwLock::new(HashMap::new()),
        }
    }
}

/// One-line preview of a plan for the model: kind plus edit count when present.
fn plan_preview(plan: &serde_json::Value) -> String {
    let kind = plan.get("kind").and_then(|k| k.as_str()).unwrap_or("plan");
    match plan.get("edits").and_then(|e| e.as_array()) {
        Some(edits) => format!("{kind}: {} edit(s)", edits.len()),
        None => kind.to_string(),
    }
}

#[async_trait]
impl RefactorCapability for DaemonRefactor {
    async fn plan_refactor(
        &self,
        request: RefactorRequest,
    ) -> CapabilityResult<RefactorPlanHandle> {
        // Merge the request kind into the params object, then deserialize the
        // exact RefactorPlanParams the MCP tool uses — same dispatch, same
        // governance (RX-V1/V2/V3 fail-closed behavior is unchanged).
        let mut params_value = match request.input_json {
            serde_json::Value::Object(map) => serde_json::Value::Object(map),
            serde_json::Value::Null => serde_json::json!({}),
            other => {
                return Err(BroError::new(
                    "bad_input",
                    format!("refactor params must be an object, got {other}"),
                ));
            }
        };
        params_value["kind"] = serde_json::Value::String(request.kind);
        let params: RefactorPlanParams = serde_json::from_value(params_value)
            .map_err(|e| BroError::new("bad_input", e.to_string()))?;

        let ctx = refactor::PlanContext {
            lsp: Some(self.state.lsp_sessions.clone()),
        };
        let plan_json = refactor::plan_with_ctx(&params, &ctx)
            .map_err(|e| BroError::new("refactor_plan_failed", e.to_string()))?;
        let plan: serde_json::Value = serde_json::from_str(&plan_json)
            .map_err(|e| BroError::new("refactor_plan_parse_failed", e.to_string()))?;

        let id = format!("ref:plan/{}", uuid::Uuid::new_v4());
        let preview = plan_preview(&plan);
        self.plans.write().insert(id.clone(), plan);
        Ok(RefactorPlanHandle { id, preview })
    }

    async fn materialize_plan(&self, id: String) -> CapabilityResult<serde_json::Value> {
        self.plans
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| BroError::new("unknown_plan", format!("no host-side plan for {id}")))
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
    bro_harness::capabilities::install_cells(Arc::new(DaemonCells {
        state: state.clone(),
    }));
    bro_harness::capabilities::install_durable_cells(Arc::new(DaemonCells {
        state: state.clone(),
    }));
    bro_harness::capabilities::install_refactor(Arc::new(DaemonRefactor::new(state.clone())));
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

    #[tokio::test]
    async fn daemon_cells_registers_and_loads_catalog_cell() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let cells = DaemonCells {
            state: state.clone(),
        };
        let registered = cells
            .register_cell(CellRegisterRequest {
                name: "math/double".to_string(),
                version: "v1".to_string(),
                source: "function run(input) { return input.n * 2; }".to_string(),
                contract_json: serde_json::json!({
                    "entry": "run",
                    "input": { "type": "object" },
                    "output": { "type": "integer" }
                }),
                description: Some("double an integer".to_string()),
                supersedes: None,
            })
            .await
            .expect("register cell");
        assert_eq!(registered.handle, "atom:math/double@v1");
        assert_eq!(registered.artifact_ref, "cell:math/double@v1");

        let loaded = cells
            .load_cell(CellLoadRequest {
                handle: "atom:math/double@v1".to_string(),
            })
            .await
            .expect("load cell");
        assert_eq!(loaded.name, "math/double");
        assert_eq!(loaded.version, "v1");
        assert!(loaded.source.contains("input.n * 2"));
        assert_eq!(loaded.contract_json["entry"], "run");
    }

    #[tokio::test]
    async fn daemon_cells_registers_and_schedules_durable_cell_without_legacy_routing() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let cells = DaemonCells {
            state: state.clone(),
        };
        let reusable = cells
            .register_cell(CellRegisterRequest {
                name: "math/inc".to_string(),
                version: "v1".to_string(),
                source: "function run(input) { return { n: input.n + 1 }; }".to_string(),
                contract_json: serde_json::json!({
                    "entry": "run",
                    "input": { "type": "object" },
                    "output": { "type": "object" }
                }),
                description: None,
                supersedes: None,
            })
            .await
            .expect("register reusable cell");

        let durable = cells
            .register_durable_cell(DurableCellRegisterRequest {
                name: "nightly-inc".to_string(),
                version: "v1".to_string(),
                cell_handle: reusable.handle.clone(),
                description: Some("scheduled increment cell".to_string()),
                supersedes: None,
            })
            .await
            .expect("register durable cell");
        assert_eq!(durable.handle, "cell:nightly-inc@v1");
        assert_eq!(durable.source_cell, "atom:math/inc@v1");

        let direct = crate::cells::run_cell_once(
            state.clone(),
            &durable.handle,
            serde_json::json!({ "n": 1 }),
        )
        .await
        .expect("run durable cell directly");
        assert_eq!(direct, serde_json::json!({ "n": 2 }));

        let scheduled = cells
            .schedule_cell(CellScheduleRequest {
                name: "nightly-inc".to_string(),
                cell_handle: durable.handle.clone(),
                schedule: "0 0 0 1 1 * 2099".to_string(),
                input_json: serde_json::json!({ "n": 41 }),
                concurrency: 1,
            })
            .await
            .expect("schedule durable cell");
        assert_eq!(scheduled.status, "scheduled");
        assert!(
            state
                .store_dir
                .join("cell-schedules")
                .join("nightly-inc.json")
                .exists()
        );
        assert!(state.workflow_registry.read().is_empty());
        assert!(state.crons.list().is_empty());
        assert!(
            state
                .packets
                .read()
                .load("domain:cell-schedule/nightly-inc")
                .is_err()
        );
    }

    /// A bogus plan kind drives the real plan dispatch and surfaces its error —
    /// proving plan_refactor reaches live refactor machinery, not a stub.
    #[tokio::test]
    async fn daemon_refactor_reaches_real_plan_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let refactor = DaemonRefactor::new(state);
        let result = refactor
            .plan_refactor(RefactorRequest {
                kind: "no_such_plan_kind".to_string(),
                input_json: serde_json::json!({ "source": "a.rs" }),
            })
            .await;
        assert!(result.is_err(), "unknown plan kind must error");
    }

    /// Materializing an id that was never produced is a clean error, not a
    /// panic — the handle is real (dereferenceable) or it fails.
    #[tokio::test]
    async fn daemon_refactor_unknown_plan_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(dir.path()));
        let refactor = DaemonRefactor::new(state);
        let err = refactor
            .materialize_plan("ref:plan/missing".to_string())
            .await
            .expect_err("unknown plan id must error");
        assert_eq!(err.code, "unknown_plan");
    }
}
