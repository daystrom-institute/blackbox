//! Generic stateful-consultant runtime primitives.
//!
//! Extracted from the Badgey subsystem (gap-9dae9a60): instance identity and
//! registry, proposal store with a Pending→Applying→Applied/Failed state
//! machine, idempotent action journal, and per-instance resume-queue turn
//! serialization. The `descriptor` module is the configuration boundary: a
//! code-owned `ConsumerDescriptor` binds a consumer's vocabulary (id prefix,
//! intent-note grammar, brofile refs, proposal kinds) to this runtime. Badgey
//! is the first configured consumer (`orchestration::badgey::vocabulary`).
//! See design/orchestration/agents/consultant-runtime.md.

#![allow(dead_code)]

pub mod consumers;
pub mod descriptor;
pub mod events;
pub mod journal;
pub mod proposals;
pub mod queue;
pub mod registry;
pub mod types;

pub use journal::ActionJournal;
pub use proposals::ProposalStore;
pub use registry::ConsultantRegistry;
