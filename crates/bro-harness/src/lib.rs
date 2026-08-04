//! Library surface for bro-harness.
//!
//! The binary remains a thin CLI wrapper. The library supports the isolate
//! binary, integration tests, and non-daemon embedders. `blackboxd` deliberately
//! does not link it; the daemon boundary is the standalone process protocol.

// Phase 4 concurrency enforcement (concurrency-model §5): this crate denies
// clippy::disallowed_methods so blocking fs and process calls stay out of
// production actor contexts. Test code legitimately spawns processes and
// touches the filesystem, so the lint is allowed only under cfg(test). The
// non-test build of this same code is still checked: --all-targets compiles
// this target without cfg(test) too, with the deny in force.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

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
