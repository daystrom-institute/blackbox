//! Library surface for bro-harness.
//!
//! The binary remains a thin CLI wrapper. The library supports the isolate
//! binary, integration tests, and non-daemon embedders. `blackboxd` deliberately
//! does not link it; the daemon boundary is the standalone process protocol.

pub mod agent_loop;
pub mod bindings;
pub mod bound;
pub mod capabilities;
pub mod cli;
pub mod code_mode;
pub mod compaction;
pub mod context;
pub mod diagnostics;
pub mod emit;
pub mod event_log;
pub mod hooks;
pub mod lsp_baselines;
pub mod mcp;
pub mod project_doc;
pub mod registry;
pub mod report;
pub mod session;
pub mod transport;
