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

pub mod code_facts;
pub mod edit_algebra;
pub mod ledger;
pub mod lsp_facts;

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
pub fn binding_tools() -> Vec<Arc<dyn Tool>> {
    let ledger = Arc::new(ledger::ProvenanceLedger::default());
    let mut tools = code_facts::tools();
    tools.extend(edit_algebra::tools(
        Arc::new(edit_algebra::EditStore::default()),
        Arc::clone(&ledger),
    ));
    tools.extend(lsp_facts::tools(
        Arc::new(lsp_facts::LspState::default()),
        ledger,
    ));
    tools
}

/// Namespace documentation + hand-authored TS declarations, keyed by
/// namespace name, for the exec tool description.
pub fn namespace_descriptions() -> BTreeMap<String, ToolNamespaceDescription> {
    BTreeMap::from([
        ("code".to_string(), code_facts::namespace_description()),
        ("edits".to_string(), edit_algebra::namespace_description()),
        ("lsp".to_string(), lsp_facts::namespace_description()),
    ])
}
