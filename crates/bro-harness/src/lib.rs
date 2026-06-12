//! Library surface for bro-harness.
//!
//! The binary remains a thin CLI wrapper. Keeping module ownership here lets the
//! daemon link and drive the harness in-process in a later slice without
//! changing the existing subprocess entrypoint.

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
