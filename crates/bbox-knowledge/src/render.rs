//! Global guidance render primitives.
//!
//! The managed-region patch machinery, global target resolution, and the
//! global render authority check live in `bbox_util::global_render` so the
//! thin `bro` client can apply a daemon-computed global render plan on an
//! operator host without linking the knowledge store. This module keeps the
//! historical `bbox_knowledge::render::*` paths as re-exports.
pub use bbox_util::global_render::*;
