//! Generic stateful-consultant runtime primitives.
//!
//! Extracted from the Badgey subsystem (gap-9dae9a60): instance identity and
//! registry, proposal store with a Pending→Applying→Applied/Failed state
//! machine, idempotent action journal, and per-instance resume-queue turn
//! serialization. Badgey is the first configured consumer; consumer-specific
//! vocabulary (intent-note grammar, proposal kinds, event mapping) still lives
//! in `orchestration::badgey` until the consumer-descriptor phase.
//! See design/orchestration/agents/consultant-runtime.md.

#![allow(dead_code)]

pub mod journal;
pub mod proposals;
pub mod queue;
pub mod registry;
pub mod types;

pub use journal::ActionJournal;
pub use proposals::ProposalStore;
pub use registry::ConsultantRegistry;
