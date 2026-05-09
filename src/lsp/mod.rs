//! Long-lived LSP session management.
//!
//! Per-project LSP servers (JDTLS for Java, rust-analyzer for Rust)
//! behave like indexed workspace servers — they pay multi-second
//! cold-start cost on `initialize` and only become useful after
//! workspace import. Spawning them per-call wastes that work and
//! forces hacks like fixed `sleep` calls or external `timeout`
//! wrappers.
//!
//! [`session_manager::LspSessionManager`] keys live sessions on
//! `(canonical_project_root, Language)`, lazily spawns the matching
//! backend on the first request, reuses the same stdio handles for
//! every subsequent call, and evicts idle sessions on a background
//! tick. The daemon owns one global manager; refactor tools borrow a
//! short-lived [`session_manager::LspClient`] handle through
//! [`session_manager::LspSessionManager::with_session`].

pub mod session_manager;

pub use session_manager::{LspError, LspSessionManager};
