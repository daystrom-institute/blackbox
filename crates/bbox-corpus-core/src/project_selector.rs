//! Pure selector types for the shared project resolver
//! (design/daemon-runtime/durable-project-catalog-phase2-impl.md §5).
//!
//! These types live below the resolver engine (`bbox-indexing`) so lower
//! crates can consume resolution outputs without depending on the engine or
//! either store implementation. Nothing here touches the filesystem.
//!
//! Typed-value integrity: the version-1 arm of every output wraps
//! `ProjectRecord`-derived data under its own variant. A v1 resolution can
//! never be read as catalog authority, and no `CorpusProject`,
//! `PublishedScope`, or `CheckoutAttachment` is ever synthesized from
//! version-1 hints.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::project_catalog::{AttachmentCapabilities, AttachmentKind, CorpusProject};
use crate::project_record::ProjectRecord;

/// How a resolution will be used. Preserves the deliberate read/write
/// asymmetry of the underlying gates instead of collapsing it: retrieval may
/// alias broadly, writes must not.
///
/// Moved here from `bbox-indexing::projects` so pure consumers can name it;
/// `bbox-indexing` re-exports it under the original path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResolveIntent {
    /// Retrieval scope ("which corpus do I query?"): the broad gate.
    #[default]
    Read,
    /// Write-side aliasing (where durable state lands): the conservative
    /// managed gate.
    Write,
}

/// The caller-class taxonomy of phase-2 §5.2, fixed at the call site and
/// never inferred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectorClass {
    /// The caller needs exactly one project (writes, administration, graph
    /// project operations, lease acquisition). Unknown or ambiguous
    /// selectors fail closed on the catalog backend.
    #[default]
    Selection,
    /// The caller narrows a query. A selector that resolves narrows by
    /// identity; one that does not resolve keeps the surface's documented
    /// literal semantics and never manufactures identity.
    Filter,
}

/// Session-authoritative checkout identity supplied by the daemon when a
/// request runs inside a pinned session checkout. Used only by the final
/// legacy fallback arm and by attachment selection; never trusted from
/// request bodies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCheckoutRef {
    /// Durable checkout id from `.bbox/local/checkout-id` when known.
    pub checkout_id: Option<String>,
    /// Canonical checkout project dir for the session.
    pub checkout_project_dir: Option<String>,
}

/// One resolver request. `scope` is populated only by operator/internal APIs
/// that explicitly accept a typed scope; string selectors are never sniffed
/// into scopes.
#[derive(Debug, Clone, Default)]
pub struct ProjectSelectorRequest {
    /// Raw caller-supplied selector string, if any.
    pub selector: Option<String>,
    /// Explicit typed scope for scope-accepting APIs.
    pub scope: Option<crate::identity::PublishedScope>,
    /// Explicit attachment selection for path operations.
    pub attachment_id: Option<String>,
    /// Session checkout, when the daemon has one for this request.
    pub session: Option<SessionCheckoutRef>,
    pub intent: ResolveIntent,
    pub class: SelectorClass,
}

impl ProjectSelectorRequest {
    pub fn selection(selector: impl Into<String>, intent: ResolveIntent) -> Self {
        Self {
            selector: Some(selector.into()),
            intent,
            class: SelectorClass::Selection,
            ..Self::default()
        }
    }

    pub fn filter(selector: impl Into<String>) -> Self {
        Self {
            selector: Some(selector.into()),
            intent: ResolveIntent::Read,
            class: SelectorClass::Filter,
            ..Self::default()
        }
    }
}

/// Bounded, typed resolver failure (phase-2 §5.4 closed code set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectResolveError {
    code: &'static str,
    detail: String,
}

const MAX_ERROR_DETAIL_BYTES: usize = 256;

impl ProjectResolveError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_ERROR_DETAIL_BYTES {
            let mut cut = MAX_ERROR_DETAIL_BYTES;
            while !detail.is_char_boundary(cut) {
                cut -= 1;
            }
            detail.truncate(cut);
        }
        Self { code, detail }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub fn selector_unknown(selector: &str) -> Self {
        Self::new("error.project_selector_unknown", selector)
    }

    pub fn selector_ambiguous(detail: impl Into<String>) -> Self {
        Self::new("error.project_selector_ambiguous", detail)
    }

    pub fn alias_conflict(alias: &str) -> Self {
        Self::new("error.project_alias_conflict", alias)
    }

    pub fn scope_unknown() -> Self {
        Self::new("error.project_scope_unknown", "no project owns this scope")
    }

    pub fn attachment_required(project_id: &str) -> Self {
        Self::new("error.project_attachment_required", project_id)
    }

    pub fn attachment_ambiguous(project_id: &str, candidates: usize) -> Self {
        Self::new(
            "error.project_attachment_ambiguous",
            format!("{project_id}: {candidates} active attachments"),
        )
    }

    pub fn capability_denied(detail: impl Into<String>) -> Self {
        Self::new("error.project_capability_denied", detail)
    }

    pub fn catalog_inactive() -> Self {
        Self::new(
            "error.project_catalog_inactive",
            "the durable project catalog is not the active runtime authority",
        )
    }
}

impl std::fmt::Display for ProjectResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ProjectResolveError {}

/// Which compatibility lane produced a non-identity literal-filter outcome.
/// Counted per surface for the phase-6 retirement observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityLane {
    /// The permanent unregistered-cwd substring lane (governing decision 9):
    /// the selector matched nothing and the surface keeps its documented
    /// literal semantics with no catalog identity and no filesystem
    /// authority.
    UnregisteredLiteral,
}

/// Backend-tagged project identity view (phase-2 §5.3). The v1 arm carries
/// the joined record; the catalog arm carries the real catalog project.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedProjectIdentity {
    V1Compat { record: ProjectRecord },
    Catalog { project: CorpusProject },
}

impl ResolvedProjectIdentity {
    pub fn project_id(&self) -> &str {
        match self {
            ResolvedProjectIdentity::V1Compat { record } => &record.project_id,
            ResolvedProjectIdentity::Catalog { project } => project.project_id.as_str(),
        }
    }

    pub fn display(&self) -> &str {
        match self {
            ResolvedProjectIdentity::V1Compat { record } => &record.canonical_path,
            ResolvedProjectIdentity::Catalog { project } => &project.display_name,
        }
    }

    pub fn aliases(&self) -> BTreeSet<String> {
        match self {
            ResolvedProjectIdentity::V1Compat { record } => record.aliases.clone(),
            ResolvedProjectIdentity::Catalog { project } => project
                .operator_aliases
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
        }
    }
}

/// Backend-tagged attachment view. The v1 arm mirrors the legacy
/// `CheckoutContext`; the catalog arm names a validated attachment.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAttachment {
    V1Compat {
        /// Canonical top of the checkout containing the input (the base root
        /// itself when resolution did not pass through a worktree).
        checkout_dir: String,
        /// True for the base root and managed worktrees (the write-side
        /// aliasing gate); false for recognized-but-unmanaged checkouts.
        managed: bool,
        checkout_id: Option<String>,
    },
    Catalog {
        attachment_id: String,
        kind: AttachmentKind,
        checkout_dir: String,
        checkout_project_dir: String,
        capabilities: AttachmentCapabilities,
    },
}

impl ResolvedAttachment {
    /// The directory path operations act within.
    pub fn checkout_dir(&self) -> &str {
        match self {
            ResolvedAttachment::V1Compat { checkout_dir, .. } => checkout_dir,
            ResolvedAttachment::Catalog { checkout_dir, .. } => checkout_dir,
        }
    }
}

/// Resolution stopped at the catalog: corpus identity with no filesystem
/// authority (governing §5.3 stopping point one).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogProjectContext {
    pub project: ResolvedProjectIdentity,
}

/// Resolution selected one attachment (governing §5.3 stopping point two).
/// Still carries no filesystem authority: leases come from the broker.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachedProjectContext {
    pub project: ResolvedProjectIdentity,
    pub attachment: ResolvedAttachment,
    /// Durable path-lane store key under the §5.3 key-to-base rule: the v1
    /// record's canonical path, or the catalog project's active base
    /// attachment dir (falling back to the resolving attachment's dir when
    /// no base attachment exists).
    pub store_key: String,
}

/// One resolver outcome (phase-2 §5.3).
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectResolution {
    Catalog(CatalogProjectContext),
    Attached(AttachedProjectContext),
    /// Filter-class miss: the surface keeps its literal semantics. Never
    /// returned for `SelectorClass::Selection`.
    LiteralFilter {
        raw: String,
        lane: CompatibilityLane,
    },
}

impl ProjectResolution {
    pub fn project_id(&self) -> Option<&str> {
        match self {
            ProjectResolution::Catalog(ctx) => Some(ctx.project.project_id()),
            ProjectResolution::Attached(ctx) => Some(ctx.project.project_id()),
            ProjectResolution::LiteralFilter { .. } => None,
        }
    }

    /// Path-lane store key when the resolution carries one.
    pub fn store_key(&self) -> Option<&str> {
        match self {
            ProjectResolution::Attached(ctx) => Some(&ctx.store_key),
            ProjectResolution::Catalog(_) | ProjectResolution::LiteralFilter { .. } => None,
        }
    }
}
