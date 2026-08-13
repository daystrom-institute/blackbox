//! Tenant-owned cross-entity evidence bindings.
//!
//! Evidence bindings bind two authority planes without collapsing them: a
//! tenant record vertex can point at a source vertex in a different graph, and
//! a source vertex can point at a materialized project file, while each plane
//! keeps its own generation and custody. The bindings themselves are neither
//! plane's property. They live in `.bbox/evidence/bindings.json`, outside both
//! project-authored graph facts (`.bbox/graphs`) and connector-managed source
//! snapshots, and reach the corpus over the repo-owned state lane exactly like
//! `.bbox/knowledge` and `.bbox/graphs` do.
//!
//! # Why the committed document is not written in canonical EntityRefs
//!
//! Runtime endpoints ARE canonical [`EntityRef`]s and generic edge kinds; that
//! is the whole point of the milestone, and no connector-specific entity
//! variant is introduced. The committed authoring form is deliberately one
//! step short of canonical for project-scoped endpoints, because a canonical
//! `project_file` or `project_graph_vertex` ref embeds a `project_id`, and
//! `project_id` is assigned by whichever host registered the checkout. Baking
//! a host-assigned id into a committed repo file would make the file wrong on
//! every other host that clones it. So a project-scoped endpoint names only
//! the parts the repo actually owns (graph id, vertex id, path/chunk hashes)
//! and the loader materializes the canonical ref with the project id supplied
//! by the lane it arrived on. Endpoints that carry no project scope
//! (`knowledge:`, `thread:`, `commit:`, ...) are authored as canonical ref
//! strings directly through [`EvidenceEndpointV1::Ref`], which refuses
//! project-scoped types for exactly this reason.
//!
//! # What this module does and does not own
//!
//! It owns the document schema, validation, endpoint resolution into canonical
//! refs, the accepted-set replacement rule, and the endpoint-status ALGEBRA
//! (how an observation becomes current/stale/missing/unauthorized/unresolved,
//! and how two endpoint statuses aggregate into one edge freshness). It does
//! not own the observation itself: looking up whether a vertex is live in the
//! current generation needs the daemon's view catalog, so the read plane makes
//! the observation and this module scores it. That split keeps the kernel a
//! leaf crate rather than reaching up into the indexing layer.

use std::collections::{BTreeMap, BTreeSet};

use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use serde::{Deserialize, Serialize};

/// Schema version of `.bbox/evidence/bindings.json`.
pub const EVIDENCE_DOCUMENT_VERSION: u32 = 1;

/// Repo-relative path of the committed bindings document.
pub const EVIDENCE_BINDINGS_PATH: &str = ".bbox/evidence/bindings.json";

/// Repo-relative root of the evidence lane.
pub const EVIDENCE_LANE_ROOT: &str = ".bbox/evidence";

/// Filename the evidence lane admits. The lane is a single document rather
/// than a directory tree: one complete valid document replaces one project's
/// accepted binding set, so there is nothing to merge across files.
pub const EVIDENCE_BINDINGS_FILENAME: &str = "bindings.json";

pub const EVIDENCE_META_BINDING_ID: &str = "evidence.binding_id";
pub const EVIDENCE_META_PROJECT_ID: &str = "evidence.project_id";
pub const EVIDENCE_META_ASSERTION_AUTHORITY: &str = "evidence.assertion_authority";
pub const EVIDENCE_META_ASSERTED_AT: &str = "evidence.asserted_at";
pub const EVIDENCE_META_OBSERVATION_ID: &str = "evidence.observation_id";
pub const EVIDENCE_META_MAPPING_VERSION: &str = "evidence.mapping_version";
pub const EVIDENCE_META_SOURCE_GENERATION: &str = "evidence.source_generation";
pub const EVIDENCE_META_TARGET_GENERATION: &str = "evidence.target_generation";
pub const EVIDENCE_META_SOURCE_STATUS: &str = "evidence.source_status";
pub const EVIDENCE_META_TARGET_STATUS: &str = "evidence.target_status";
pub const EVIDENCE_META_FRESHNESS: &str = "evidence.freshness";

/// Who asserted a binding. This is provenance, not permission: an operator
/// assertion is not more authoritative than a connector one, it is differently
/// sourced, and each authority owes a different provenance field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAssertionAuthority {
    /// Asserted by project-authored mapping rules committed to the repo.
    Project,
    /// Asserted by a connector from a specific remote observation.
    Connector,
    /// Asserted by a human operator action.
    Operator,
}

impl EvidenceAssertionAuthority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Connector => "connector",
            Self::Operator => "operator",
        }
    }
}

/// A binding endpoint as authored in the committed document.
///
/// Project-scoped variants omit `project_id` on purpose (see the module
/// docs); [`EvidenceEndpointV1::resolve`] supplies it from the lane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceEndpointV1 {
    /// A vertex in one of this project's graphs, in either authority plane.
    GraphVertex { graph_id: String, vertex_id: String },
    /// A chunk of a materialized project file.
    ProjectFile {
        rel_path_hash: String,
        chunk_hash: String,
        #[serde(default)]
        occurrence_idx: u32,
    },
    /// A durable knowledge entry.
    Knowledge { id: String },
    /// Any canonical, project-independent entity ref, verbatim.
    Ref { entity_ref: String },
}

impl EvidenceEndpointV1 {
    /// Materializes the canonical runtime ref for this endpoint.
    ///
    /// `project_id` comes from the lane the document arrived on, never from
    /// the document itself.
    pub fn resolve(&self, project_id: &str) -> Result<EntityRef, EvidenceEndpointError> {
        match self {
            Self::GraphVertex {
                graph_id,
                vertex_id,
            } => Ok(EntityRef::ProjectGraphVertex {
                project_id: project_id.to_string(),
                graph_id: graph_id.clone(),
                vertex_id: vertex_id.clone(),
            }),
            Self::ProjectFile {
                rel_path_hash,
                chunk_hash,
                occurrence_idx,
            } => Ok(EntityRef::ProjectFile {
                project_id: project_id.to_string(),
                rel_path_hash: rel_path_hash.clone(),
                chunk_hash: chunk_hash.clone(),
                occurrence_idx: *occurrence_idx,
            }),
            Self::Knowledge { id } => Ok(EntityRef::Knowledge { id: id.clone() }),
            Self::Ref { entity_ref } => {
                let parsed = EntityRef::parse(entity_ref)
                    .map_err(|_| EvidenceEndpointError::Unparseable(entity_ref.clone()))?;
                if project_scoped_type(parsed.entity_type()) {
                    return Err(EvidenceEndpointError::ProjectScopedRef(entity_ref.clone()));
                }
                Ok(parsed)
            }
        }
    }
}

/// Why an authored endpoint could not become a canonical ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceEndpointError {
    /// The literal ref string is not a canonical entity ref at all.
    Unparseable(String),
    /// The literal ref string names a project-scoped type, which would bake a
    /// host-assigned project id into committed state. Use the typed variant.
    ProjectScopedRef(String),
}

impl EvidenceEndpointError {
    fn code(&self) -> &'static str {
        match self {
            Self::Unparseable(_) => "evidence.unparseable_endpoint",
            Self::ProjectScopedRef(_) => "evidence.project_scoped_literal_ref",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Unparseable(value) => {
                format!("endpoint `{value}` is not a canonical entity ref")
            }
            Self::ProjectScopedRef(value) => format!(
                "endpoint `{value}` names a project-scoped type; author it with the typed endpoint \
                 form so the committed document carries no host-assigned project id"
            ),
        }
    }
}

/// True for entity types whose canonical ref embeds a host-assigned project id.
fn project_scoped_type(entity_type: EntityType) -> bool {
    matches!(
        entity_type,
        EntityType::ProjectFile
            | EntityType::ProjectFileV2
            | EntityType::ProjectGraphVertex
            | EntityType::ProvisionalProjectGraphVertex
            | EntityType::Symbol
            | EntityType::SymbolV2
    )
}

/// One binding as authored in the committed document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBindingV1 {
    pub binding_id: String,
    pub source: EvidenceEndpointV1,
    /// Generic edge kind. Namespaced project vocabulary, never a Rust variant.
    pub kind: String,
    pub target: EvidenceEndpointV1,
    pub assertion_authority: EvidenceAssertionAuthority,
    /// Required for connector assertions: which remote observation produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<String>,
    /// Required for project and operator assertions: which mapping produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_version: Option<String>,
    pub asserted_at: String,
    /// Generation the source endpoint was observed at, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_generation: Option<u64>,
    /// Generation the target endpoint was observed at, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_generation: Option<u64>,
}

/// The committed document. One complete valid document replaces one project's
/// accepted binding set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDocumentV1 {
    pub version: u32,
    #[serde(default)]
    pub bindings: Vec<EvidenceBindingV1>,
}

/// A binding with its endpoints materialized as canonical refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceBinding {
    pub binding_id: String,
    pub project_id: String,
    pub source: EntityRef,
    pub kind: String,
    pub target: EntityRef,
    pub assertion_authority: EvidenceAssertionAuthority,
    pub observation_id: Option<String>,
    pub mapping_version: Option<String>,
    pub asserted_at: String,
    pub source_generation: Option<u64>,
    pub target_generation: Option<u64>,
}

/// A structured validation failure. Codes are stable and machine-readable.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvidenceValidationError {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    pub message: String,
}

impl EvidenceValidationError {
    fn document(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            binding_id: None,
            message: message.into(),
        }
    }

    fn binding(binding_id: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            binding_id: Some(binding_id.to_string()),
            message: message.into(),
        }
    }
}

/// Bounds on one evidence document.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceParseLimits {
    pub max_bindings: usize,
}

impl Default for EvidenceParseLimits {
    fn default() -> Self {
        Self {
            max_bindings: 20_000,
        }
    }
}

/// One project's accepted binding set.
///
/// An empty set is the correct reading of an absent lane, which is what every
/// pre-evidence checkout presents. Nothing distinguishes "this project has no
/// bindings" from "this checkout predates the evidence lane", and nothing
/// needs to: both traverse identically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceBindingSet {
    bindings: Vec<EvidenceBinding>,
    forward: BTreeMap<String, Vec<usize>>,
    reverse: BTreeMap<String, Vec<usize>>,
}

impl EvidenceBindingSet {
    pub fn from_bindings(bindings: Vec<EvidenceBinding>) -> Self {
        let mut forward: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut reverse: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, binding) in bindings.iter().enumerate() {
            forward
                .entry(binding.source.render())
                .or_default()
                .push(index);
            reverse
                .entry(binding.target.render())
                .or_default()
                .push(index);
        }
        Self {
            bindings,
            forward,
            reverse,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &EvidenceBinding> {
        self.bindings.iter()
    }

    /// Bindings whose SOURCE endpoint is `entity`.
    pub fn forward(&self, entity: &EntityRef) -> Vec<&EvidenceBinding> {
        self.lookup(&self.forward, entity)
    }

    /// Bindings whose TARGET endpoint is `entity`.
    pub fn reverse(&self, entity: &EntityRef) -> Vec<&EvidenceBinding> {
        self.lookup(&self.reverse, entity)
    }

    fn lookup(
        &self,
        index: &BTreeMap<String, Vec<usize>>,
        entity: &EntityRef,
    ) -> Vec<&EvidenceBinding> {
        index
            .get(&entity.render())
            .map(|slots| slots.iter().map(|slot| &self.bindings[*slot]).collect())
            .unwrap_or_default()
    }

    /// Every distinct endpoint mentioned by the set, in rendered form.
    pub fn endpoints(&self) -> BTreeSet<String> {
        self.forward
            .keys()
            .chain(self.reverse.keys())
            .cloned()
            .collect()
    }
}

/// Outcome of parsing one candidate document.
///
/// `bindings` is `Some` only for a complete valid document. An invalid
/// candidate returns `None` with the reasons, and the caller keeps its prior
/// accepted set: a bad commit degrades freshness reporting at worst, it never
/// silently drops tenant-owned assertions.
#[derive(Debug, Clone)]
pub struct EvidenceLoad {
    pub bindings: Option<EvidenceBindingSet>,
    pub errors: Vec<EvidenceValidationError>,
}

impl EvidenceLoad {
    pub fn valid(&self) -> bool {
        self.bindings.is_some()
    }

    /// The accepted set for an absent lane: empty and valid.
    pub fn absent() -> Self {
        Self {
            bindings: Some(EvidenceBindingSet::default()),
            errors: Vec::new(),
        }
    }

    fn rejected(errors: Vec<EvidenceValidationError>) -> Self {
        Self {
            bindings: None,
            errors,
        }
    }
}

/// Parses and validates one candidate `bindings.json` for `project_id`.
pub fn parse_evidence_document(
    project_id: &str,
    bytes: &[u8],
    limits: EvidenceParseLimits,
) -> EvidenceLoad {
    let document: EvidenceDocumentV1 = match serde_json::from_slice(bytes) {
        Ok(document) => document,
        Err(error) => {
            return EvidenceLoad::rejected(vec![EvidenceValidationError::document(
                "evidence.unparseable_document",
                format!("evidence bindings document did not parse: {error}"),
            )]);
        }
    };
    resolve_evidence_document(project_id, &document, limits)
}

/// Validates an already-decoded document and resolves it into an accepted set.
pub fn resolve_evidence_document(
    project_id: &str,
    document: &EvidenceDocumentV1,
    limits: EvidenceParseLimits,
) -> EvidenceLoad {
    let errors = validate_evidence_document(document, limits);
    if !errors.is_empty() {
        return EvidenceLoad::rejected(errors);
    }
    let mut resolved = Vec::with_capacity(document.bindings.len());
    let mut errors = Vec::new();
    for binding in &document.bindings {
        let source = match binding.source.resolve(project_id) {
            Ok(entity) => entity,
            Err(error) => {
                errors.push(EvidenceValidationError::binding(
                    &binding.binding_id,
                    error.code(),
                    error.message(),
                ));
                continue;
            }
        };
        let target = match binding.target.resolve(project_id) {
            Ok(entity) => entity,
            Err(error) => {
                errors.push(EvidenceValidationError::binding(
                    &binding.binding_id,
                    error.code(),
                    error.message(),
                ));
                continue;
            }
        };
        if source == target {
            errors.push(EvidenceValidationError::binding(
                &binding.binding_id,
                "evidence.self_edge",
                "an evidence binding must connect two distinct entity refs",
            ));
            continue;
        }
        resolved.push(EvidenceBinding {
            binding_id: binding.binding_id.clone(),
            project_id: project_id.to_string(),
            source,
            kind: binding.kind.clone(),
            target,
            assertion_authority: binding.assertion_authority,
            observation_id: binding.observation_id.clone(),
            mapping_version: binding.mapping_version.clone(),
            asserted_at: binding.asserted_at.clone(),
            source_generation: binding.source_generation,
            target_generation: binding.target_generation,
        });
    }
    if !errors.is_empty() {
        return EvidenceLoad::rejected(errors);
    }
    EvidenceLoad {
        bindings: Some(EvidenceBindingSet::from_bindings(resolved)),
        errors: Vec::new(),
    }
}

/// Structural validation of a decoded document, before endpoint resolution.
pub fn validate_evidence_document(
    document: &EvidenceDocumentV1,
    limits: EvidenceParseLimits,
) -> Vec<EvidenceValidationError> {
    let mut errors = Vec::new();
    if document.version != EVIDENCE_DOCUMENT_VERSION {
        errors.push(EvidenceValidationError::document(
            "evidence.unsupported_version",
            format!(
                "evidence document version {} is unsupported; expected {EVIDENCE_DOCUMENT_VERSION}",
                document.version
            ),
        ));
    }
    if document.bindings.len() > limits.max_bindings {
        errors.push(EvidenceValidationError::document(
            "evidence.binding_limit",
            format!(
                "evidence document declares {} bindings, above the {} limit",
                document.bindings.len(),
                limits.max_bindings
            ),
        ));
    }

    let mut seen = BTreeSet::new();
    for binding in &document.bindings {
        let id = binding.binding_id.as_str();
        if id.trim().is_empty() || id.bytes().any(|byte| byte.is_ascii_control()) {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.invalid_binding_id",
                "binding_id must be non-empty and contain no control characters",
            ));
        } else if !seen.insert(id) {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.duplicate_binding_id",
                format!("binding_id `{id}` appears more than once"),
            ));
        }
        if binding.kind.trim().is_empty()
            || binding.kind.bytes().any(|byte| byte.is_ascii_control())
        {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.invalid_kind",
                "evidence kind must be non-empty and contain no control characters",
            ));
        }
        if binding.asserted_at.trim().is_empty() {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.missing_asserted_at",
                "asserted_at must not be empty",
            ));
        }
        if binding.assertion_authority == EvidenceAssertionAuthority::Connector
            && binding
                .observation_id
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.connector_observation_required",
                "connector assertions require observation_id",
            ));
        }
        if matches!(
            binding.assertion_authority,
            EvidenceAssertionAuthority::Project | EvidenceAssertionAuthority::Operator
        ) && binding
            .mapping_version
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.mapping_version_required",
                "project and operator assertions require mapping_version",
            ));
        }
        if !matches!(
            (&binding.source, &binding.target),
            (EvidenceEndpointV1::GraphVertex { .. }, _)
                | (_, EvidenceEndpointV1::GraphVertex { .. })
        ) {
            errors.push(EvidenceValidationError::binding(
                id,
                "evidence.graph_endpoint_required",
                "at least one evidence endpoint must be a project graph vertex",
            ));
        }
    }
    errors
}

/// Freshness of one endpoint, resolved at traversal and bundle time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceEndpointStatus {
    /// Live, and at the generation the binding recorded (or the binding
    /// recorded none).
    Current,
    /// Live, but not at the generation the binding recorded; or recorded a
    /// generation and the endpoint's graph is gone.
    Stale,
    /// Not live, and the binding recorded no generation to be stale against.
    Missing,
    /// Outside the binding's authorized project scope. Retained for
    /// diagnosis; never crossed by path traversal.
    Unauthorized,
    /// A kind of endpoint this read plane cannot resolve a generation for.
    Unresolved,
}

impl EvidenceEndpointStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Missing => "missing",
            Self::Unauthorized => "unauthorized",
            Self::Unresolved => "unresolved",
        }
    }

    /// Whether path traversal may cross an edge with this endpoint status.
    ///
    /// Only `Unauthorized` refuses. Stale and missing endpoints stay
    /// traversable and labeled, because a stale chain is still the answer to
    /// "what did we assert?"; an unauthorized one is a scope violation and is
    /// never an answer.
    pub fn traversable(self) -> bool {
        !matches!(self, Self::Unauthorized)
    }
}

/// What the read plane saw when it looked an endpoint up.
///
/// The read plane observes; this module scores. Keeping the two apart is what
/// lets the status algebra be unit-tested without a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceEndpointObservation {
    /// The endpoint belongs to a different project scope than the binding.
    OutOfScope,
    /// This endpoint kind carries no generation the read plane can compare.
    Unresolvable,
    /// The endpoint is not live in the requested visibility.
    Absent,
    /// The endpoint is live, at this generation when it has one.
    Present { generation: Option<u64> },
}

/// Scores one observation against the generation the binding recorded.
pub fn resolve_endpoint_status(
    observation: EvidenceEndpointObservation,
    expected_generation: Option<u64>,
) -> EvidenceEndpointStatus {
    match observation {
        EvidenceEndpointObservation::OutOfScope => EvidenceEndpointStatus::Unauthorized,
        EvidenceEndpointObservation::Unresolvable => EvidenceEndpointStatus::Unresolved,
        // A binding that recorded a generation and now finds nothing is
        // STALE, not missing: we know it used to be there. Without a recorded
        // generation there is nothing to be stale against, so it is missing.
        EvidenceEndpointObservation::Absent => {
            if expected_generation.is_some() {
                EvidenceEndpointStatus::Stale
            } else {
                EvidenceEndpointStatus::Missing
            }
        }
        EvidenceEndpointObservation::Present { generation } => {
            match (expected_generation, generation) {
                (Some(expected), Some(actual)) if expected != actual => {
                    EvidenceEndpointStatus::Stale
                }
                _ => EvidenceEndpointStatus::Current,
            }
        }
    }
}

/// Collapses two endpoint statuses into one edge freshness.
///
/// Precedence is worst-first: unauthorized beats stale beats missing beats
/// unresolved beats current. An edge is only current when both ends are.
pub fn aggregate_endpoint_status(
    source: EvidenceEndpointStatus,
    target: EvidenceEndpointStatus,
) -> EvidenceEndpointStatus {
    for status in [
        EvidenceEndpointStatus::Unauthorized,
        EvidenceEndpointStatus::Stale,
        EvidenceEndpointStatus::Missing,
        EvidenceEndpointStatus::Unresolved,
    ] {
        if source == status || target == status {
            return status;
        }
    }
    EvidenceEndpointStatus::Current
}

/// The metadata every read-plane surface attaches to an evidence edge.
///
/// One producer so `bbox_inspect_entity`, `bbox_find_paths`, and
/// `bbox_bundle_evidence` cannot drift into three dialects of the same edge.
pub fn binding_metadata(
    binding: &EvidenceBinding,
    source_status: EvidenceEndpointStatus,
    target_status: EvidenceEndpointStatus,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::from([
        (
            EVIDENCE_META_BINDING_ID.to_string(),
            binding.binding_id.clone(),
        ),
        (
            EVIDENCE_META_PROJECT_ID.to_string(),
            binding.project_id.clone(),
        ),
        (
            EVIDENCE_META_ASSERTION_AUTHORITY.to_string(),
            binding.assertion_authority.as_str().to_string(),
        ),
        (
            EVIDENCE_META_ASSERTED_AT.to_string(),
            binding.asserted_at.clone(),
        ),
        (
            EVIDENCE_META_SOURCE_STATUS.to_string(),
            source_status.as_str().to_string(),
        ),
        (
            EVIDENCE_META_TARGET_STATUS.to_string(),
            target_status.as_str().to_string(),
        ),
        (
            EVIDENCE_META_FRESHNESS.to_string(),
            aggregate_endpoint_status(source_status, target_status)
                .as_str()
                .to_string(),
        ),
    ]);
    for (name, value) in [
        (
            EVIDENCE_META_OBSERVATION_ID,
            binding.observation_id.as_ref(),
        ),
        (
            EVIDENCE_META_MAPPING_VERSION,
            binding.mapping_version.as_ref(),
        ),
    ] {
        if let Some(value) = value {
            metadata.insert(name.to_string(), value.clone());
        }
    }
    for (name, value) in [
        (EVIDENCE_META_SOURCE_GENERATION, binding.source_generation),
        (EVIDENCE_META_TARGET_GENERATION, binding.target_generation),
    ] {
        if let Some(value) = value {
            metadata.insert(name.to_string(), value.to_string());
        }
    }
    metadata
}

/// The project id embedded in a canonical ref, when it has one.
pub fn entity_project_scope(entity: &EntityRef) -> Option<&str> {
    match entity {
        EntityRef::ProjectGraphVertex { project_id, .. }
        | EntityRef::ProjectFile { project_id, .. }
        | EntityRef::ProjectFileV2 { project_id, .. }
        | EntityRef::Symbol { project_id, .. }
        | EntityRef::SymbolV2 { project_id, .. } => Some(project_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_endpoint(graph_id: &str, vertex_id: &str) -> EvidenceEndpointV1 {
        EvidenceEndpointV1::GraphVertex {
            graph_id: graph_id.into(),
            vertex_id: vertex_id.into(),
        }
    }

    fn file_endpoint() -> EvidenceEndpointV1 {
        EvidenceEndpointV1::ProjectFile {
            rel_path_hash: "pathhash".into(),
            chunk_hash: "chunkhash".into(),
            occurrence_idx: 0,
        }
    }

    fn binding(
        binding_id: &str,
        source: EvidenceEndpointV1,
        target: EvidenceEndpointV1,
        authority: EvidenceAssertionAuthority,
    ) -> EvidenceBindingV1 {
        EvidenceBindingV1 {
            binding_id: binding_id.into(),
            source,
            kind: "record:CORRESPONDS_TO".into(),
            target,
            assertion_authority: authority,
            observation_id: match authority {
                EvidenceAssertionAuthority::Connector => Some("obs-1".into()),
                _ => None,
            },
            mapping_version: match authority {
                EvidenceAssertionAuthority::Connector => None,
                _ => Some("mapping-v1".into()),
            },
            asserted_at: "2026-01-01T00:00:00Z".into(),
            source_generation: Some(1),
            target_generation: None,
        }
    }

    fn document(bindings: Vec<EvidenceBindingV1>) -> EvidenceDocumentV1 {
        EvidenceDocumentV1 {
            version: EVIDENCE_DOCUMENT_VERSION,
            bindings,
        }
    }

    /// Contract: bindings use canonical EntityRef endpoints, materialized with
    /// the project id from the lane rather than from committed bytes.
    #[test]
    fn typed_endpoints_resolve_to_canonical_refs_under_the_lane_project_id() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                file_endpoint(),
                EvidenceAssertionAuthority::Project,
            )]),
            EvidenceParseLimits::default(),
        );
        let set = load.bindings.expect("valid document resolves");
        let resolved = set.iter().next().unwrap();
        assert_eq!(
            resolved.source,
            EntityRef::ProjectGraphVertex {
                project_id: "proj-a".into(),
                graph_id: "records".into(),
                vertex_id: "record-1".into(),
            }
        );
        assert_eq!(
            resolved.target,
            EntityRef::ProjectFile {
                project_id: "proj-a".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: "chunkhash".into(),
                occurrence_idx: 0,
            }
        );
        // The same committed bytes on another host resolve to that host's ids.
        let other = resolve_evidence_document(
            "proj-b",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                file_endpoint(),
                EvidenceAssertionAuthority::Project,
            )]),
            EvidenceParseLimits::default(),
        );
        let other = other.bindings.unwrap();
        assert_eq!(
            entity_project_scope(&other.iter().next().unwrap().source),
            Some("proj-b")
        );
    }

    /// Contract: a knowledge entry is a legal evidence endpoint.
    #[test]
    fn knowledge_entries_are_legal_evidence_endpoints() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                EvidenceEndpointV1::Knowledge {
                    id: "abc12345".into(),
                },
                EvidenceAssertionAuthority::Operator,
            )]),
            EvidenceParseLimits::default(),
        );
        let set = load.bindings.expect("knowledge endpoint resolves");
        assert_eq!(
            set.iter().next().unwrap().target,
            EntityRef::Knowledge {
                id: "abc12345".into()
            }
        );
    }

    /// A literal ref endpoint may name project-independent types only; a
    /// project-scoped literal would bake a host-assigned id into the repo.
    #[test]
    fn literal_ref_endpoints_refuse_project_scoped_types() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                EvidenceEndpointV1::Ref {
                    entity_ref: "project_graph_vertex:proj-a:source:asset-1".into(),
                },
                EvidenceAssertionAuthority::Operator,
            )]),
            EvidenceParseLimits::default(),
        );
        assert!(load.bindings.is_none());
        assert!(
            load.errors
                .iter()
                .any(|error| error.code == "evidence.project_scoped_literal_ref")
        );
    }

    #[test]
    fn literal_ref_endpoints_accept_project_independent_types() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                EvidenceEndpointV1::Ref {
                    entity_ref: "thread:1234abcd".into(),
                },
                EvidenceAssertionAuthority::Operator,
            )]),
            EvidenceParseLimits::default(),
        );
        assert!(load.valid(), "{:?}", load.errors);
    }

    /// Contract: each binding records assertion authority and observation or
    /// mapping provenance.
    #[test]
    fn connector_assertions_require_observation_provenance() {
        let mut candidate = binding(
            "b1",
            graph_endpoint("records", "record-1"),
            file_endpoint(),
            EvidenceAssertionAuthority::Connector,
        );
        candidate.observation_id = None;
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![candidate]),
            EvidenceParseLimits::default(),
        );
        assert!(load.bindings.is_none());
        assert!(
            load.errors
                .iter()
                .any(|error| error.code == "evidence.connector_observation_required")
        );
    }

    #[test]
    fn project_and_operator_assertions_require_mapping_version() {
        for authority in [
            EvidenceAssertionAuthority::Project,
            EvidenceAssertionAuthority::Operator,
        ] {
            let mut candidate = binding(
                "b1",
                graph_endpoint("records", "record-1"),
                file_endpoint(),
                authority,
            );
            candidate.mapping_version = None;
            let load = resolve_evidence_document(
                "proj-a",
                &document(vec![candidate]),
                EvidenceParseLimits::default(),
            );
            assert!(
                load.errors
                    .iter()
                    .any(|error| error.code == "evidence.mapping_version_required"),
                "{authority:?} should require mapping_version"
            );
        }
    }

    #[test]
    fn duplicate_binding_ids_are_refused() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![
                binding(
                    "b1",
                    graph_endpoint("records", "record-1"),
                    file_endpoint(),
                    EvidenceAssertionAuthority::Project,
                ),
                binding(
                    "b1",
                    graph_endpoint("records", "record-2"),
                    file_endpoint(),
                    EvidenceAssertionAuthority::Project,
                ),
            ]),
            EvidenceParseLimits::default(),
        );
        assert!(
            load.errors
                .iter()
                .any(|error| error.code == "evidence.duplicate_binding_id")
        );
    }

    /// Contract: no connector-specific entity variants. An evidence binding
    /// must anchor on the graph layer rather than becoming a general edge
    /// store over arbitrary corpus entities.
    #[test]
    fn a_binding_with_no_graph_endpoint_is_refused() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                file_endpoint(),
                EvidenceEndpointV1::Knowledge {
                    id: "abc12345".into(),
                },
                EvidenceAssertionAuthority::Operator,
            )]),
            EvidenceParseLimits::default(),
        );
        assert!(
            load.errors
                .iter()
                .any(|error| error.code == "evidence.graph_endpoint_required")
        );
    }

    #[test]
    fn self_edges_are_refused() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                graph_endpoint("records", "record-1"),
                EvidenceAssertionAuthority::Project,
            )]),
            EvidenceParseLimits::default(),
        );
        assert!(
            load.errors
                .iter()
                .any(|error| error.code == "evidence.self_edge")
        );
    }

    /// Contract: an invalid candidate leaves the prior accepted set intact.
    /// The kernel expresses that by refusing to hand back ANY set.
    #[test]
    fn an_invalid_candidate_yields_no_replacement_set() {
        let load = parse_evidence_document("proj-a", b"{ not json", EvidenceParseLimits::default());
        assert!(load.bindings.is_none());
        assert!(!load.valid());
        assert_eq!(load.errors[0].code, "evidence.unparseable_document");
    }

    /// Contract: an unsupported document version is a rejection, not a
    /// best-effort partial read.
    #[test]
    fn an_unsupported_document_version_is_refused() {
        let load = resolve_evidence_document(
            "proj-a",
            &EvidenceDocumentV1 {
                version: EVIDENCE_DOCUMENT_VERSION + 1,
                bindings: Vec::new(),
            },
            EvidenceParseLimits::default(),
        );
        assert!(load.bindings.is_none());
        assert_eq!(load.errors[0].code, "evidence.unsupported_version");
    }

    /// An absent lane reads as an empty accepted set, never as an error.
    #[test]
    fn an_absent_lane_reads_as_an_empty_accepted_set() {
        let load = EvidenceLoad::absent();
        assert!(load.valid());
        assert!(load.bindings.unwrap().is_empty());
    }

    #[test]
    fn an_empty_document_is_a_valid_empty_set() {
        let load = parse_evidence_document(
            "proj-a",
            br#"{"version":1}"#,
            EvidenceParseLimits::default(),
        );
        assert!(load.bindings.expect("empty document is valid").is_empty());
    }

    #[test]
    fn forward_and_reverse_indexes_find_both_endpoints() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                file_endpoint(),
                EvidenceAssertionAuthority::Project,
            )]),
            EvidenceParseLimits::default(),
        );
        let set = load.bindings.unwrap();
        let record = EntityRef::ProjectGraphVertex {
            project_id: "proj-a".into(),
            graph_id: "records".into(),
            vertex_id: "record-1".into(),
        };
        let file = EntityRef::ProjectFile {
            project_id: "proj-a".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "chunkhash".into(),
            occurrence_idx: 0,
        };
        assert_eq!(set.forward(&record).len(), 1);
        assert_eq!(set.reverse(&file).len(), 1);
        assert!(set.reverse(&record).is_empty());
        assert!(set.forward(&file).is_empty());
        assert_eq!(set.endpoints().len(), 2);
    }

    /// Contract: endpoint status resolves as current, stale, missing,
    /// unauthorized, or unresolved.
    #[test]
    fn endpoint_status_resolves_every_contract_state() {
        use EvidenceEndpointObservation as Observed;
        assert_eq!(
            resolve_endpoint_status(
                Observed::Present {
                    generation: Some(1)
                },
                Some(1)
            ),
            EvidenceEndpointStatus::Current
        );
        assert_eq!(
            resolve_endpoint_status(
                Observed::Present {
                    generation: Some(2)
                },
                Some(1)
            ),
            EvidenceEndpointStatus::Stale
        );
        assert_eq!(
            resolve_endpoint_status(Observed::Present { generation: None }, None),
            EvidenceEndpointStatus::Current
        );
        assert_eq!(
            resolve_endpoint_status(Observed::Absent, Some(1)),
            EvidenceEndpointStatus::Stale
        );
        assert_eq!(
            resolve_endpoint_status(Observed::Absent, None),
            EvidenceEndpointStatus::Missing
        );
        assert_eq!(
            resolve_endpoint_status(Observed::OutOfScope, Some(1)),
            EvidenceEndpointStatus::Unauthorized
        );
        assert_eq!(
            resolve_endpoint_status(Observed::Unresolvable, None),
            EvidenceEndpointStatus::Unresolved
        );
    }

    /// Contract: connector reprojection changes freshness but cannot delete a
    /// tenant-owned binding. The binding set is untouched by a generation
    /// bump; only the scored status moves.
    #[test]
    fn reprojection_moves_status_without_dropping_the_binding() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                file_endpoint(),
                EvidenceAssertionAuthority::Project,
            )]),
            EvidenceParseLimits::default(),
        );
        let set = load.bindings.unwrap();
        let held = set.iter().next().unwrap();
        assert_eq!(set.len(), 1);
        // Same binding, endpoint reprojected to generation 2.
        let before = resolve_endpoint_status(
            EvidenceEndpointObservation::Present {
                generation: Some(1),
            },
            held.source_generation,
        );
        let after = resolve_endpoint_status(
            EvidenceEndpointObservation::Present {
                generation: Some(2),
            },
            held.source_generation,
        );
        assert_eq!(before, EvidenceEndpointStatus::Current);
        assert_eq!(after, EvidenceEndpointStatus::Stale);
        // Endpoint deleted outright: still stale, still held.
        let deleted =
            resolve_endpoint_status(EvidenceEndpointObservation::Absent, held.source_generation);
        assert_eq!(deleted, EvidenceEndpointStatus::Stale);
        assert_eq!(set.len(), 1);
    }

    /// Contract: path traversal never crosses an unauthorized endpoint, while
    /// inspection retains non-current bindings for diagnosis.
    #[test]
    fn only_unauthorized_endpoints_refuse_traversal() {
        assert!(EvidenceEndpointStatus::Current.traversable());
        assert!(EvidenceEndpointStatus::Stale.traversable());
        assert!(EvidenceEndpointStatus::Missing.traversable());
        assert!(EvidenceEndpointStatus::Unresolved.traversable());
        assert!(!EvidenceEndpointStatus::Unauthorized.traversable());
    }

    #[test]
    fn aggregate_status_is_worst_first() {
        use EvidenceEndpointStatus as Status;
        assert_eq!(
            aggregate_endpoint_status(Status::Current, Status::Current),
            Status::Current
        );
        assert_eq!(
            aggregate_endpoint_status(Status::Current, Status::Unresolved),
            Status::Unresolved
        );
        assert_eq!(
            aggregate_endpoint_status(Status::Missing, Status::Unresolved),
            Status::Missing
        );
        assert_eq!(
            aggregate_endpoint_status(Status::Missing, Status::Stale),
            Status::Stale
        );
        assert_eq!(
            aggregate_endpoint_status(Status::Stale, Status::Unauthorized),
            Status::Unauthorized
        );
    }

    #[test]
    fn binding_metadata_carries_authority_provenance_and_both_statuses() {
        let load = resolve_evidence_document(
            "proj-a",
            &document(vec![binding(
                "b1",
                graph_endpoint("records", "record-1"),
                file_endpoint(),
                EvidenceAssertionAuthority::Project,
            )]),
            EvidenceParseLimits::default(),
        );
        let set = load.bindings.unwrap();
        let metadata = binding_metadata(
            set.iter().next().unwrap(),
            EvidenceEndpointStatus::Current,
            EvidenceEndpointStatus::Missing,
        );
        assert_eq!(metadata[EVIDENCE_META_BINDING_ID], "b1");
        assert_eq!(metadata[EVIDENCE_META_PROJECT_ID], "proj-a");
        assert_eq!(metadata[EVIDENCE_META_ASSERTION_AUTHORITY], "project");
        assert_eq!(metadata[EVIDENCE_META_ASSERTED_AT], "2026-01-01T00:00:00Z");
        assert_eq!(metadata[EVIDENCE_META_MAPPING_VERSION], "mapping-v1");
        assert_eq!(metadata[EVIDENCE_META_SOURCE_GENERATION], "1");
        assert!(!metadata.contains_key(EVIDENCE_META_TARGET_GENERATION));
        assert_eq!(metadata[EVIDENCE_META_SOURCE_STATUS], "current");
        assert_eq!(metadata[EVIDENCE_META_TARGET_STATUS], "missing");
        assert_eq!(metadata[EVIDENCE_META_FRESHNESS], "missing");
    }

    /// The document round-trips: what an author commits is what the loader
    /// reads back, so a reviewer diffing the file sees the accepted set.
    #[test]
    fn the_committed_document_round_trips_through_serde() {
        let original = document(vec![binding(
            "b1",
            graph_endpoint("records", "record-1"),
            file_endpoint(),
            EvidenceAssertionAuthority::Project,
        )]);
        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: EvidenceDocumentV1 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn unknown_document_fields_are_refused() {
        let load = parse_evidence_document(
            "proj-a",
            br#"{"version":1,"bindings":[],"surprise":true}"#,
            EvidenceParseLimits::default(),
        );
        assert!(load.bindings.is_none());
        assert_eq!(load.errors[0].code, "evidence.unparseable_document");
    }
}
