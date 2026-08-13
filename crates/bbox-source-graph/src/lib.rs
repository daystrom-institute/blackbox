//! Connector-managed source graph projections: the dedicated store that
//! accepts one atomic snapshot of descriptor, schema, normalized graph facts,
//! source observation references, and a named checkpoint transition.
//!
//! # Where this lives, and why not under a graph root
//!
//! Connector-managed generations live in their OWN store root, under neither
//! `.bbox/graphs` (project-authored, committed) nor `.bbox/local/graphs`
//! (provisional scratch). The separation is the authority boundary made
//! physical: a checkout has no path that writes here, and this store refuses
//! any descriptor that does not carry connector authority, so a connector
//! refresh has no path to project-authored facts. `GraphSource::
//! ConnectorManaged` is deliberately rootless in the kernel, so checkout
//! discovery cannot reach these graphs however it is asked.
//!
//! # One authority file, N self-verifying advisory files
//!
//! Per accepted source graph the layout is:
//!
//! ```text
//! <root>/scopes/<scope-digest>/graphs/<graph-id>/
//!     snapshot.json                              the ONLY authority
//!     snapshot.prior.json                        advisory: prior generation
//!     observations/sha256/<hh>/<digest>.json     advisory: retained batches
//! ```
//!
//! There is deliberately NO manifest and NO index sidecar. The accepted
//! generation, its schema, its facts, its checkpoint set, and its observation
//! references are one value in one file, so the pair-of-stores tear that the
//! code-source activation journal has to reconcile against the edge-sidecar
//! workspace manifest (see `bbox_indexing::code_source_locality_manifest`, and
//! its "three readers, two stores" note) has no analogue here. Every other
//! file in the layout is derived, advisory, and independently self-verifying:
//! a retained observation blob is named by the digest of its own batch bytes,
//! and the prior snapshot carries the same integrity fields the authority file
//! does.
//!
//! # Write ordering and its crash windows
//!
//! One `accept` issues up to three durable writes, in this literal order, all
//! under the store lock on `snapshot.json`, and each individually crash safe
//! (temp file, fsync, atomic rename, parent fsync):
//!
//! 1. `observations/sha256/<hh>/<digest>.json` - retain the accepted batch,
//!    content addressed. Skipped when the digest is already present, because
//!    content-addressed files are immutable once written.
//! 2. `snapshot.prior.json` - copy the currently accepted snapshot aside for
//!    diagnosis. Skipped on the first accepted generation.
//! 3. `snapshot.json` - install the new accepted generation. THE COMMIT POINT.
//!
//! Because the authority is a single file installed by a single rename, the
//! crash windows are all benign and none of them needs a repair pass:
//!
//! - Crash between 1 and 2, or between 2 and 3: the accepted generation and
//!   the accepted checkpoint set are exactly what they were before the call.
//!   The reader consults `snapshot.json` alone, so the observation blob is an
//!   unreferenced retained batch and the prior copy is a duplicate of the
//!   still-accepted generation. Neither can be mistaken for an acceptance.
//!   The unreferenced blob is reclaimed by the next retention sweep, and an
//!   exact replay of the same batch rewrites identical bytes to the same path
//!   rather than accumulating a second copy.
//! - Crash during 3: rename is atomic, so a reader sees either the whole old
//!   generation or the whole new one, never a torn snapshot.
//! - `snapshot.prior.json` is never consulted for authority. A prior copy
//!   whose generation is not exactly one less than the accepted generation is
//!   the crash-window duplicate above; it is reported as unavailable for
//!   diagnosis and overwritten by the next accept.
//!
//! A rejected acceptance (invalid graph, checkpoint conflict, schema
//! rollback, generation conflict, or a snapshot that fails its own integrity
//! check on open) never reaches step 3, so it leaves the accepted generation
//! and the accepted checkpoint set unchanged. That is the same discipline the
//! code-source activation path uses: stage, validate, flip atomically, retain
//! the prior generation for diagnosis.
//!
//! # What this crate is not
//!
//! Observation is not projection. A connector satellite contacts the remote
//! system on the producer plane and ships an [`ObservationBatch`]; a
//! [`GraphProjection`] turns an accepted batch into a [`GraphDelta`]
//! deterministically, without remote access, which is what makes reprojection
//! from retained observations possible corpus side. This crate owns neither
//! leg: it owns the durable acceptance of the projection's result.

mod delta;
mod observations;
mod paths;
mod projection;
mod store;

pub use delta::*;
pub use observations::*;
pub use paths::*;
pub use projection::*;
pub use store::*;
