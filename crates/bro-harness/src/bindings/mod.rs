//! Domain bindings projected into code-mode cells as namespace globals.
//!
//! A *binding* is an ordinary [`bro_tools::Tool`] whose
//! [`Tool::namespace_binding`](bro_tools::Tool::namespace_binding) is set, so
//! code mode installs it as `<namespace>.<method>(...)` beside `tools` instead
//! of a flat `tools.*` property. Dispatch is unchanged — canonical name
//! through the same seam and filter. The namespace ships per the cell-DSL
//! contract (design/bro-harness/code-mode-cell-dsl.md §5): bindings +
//! hand-authored TS declarations + provenance tier (+ choke-point policy for
//! mutating namespaces; the fact namespaces here are pure).
//!
//! Everything here is harness-native — pure functions of the working set,
//! zero daemon reach-back (decision af3c4783). First tenant: the `code.*`
//! facts of design/bro-harness/refactor-v2-pressure-test.md §5.

pub mod analysis;
pub mod build_gate;
pub mod code_facts;
pub mod edit_algebra;
pub mod java_transforms;
pub mod ledger;
pub mod lsp_facts;
pub mod rust_transforms;

use std::collections::BTreeMap;
use std::sync::Arc;

use bro_code_mode::ToolNamespaceDescription;
use bro_tools::Tool;

/// Every domain-binding tool the harness ships, for the code-mode callable
/// set. Cell-only constructs: callers add these to the code-mode catalog +
/// seam (the tool filter still applies per name), never to the flat wire
/// surface.
///
/// Called once per session (agent_loop tool assembly), which is what scopes
/// the edits algebra's [`edit_algebra::EditStore`] and the provenance
/// [`ledger::ProvenanceLedger`] to the session: EditSets and issuance
/// records survive across cells, die with the session. The ledger is shared
/// between the producing bindings (`lsp.*`) and the consuming algebra
/// (`edits.merge` / `edits.apply`) — that shared seam is what makes
/// `semantic_status` lineage-computed rather than cell-claimed.
pub struct BindingToolSession {
    tools: Vec<Arc<dyn Tool>>,
    lsp_state: Arc<lsp_facts::LspState>,
}

impl BindingToolSession {
    pub fn new() -> Self {
        let ledger = Arc::new(ledger::ProvenanceLedger::default());
        let lsp_state = Arc::new(lsp_facts::LspState::default());
        let rust_ledger = Arc::clone(&ledger);
        let mut tools = code_facts::tools();
        tools.extend(edit_algebra::tools(
            Arc::new(edit_algebra::EditStore::default()),
            Arc::clone(&ledger),
        ));
        tools.extend(lsp_facts::tools(Arc::clone(&lsp_state), ledger));
        tools.extend(java_transforms::tools(Arc::clone(&lsp_state)));
        tools.extend(analysis::tools());
        tools.extend(build_gate::tools());
        tools.extend(rust_transforms::tools(rust_ledger));
        Self { tools, lsp_state }
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    pub async fn shutdown(&self) {
        self.lsp_state.shutdown_all().await;
    }
}

impl Default for BindingToolSession {
    fn default() -> Self {
        Self::new()
    }
}

pub fn binding_tools() -> Vec<Arc<dyn Tool>> {
    BindingToolSession::new().tools()
}

/// Namespace documentation + hand-authored TS declarations, keyed by
/// namespace name, for the exec tool description.
pub fn namespace_descriptions() -> BTreeMap<String, ToolNamespaceDescription> {
    BTreeMap::from([
        ("analysis".to_string(), analysis::namespace_description()),
        ("build".to_string(), build_gate::namespace_description()),
        ("code".to_string(), code_facts::namespace_description()),
        ("edits".to_string(), edit_algebra::namespace_description()),
        ("java".to_string(), java_transforms::namespace_description()),
        ("lsp".to_string(), lsp_facts::namespace_description()),
        ("rust".to_string(), rust_transforms::namespace_description()),
    ])
}
