use std::cmp::Ordering;

use anyhow::Result;

use crate::artifacts::{ArtifactCatalog, ArtifactKind, ArtifactListParams};

use super::types::{AgentCostClass, AgentManifest, AgentProvenance};

// ---------------------------------------------------------------------------
// ListFilter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub include_superseded: bool,
    pub cost_class: Option<AgentCostClass>,
    pub provenance_kind: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub cost_class: Option<AgentCostClass>,
    pub provenance_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentSearchResult {
    pub name: String,
    pub version: String,
    pub score: f64,
    pub description: String,
    pub when_to_use: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub cost_class: Option<AgentCostClass>,
    pub provenance_kind: Option<String>,
    pub matched_anti_patterns: Vec<String>,
}

// ---------------------------------------------------------------------------
// AgentSummary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub name: String,
    pub version: String,
    pub active: bool,
    pub description: Option<String>,
    pub cost_class: Option<AgentCostClass>,
    pub provenance_kind: Option<String>,
    pub installed_at: String,
    pub supersedes_chain: Vec<String>,
    pub embedding_pending: Option<bool>,
}

// ---------------------------------------------------------------------------
// AgentRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub name: String,
    pub version: String,
    pub active: bool,
    pub installed_at: String,
    pub source: String,
    pub metadata: ArtifactRecordMeta,
    pub manifest: Option<AgentManifest>,
    pub manifest_parse_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ArtifactRecordMeta {
    pub supersedes: Option<String>,
    pub supersedes_chain: Vec<String>,
    pub superseded_by: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentRegistry — read-only projection over the artifact catalog
// ---------------------------------------------------------------------------

/// Read-only projection over `ArtifactCatalog` for agent artifacts.
///
/// **Multi-version limitation:** `ArtifactCatalog` stores one artifact blob
/// and one metadata file per `(kind, name)` pair. Installing a new version
/// overwrites the previous blob on disk, so pinned refs like `name@v1`
/// cannot retrieve the old version after v2 overwrites it. The registry
/// correctly parses pinned refs and queries by version string, but the
/// catalog will only ever have the latest version's data. Historical
/// multi-version support requires a catalog redesign (out of scope for
/// the current agent-system phase).
pub struct AgentRegistry<'a> {
    catalog: &'a ArtifactCatalog,
}

impl<'a> AgentRegistry<'a> {
    pub fn new(catalog: &'a ArtifactCatalog) -> Self {
        Self { catalog }
    }

    pub fn list(&self, filter: &ListFilter) -> Result<Vec<AgentSummary>> {
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: None,
            include_superseded: filter.include_superseded,
        };
        let entries = self.catalog.list(&params)?;
        let mut out = Vec::new();
        for entry in entries {
            let (manifest, parse_err) = self.load_manifest_degraded(&entry.name);
            let cost_class = manifest.as_ref().map(|m| m.cost_class);
            let provenance_kind = manifest.as_ref().and_then(|m| {
                m.provenance.as_ref().map(|p| match p {
                    AgentProvenance::HandAuthored { .. } => "hand_authored".to_string(),
                    AgentProvenance::Distilled { .. } => "distilled".to_string(),
                    AgentProvenance::Imported { .. } => "imported".to_string(),
                })
            });
            let description = manifest
                .as_ref()
                .map(|m| m.description.clone())
                .or(entry.description.clone());
            if let Some(ref wanted) = filter.cost_class {
                if cost_class != Some(*wanted) {
                    continue;
                }
            }
            if let Some(ref wanted_kind) = filter.provenance_kind {
                if provenance_kind.as_deref() != Some(wanted_kind) {
                    continue;
                }
            }
            let embedding_pending = if parse_err.is_some() {
                None
            } else {
                Some(manifest.as_ref().is_some_and(|m| m.embedding.is_none()))
            };
            out.push(AgentSummary {
                name: entry.name,
                version: entry.version,
                active: entry.active,
                description,
                cost_class,
                provenance_kind,
                installed_at: entry.installed_at,
                supersedes_chain: entry.supersedes_chain,
                embedding_pending,
            });
        }
        Ok(out)
    }

    pub fn get(&self, name_or_ref: &str) -> Result<Option<AgentRecord>> {
        let (name, version_pin) = parse_name_or_ref(name_or_ref)?;
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: Some(name.clone()),
            include_superseded: version_pin.is_some(),
        };
        let entries = self.catalog.list(&params)?;
        let entry = match version_pin {
            Some(v) => entries.into_iter().find(|e| e.version == v),
            None => entries.into_iter().find(|e| e.active),
        };
        let entry = match entry {
            Some(e) => e,
            None => return Ok(None),
        };
        let (manifest, manifest_parse_error) = self.load_manifest_degraded(&entry.name);
        let supersedes = self
            .catalog
            .metadata_for(ArtifactKind::Agent, &entry.name)
            .ok()
            .flatten()
            .and_then(|m| m.supersedes);
        Ok(Some(AgentRecord {
            name: entry.name,
            version: entry.version,
            active: entry.active,
            installed_at: entry.installed_at,
            source: entry.source,
            metadata: ArtifactRecordMeta {
                supersedes,
                supersedes_chain: entry.supersedes_chain,
                superseded_by: entry.superseded_by,
            },
            manifest,
            manifest_parse_error,
        }))
    }

    fn load_manifest_degraded(&self, name: &str) -> (Option<AgentManifest>, Option<String>) {
        let value = match self.catalog.load_artifact_value(ArtifactKind::Agent, name) {
            Ok(Some(v)) => v,
            _ => return (None, None),
        };
        let manifest_value = value.get("manifest").unwrap_or(&value);
        match serde_json::from_value(manifest_value.clone()) {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e.to_string())),
        }
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
        filter: &SearchFilter,
        exclude_anti_pattern_matches: bool,
    ) -> Result<Vec<AgentSearchResult>> {
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: None,
            include_superseded: false,
        };
        let entries = self.catalog.list(&params)?;
        let query_lower = query.to_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut candidates: Vec<AgentSearchResult> = Vec::new();

        for entry in entries {
            if !entry.active {
                continue;
            }
            let (manifest, _) = self.load_manifest_degraded(&entry.name);
            let Some(manifest) = manifest else {
                continue;
            };

            let cost_class = manifest.cost_class;
            let provenance_kind = manifest.provenance.as_ref().map(|p| match p {
                AgentProvenance::HandAuthored { .. } => "hand_authored",
                AgentProvenance::Distilled { .. } => "distilled",
                AgentProvenance::Imported { .. } => "imported",
            });
            if let Some(ref wanted) = filter.cost_class {
                if cost_class != *wanted {
                    continue;
                }
            }
            if let Some(ref wanted_kind) = filter.provenance_kind {
                if provenance_kind != Some(wanted_kind.as_str()) {
                    continue;
                }
            }

            let desc_lower = manifest.description.to_lowercase();
            let wtu_lower: Vec<String> =
                manifest.when_to_use.iter().map(|s| s.to_lowercase()).collect();
            let ap_lower: Vec<String> =
                manifest.anti_patterns.iter().map(|s| s.to_lowercase()).collect();

            let positive_score = text_relevance(&query_terms, &desc_lower, &wtu_lower);
            let anti_score = text_relevance(&query_terms, "", &ap_lower);

            let mut matched_anti_patterns = Vec::new();
            for (i, ap) in manifest.anti_patterns.iter().enumerate() {
                let ap_l = ap.to_lowercase();
                if query_terms.iter().any(|t| ap_l.contains(t)) {
                    matched_anti_patterns.push(ap.clone());
                }
                let _ = i;
            }

            if positive_score == 0.0 {
                continue;
            }

            if exclude_anti_pattern_matches && anti_score > positive_score {
                continue;
            }

            let penalty = if anti_score > positive_score {
                0.3 * anti_score
            } else {
                0.0
            };
            let final_score = (positive_score - penalty).max(0.0);

            if final_score <= 0.0 {
                continue;
            }

            candidates.push(AgentSearchResult {
                name: entry.name,
                version: entry.version,
                score: final_score,
                description: manifest.description,
                when_to_use: manifest.when_to_use,
                anti_patterns: manifest.anti_patterns,
                cost_class: Some(cost_class),
                provenance_kind: provenance_kind.map(String::from),
                matched_anti_patterns,
            });
        }

        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    cost_rank(a.cost_class).cmp(&cost_rank(b.cost_class))
                })
        });
        candidates.truncate(limit);

        Ok(candidates)
    }
}

/// Parse an agent name or versioned ref into `(name, optional_version)`.
///
/// Returns an error for structurally invalid refs:
/// - empty input
/// - `agent:` with no name
/// - `@v2` with no name
/// - `name@v` with empty version
/// - `name@v0` (version zero)
/// - non-numeric version
pub fn parse_name_or_ref(input: &str) -> Result<(String, Option<String>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("agent ref must not be empty");
    }
    if let Some(rest) = trimmed.strip_prefix("agent:") {
        if rest.is_empty() {
            anyhow::bail!("agent ref 'agent:' requires a name after the prefix");
        }
        return parse_versioned(rest);
    }
    parse_versioned(trimmed)
}

fn parse_versioned(input: &str) -> Result<(String, Option<String>)> {
    if let Some((name, ver)) = input.rsplit_once("@v") {
        if name.is_empty() {
            anyhow::bail!("agent ref '@v{ver}' requires a name before @v");
        }
        if ver.is_empty() {
            anyhow::bail!("agent ref '{name}@v' requires a version after @v");
        }
        let v: u64 = ver.parse().map_err(|_| {
            anyhow::anyhow!("agent ref version must be a positive integer, got '{ver}'")
        })?;
        if v == 0 {
            anyhow::bail!("agent ref version must be positive, got 0");
        }
        Ok((name.to_string(), Some(ver.to_string())))
    } else {
        Ok((input.to_string(), None))
    }
}

// ---------------------------------------------------------------------------
// Text relevance scoring (BM25-like simplified)
// ---------------------------------------------------------------------------

fn text_relevance(query_terms: &[&str], description: &str, lines: &[String]) -> f64 {
    let all_text: String = {
        let mut s = description.to_string();
        for line in lines {
            s.push(' ');
            s.push_str(line);
        }
        s.to_lowercase()
    };
    let mut score = 0.0f64;
    for term in query_terms {
        let count = all_text.matches(term).count();
        if count > 0 {
            score += 1.0 + (count as f64).ln_1p();
        }
    }
    score
}

fn cost_rank(cc: Option<AgentCostClass>) -> u8 {
    match cc {
        Some(AgentCostClass::Cheap) => 0,
        Some(AgentCostClass::Normal) => 1,
        Some(AgentCostClass::Expensive) => 2,
        None => 3,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_catalog(dir: &tempfile::TempDir) -> ArtifactCatalog {
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "reviewer.json".into(),
                &serde_json::json!({
                    "name": "reviewer",
                    "version": 2,
                    "supersedes": "reviewer",
                    "manifest": {
                        "description": "Improved code review agent.",
                        "when_to_use": ["after code changes", "on PR"],
                        "brofile_inline": {"provider": "claude"},
                        "cost_class": "expensive",
                        "provenance": {"kind": "distilled", "distilled_by": "badgey-01", "evidence_session_ids": [], "created_from_threads": [], "accept_count": 3, "reject_count": 0}
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "test-writer.json".into(),
                &serde_json::json!({
                    "name": "test-writer",
                    "version": 1,
                    "manifest": {
                        "description": "Generates unit tests for code.",
                        "when_to_use": ["after writing code"],
                        "brofile_inline": {"provider": "claude"},
                        "cost_class": "cheap"
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();
        catalog
    }

    #[test]
    fn list_active_only() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"reviewer"));
        assert!(names.contains(&"test-writer"));
    }

    #[test]
    fn list_with_superseded_shows_all_when_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter {
                include_superseded: true,
                ..Default::default()
            })
            .unwrap();
        assert!(results.len() >= 2);
        let reviewer = results.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(reviewer.version, "2");
        assert!(reviewer.active);
    }

    #[test]
    fn list_filter_by_cost_class() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter {
                cost_class: Some(AgentCostClass::Cheap),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "test-writer");
    }

    #[test]
    fn list_filter_by_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry
            .list(&ListFilter {
                provenance_kind: Some("distilled".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "reviewer");
    }

    #[test]
    fn get_by_bare_name_resolves_active() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("reviewer").unwrap().unwrap();
        assert_eq!(record.version, "2");
        assert!(record.active);
        assert!(record.manifest.is_some());
        assert_eq!(
            record.manifest.as_ref().unwrap().cost_class,
            AgentCostClass::Expensive
        );
    }

    #[test]
    fn get_by_pinned_version() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("reviewer@v2").unwrap().unwrap();
        assert_eq!(record.version, "2");
        assert!(record.active);
    }

    #[test]
    fn get_by_agent_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("agent:reviewer@v2").unwrap().unwrap();
        assert_eq!(record.version, "2");
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        assert!(registry.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn get_populates_supersedes_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("reviewer").unwrap().unwrap();
        assert_eq!(record.metadata.supersedes.as_deref(), Some("reviewer"));
    }

    #[test]
    fn summary_reads_manifest_description() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        let reviewer = results.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(
            reviewer.description.as_deref(),
            Some("Improved code review agent.")
        );
    }

    #[test]
    fn embedding_pending_when_no_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        assert!(results.iter().all(|r| r.embedding_pending == Some(true)));
    }

    #[test]
    fn embedding_pending_none_for_malformed_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "broken.json".into(),
                &serde_json::json!({
                    "name": "broken",
                    "version": 1,
                    "manifest": "not a valid manifest object"
                }),
                None,
                None,
                None,
            )
            .unwrap();
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        let broken = results.iter().find(|r| r.name == "broken").unwrap();
        assert_eq!(broken.embedding_pending, None);
    }

    #[test]
    fn manifest_parse_error_on_get() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "broken.json".into(),
                &serde_json::json!({
                    "name": "broken",
                    "version": 1,
                    "manifest": 42
                }),
                None,
                None,
                None,
            )
            .unwrap();
        let registry = AgentRegistry::new(&catalog);
        let record = registry.get("broken").unwrap().unwrap();
        assert!(record.manifest.is_none());
        assert!(record.manifest_parse_error.is_some());
    }

    #[test]
    fn parse_name_or_ref_bare() {
        let (name, ver) = parse_name_or_ref("reviewer").unwrap();
        assert_eq!(name, "reviewer");
        assert!(ver.is_none());
    }

    #[test]
    fn parse_name_or_ref_versioned() {
        let (name, ver) = parse_name_or_ref("reviewer@v2").unwrap();
        assert_eq!(name, "reviewer");
        assert_eq!(ver, Some("2".into()));
    }

    #[test]
    fn parse_name_or_ref_agent_prefix() {
        let (name, ver) = parse_name_or_ref("agent:reviewer@v1").unwrap();
        assert_eq!(name, "reviewer");
        assert_eq!(ver, Some("1".into()));
    }

    #[test]
    fn parse_name_or_ref_agent_prefix_without_version() {
        let (name, ver) = parse_name_or_ref("agent:reviewer").unwrap();
        assert_eq!(name, "reviewer");
        assert!(ver.is_none());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_name_or_ref("").is_err());
        assert!(parse_name_or_ref("  ").is_err());
    }

    #[test]
    fn parse_rejects_bare_agent_prefix() {
        let err = parse_name_or_ref("agent:").unwrap_err();
        assert!(err.to_string().contains("requires a name"));
    }

    #[test]
    fn parse_rejects_no_name_versioned() {
        let err = parse_name_or_ref("@v2").unwrap_err();
        assert!(err.to_string().contains("requires a name"));
    }

    #[test]
    fn parse_rejects_empty_version() {
        let err = parse_name_or_ref("reviewer@v").unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn parse_rejects_version_zero() {
        let err = parse_name_or_ref("reviewer@v0").unwrap_err();
        assert!(err.to_string().contains("positive"));
    }

    #[test]
    fn parse_rejects_non_numeric_version() {
        let err = parse_name_or_ref("reviewer@vabc").unwrap_err();
        assert!(err.to_string().contains("positive integer"));
    }

    #[test]
    fn text_relevance_basic_scoring() {
        let score = text_relevance(
            &["review", "code"],
            "reviews pull requests for code quality",
            &["when you need a review".to_string()],
        );
        assert!(score > 0.0, "should have positive score: {score}");
        let zero = text_relevance(&["xyz"], "unrelated text", &[]);
        assert_eq!(zero, 0.0);
    }

    #[test]
    fn text_relevance_multi_occurrence_boosts() {
        let one = text_relevance(&["agent"], "agent", &[]);
        let two = text_relevance(&["agent"], "agent agent", &[]);
        assert!(two > one, "more occurrences should score higher: {one} vs {two}");
    }

    #[test]
    fn search_penalizes_anti_pattern_dominance() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        catalog
            .install_value(
                ArtifactKind::Agent,
                "penalized.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "penalized",
                    "version": 1,
                    "manifest": {
                        "description": "Does things.",
                        "when_to_use": ["use sometimes"],
                        "anti_patterns": ["do not use for thing"],
                        "brofile_inline": {"provider": "claude"},
                    },
                }),
                None,
                None,
                None,
            )
            .unwrap();
        let reg = AgentRegistry::new(&catalog);
        let results = reg
            .search("thing", 10, &SearchFilter::default(), false)
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results[0].score < 2.0,
            "anti-pattern penalty should reduce score: {results:?}"
        );
        assert!(
            !results[0].matched_anti_patterns.is_empty(),
            "anti-pattern should be matched: {results:?}"
        );
    }

    #[test]
    fn search_cost_class_filter() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        catalog
            .install_value(
                ArtifactKind::Agent,
                "cheap.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "cheap-agent",
                    "version": 1,
                    "manifest": {
                        "description": "A cheap search agent.",
                        "when_to_use": ["when testing"],
                        "cost_class": "cheap",
                        "brofile_inline": {"provider": "claude"},
                    },
                }),
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "expensive.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "expensive-agent",
                    "version": 1,
                    "manifest": {
                        "description": "An expensive search agent.",
                        "when_to_use": ["when testing"],
                        "cost_class": "expensive",
                        "brofile_inline": {"provider": "claude"},
                    },
                }),
                None,
                None,
                None,
            )
            .unwrap();
        let reg = AgentRegistry::new(&catalog);
        let results = reg
            .search(
                "search agent",
                10,
                &SearchFilter {
                    cost_class: Some(AgentCostClass::Cheap),
                    provenance_kind: None,
                },
                true,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "cheap-agent");
    }
}
