//! Tantivy-free transcript reading layer.
//!
//! Peeled out of `bbox-corpus-index` so a transcript collector can link
//! transcript reading without dragging in tantivy (remote-corpus-host design,
//! `design/daemon-runtime/remote-corpus-host.md`). This crate owns the source
//! types, the read-adapter trait + registry, the durable cursor store, the
//! interactive-CLI adapters, and the harness-sessions reader (including the
//! strict prefix reader `read_fleet_event_log_prefix`). The tantivy projection
//! half stays in `bbox-corpus-index`.
#![allow(dead_code)]

pub mod adapters;
pub mod cursor_store;
pub mod harness_sessions;
pub mod interactive;
pub mod types;
