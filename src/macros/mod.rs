//! Macro layer — data model and bounded expression DSL.
//!
// Foundation module: the types and DSL here are constructed/consumed by the
// macro registry, MCP surface, planner, and backend in later milestones
// (M2-M4). Allow dead_code until those consumers land, then remove.
#![allow(dead_code)]
//!
//!
//! This module contains the pure data and pure evaluation logic for the macro
//! synthesis layer described in `design/refactor-tools/unified-code-synthesis-model.md`.
//!
//! # Contents
//!
//! - [`model`] — `MacroDefinition`, `MacroInvocation`, `MacroPlan`, `EditSet`,
//!   and all supporting types.
//! - [`expr`] — Bounded predicate DSL: dotted-path `Context` lookup,
//!   `Predicate` eval, and `${path}` interpolation.
//!
//! # What is NOT here
//!
//! - No filesystem access.
//! - No MCP tool wiring.
//! - No daemon state or registry.
//! - No Java parsing / source printing.
//! - No search engine calls.
//!
//! Those concerns land in later milestones (Phase 3 — Java backend sidecar;
//! Phase 4 — probe bindings; Phase 2 MCP surface).

pub mod backend;
pub mod expr;
pub mod java_sidecar;
pub mod java_sidecar_protocol;
pub mod model;
pub mod planner;
pub mod planner_ctx;
pub mod probe;
pub mod registry;
pub mod sidecar_backend;

// Convenience re-exports for crate-internal consumers. The MCP surface,
// registry, planner, and backend that consume these land in later macro
// milestones (M2-M4); allow unused until then.
#[allow(unused_imports)]
pub use backend::{
    BackendEditSet, JavaEmitOp, JavaMacroBackend, JavaRewriteOp, UnavailableBackend,
};
#[allow(unused_imports)]
pub use expr::{Context, InterpolateError, Predicate, eval, interpolate};
#[allow(unused_imports)]
pub use model::{
    EditSet, MacroAnchors, MacroDefinition, MacroInvocation, MacroOperation, MacroPlan,
    MacroPlanCheck, MacroPlanOperation, MacroProbe, MacroRefusal, MacroRefusalHit, MacroScope,
    MacroSemanticStatus, MacroValidation,
};
#[allow(unused_imports)]
pub use planner::MacroPlanner;
#[allow(unused_imports)]
pub use planner_ctx::MacroPlannerContext;
#[allow(unused_imports)]
pub use sidecar_backend::SidecarBackend;
#[allow(unused_imports)]
pub use probe::{CodeNavProbeRunner, ProbeOutput, ProbeRunner, ProbeSpec, UnavailableProbeRunner};
#[allow(unused_imports)]
pub use registry::{MacroRegistry, RegistryError};
