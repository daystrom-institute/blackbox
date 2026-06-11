// Phase 4 enforcement surface (concurrency-model §5): MCP handler modules
// must not call blocking fs/process APIs inline on tokio workers. Sanctioned
// exceptions (run_blocking-closure helpers, tracked migration debt) carry
// reasoned #[allow]s; everything else is a build error.
#![deny(clippy::disallowed_methods)]
pub mod agents;
pub mod artifacts;
pub mod atoms;
pub mod attention;
pub mod badgey;
pub mod badgey_adapter;
pub mod bro_helpers;
pub mod bro_params;
pub mod bro_runtime_params;
pub mod code_nav;
pub mod config;
pub mod councils;
pub mod dispatch;
pub mod gaps;
pub mod graph;
pub mod knowledge;
pub mod macros;
pub mod mcp_surface;
pub mod notes;
pub mod orchestrate;
pub mod packets;
pub mod projects;
pub mod refactor;
pub mod render;
pub mod roadmap;
pub mod roster;
pub mod sessions;
pub mod slices;
pub mod storage_gc;
pub mod storage_health;
pub mod storage_migration;
pub mod system_events;
pub mod threads;
pub mod transcripts;
pub mod whiteboards;
pub mod workspace;
