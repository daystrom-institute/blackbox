// Phase 4 enforcement surface (concurrency-model §5): MCP handler modules
// must not call blocking fs/process APIs inline on tokio workers. Sanctioned
// exceptions (run_blocking-closure helpers, tracked migration debt) carry
// reasoned #[allow]s; everything else is a build error.
#![deny(clippy::disallowed_methods)]
// Tests in these modules legitimately build fixture trees and spawn git, and
// they run on their own threads rather than tokio workers, so the deny above
// has no production surface to protect there. The handler bodies it does
// protect are still checked: --all-targets compiles these modules without
// cfg(test) too, and scripts/lint-concurrency.sh is the syntactic backstop.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
pub mod agents;
pub mod artifacts;
pub mod atoms;
pub mod attention;
pub mod badgey;
pub mod badgey_adapter;
pub mod bro_helpers;
pub mod bro_params;
pub mod bro_runtime_params;
pub mod config;
pub mod consultant;
pub mod dispatch;
pub mod doctor;
pub mod gaps;
pub mod graph;
pub mod knowledge;
pub mod mcp_surface;
pub mod notes;
pub mod orchestrate;
pub mod packets;
pub mod project_catalog;
pub mod projects;
pub mod render;
pub mod roadmap;
pub mod roster;
pub mod scope;
pub mod sessions;
pub mod storage_gc;
pub mod storage_health;
pub mod storage_migration;
pub mod system_events;
pub mod threads;
pub mod transcripts;
pub mod whiteboards;
pub mod workspace;
