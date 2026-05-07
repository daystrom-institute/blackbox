use std::cmp::Ordering;
use std::collections::BTreeMap;

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
    pub sources: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentVectorSearch {
    pub route: String,
    pub query_vector: Vec<f32>,
}

#[derive(Debug, Clone, Default)]
struct ComponentScores {
    primary: f64,
    when_to_use: f64,
    anti_patterns: f64,
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
    pub install_warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// AgentRegistry — read-only projection over the artifact catalog
// ---------------------------------------------------------------------------

/// Read-only projection over `ArtifactCatalog` for agent artifacts.
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
                Some(
                    manifest
                        .as_ref()
                        .is_none_or(|m| manifest_embedding_pending(&entry.name, &entry.version, m)),
                )
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
        if let Some(version) = version_pin {
            let Some(meta) =
                self.catalog
                    .metadata_for_version(ArtifactKind::Agent, &name, &version)?
            else {
                return Ok(None);
            };
            let (manifest, manifest_parse_error) =
                self.load_manifest_degraded_version(&name, &version);
            return Ok(Some(AgentRecord {
                name: meta.name,
                version: meta.version,
                active: meta.active,
                installed_at: meta.installed_at,
                source: meta.source,
                metadata: ArtifactRecordMeta {
                    supersedes: meta.supersedes,
                    supersedes_chain: meta.supersedes_chain,
                    superseded_by: meta.superseded_by,
                    install_warnings: meta.install_warnings,
                },
                manifest,
                manifest_parse_error,
            }));
        }
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: Some(name.clone()),
            include_superseded: false,
        };
        let entries = self.catalog.list(&params)?;
        let entry = entries.into_iter().find(|e| e.active);
        let entry = match entry {
            Some(e) => e,
            None => return Ok(None),
        };
        let (manifest, manifest_parse_error) = self.load_manifest_degraded(&entry.name);
        let metadata = self
            .catalog
            .metadata_for(ArtifactKind::Agent, &entry.name)
            .ok()
            .flatten();
        let supersedes = metadata.as_ref().and_then(|m| m.supersedes.clone());
        let install_warnings = metadata.map(|m| m.install_warnings).unwrap_or_default();
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
                install_warnings,
            },
            manifest,
            manifest_parse_error,
        }))
    }

    pub fn load_manifest_degraded(&self, name: &str) -> (Option<AgentManifest>, Option<String>) {
        let value = match self.catalog.load_artifact_value(ArtifactKind::Agent, name) {
            Ok(Some(v)) => v,
            _ => return (None, None),
        };
        load_manifest_degraded_value(value)
    }

    fn load_manifest_degraded_version(
        &self,
        name: &str,
        version: &str,
    ) -> (Option<AgentManifest>, Option<String>) {
        let value =
            match self
                .catalog
                .load_artifact_value_version(ArtifactKind::Agent, name, version)
            {
                Ok(Some(v)) => v,
                _ => return (None, None),
            };
        load_manifest_degraded_value(value)
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
        filter: &SearchFilter,
        exclude_anti_pattern_matches: bool,
    ) -> Result<Vec<AgentSearchResult>> {
        self.search_inner(query, limit, filter, exclude_anti_pattern_matches, None)
    }
}

fn load_manifest_degraded_value(
    value: serde_json::Value,
) -> (Option<AgentManifest>, Option<String>) {
    let manifest_value = value.get("manifest").unwrap_or(&value);
    match serde_json::from_value(manifest_value.clone()) {
        Ok(m) => (Some(m), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

impl<'a> AgentRegistry<'a> {
    pub fn search_with_vectors(
        &self,
        query: &str,
        limit: usize,
        filter: &SearchFilter,
        exclude_anti_pattern_matches: bool,
        vector: Option<&AgentVectorSearch>,
    ) -> Result<Vec<AgentSearchResult>> {
        self.search_inner(query, limit, filter, exclude_anti_pattern_matches, vector)
    }

    fn search_inner(
        &self,
        query: &str,
        limit: usize,
        filter: &SearchFilter,
        exclude_anti_pattern_matches: bool,
        vector: Option<&AgentVectorSearch>,
    ) -> Result<Vec<AgentSearchResult>> {
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Agent),
            name: None,
            include_superseded: false,
        };
        let entries = self.catalog.list(&params)?;
        let query_lower = query.to_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
        let vector_scores = vector
            .map(|v| agent_component_scores(&v.route, &v.query_vector, limit.max(10) * 16))
            .transpose()?
            .unwrap_or_default();

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
            let wtu_lower: Vec<String> = manifest
                .when_to_use
                .iter()
                .map(|s| s.to_lowercase())
                .collect();
            let ap_lower: Vec<String> = manifest
                .anti_patterns
                .iter()
                .map(|s| s.to_lowercase())
                .collect();

            let positive_score = text_relevance(&query_terms, &desc_lower, &wtu_lower);
            let anti_score = text_relevance(&query_terms, "", &ap_lower);
            let agent_ref = super::types::AgentRef {
                name: entry.name.clone(),
                version: entry.version.parse::<u32>().unwrap_or(1),
            };
            let component_scores = vector_scores
                .get(&agent_ref.render())
                .cloned()
                .unwrap_or_default();
            let vector_positive = component_scores.primary.max(component_scores.when_to_use);

            let mut matched_anti_patterns = Vec::new();
            for ap in &manifest.anti_patterns {
                let ap_tokens = tokenize_for_match(ap);
                let any_hit = query_terms.iter().any(|qt| {
                    let qt = qt.trim();
                    if qt.len() < 3 || STOPWORDS.contains(&qt) {
                        return false;
                    }
                    let qt_lc = qt.to_lowercase();
                    ap_tokens.iter().any(|t| token_match(&qt_lc, t))
                });
                if any_hit {
                    matched_anti_patterns.push(ap.clone());
                }
            }

            if positive_score == 0.0 && vector_positive == 0.0 {
                continue;
            }

            if exclude_anti_pattern_matches
                && (anti_score > positive_score && anti_score > vector_positive)
            {
                continue;
            }
            if exclude_anti_pattern_matches
                && component_scores.anti_patterns > vector_positive
                && component_scores.anti_patterns > positive_score
            {
                continue;
            }

            let penalty = if anti_score > positive_score {
                0.3 * anti_score
            } else {
                0.0
            } + if component_scores.anti_patterns > vector_positive {
                0.5 * component_scores.anti_patterns
            } else {
                0.0
            };
            let mut sources = BTreeMap::new();
            if positive_score > 0.0 {
                sources.insert("keyword".into(), positive_score);
            }
            if component_scores.primary > 0.0 {
                sources.insert("vector_primary".into(), component_scores.primary);
            }
            if component_scores.when_to_use > 0.0 {
                sources.insert("vector_when_to_use".into(), component_scores.when_to_use);
            }
            if component_scores.anti_patterns > 0.0 {
                sources.insert(
                    "vector_anti_patterns".into(),
                    component_scores.anti_patterns,
                );
            }
            let final_score = (positive_score
                + (2.0 * component_scores.primary)
                + (1.5 * component_scores.when_to_use)
                - penalty)
                .max(0.0);

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
                sources,
            });
        }

        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| cost_rank(a.cost_class).cmp(&cost_rank(b.cost_class)))
        });
        candidates.truncate(limit);

        Ok(candidates)
    }
}

fn manifest_embedding_pending(name: &str, version: &str, manifest: &AgentManifest) -> bool {
    let Some(embedding) = &manifest.embedding else {
        return true;
    };
    let Ok(version) = version.parse::<u32>() else {
        return true;
    };
    let route = embedding.vector_route.as_deref().unwrap_or("");
    if route.is_empty() {
        return true;
    }
    let agent = super::types::AgentRef {
        name: name.to_string(),
        version,
    };
    let when_to_use_ref = if manifest.when_to_use.is_empty() {
        None
    } else {
        match embedding.components.when_to_use.as_deref() {
            Some(vector_ref) => Some(vector_ref),
            None => return true,
        }
    };
    let anti_patterns_ref = if manifest.anti_patterns.is_empty() {
        None
    } else {
        match embedding.components.anti_patterns.as_deref() {
            Some(vector_ref) => Some(vector_ref),
            None => return true,
        }
    };
    let checks = [
        (
            crate::embed_queue::AgentManifestComponent::Primary,
            Some(embedding.components.primary.as_str()),
        ),
        (
            crate::embed_queue::AgentManifestComponent::WhenToUse,
            when_to_use_ref,
        ),
        (
            crate::embed_queue::AgentManifestComponent::AntiPatterns,
            anti_patterns_ref,
        ),
    ];
    for (component, vector_ref) in checks {
        let Some(vector_ref) = vector_ref else {
            continue;
        };
        let expected = crate::embed_queue::agent_component_entity_id(&agent, component);
        if vector_ref != expected {
            return true;
        }
        let Some(hash) = crate::embed_queue::agent_component_hash(manifest, component) else {
            continue;
        };
        match crate::vectors::contains_active(route, vector_ref, &hash) {
            Ok(true) => {}
            _ => return true,
        }
    }
    false
}

fn agent_component_scores(
    vector_route: &str,
    query_vector: &[f32],
    fetch: usize,
) -> Result<BTreeMap<String, ComponentScores>> {
    let hits = crate::vectors::search(vector_route, query_vector, fetch)?;
    let mut out = BTreeMap::<String, ComponentScores>::new();
    for hit in hits {
        let Some((agent, component)) = crate::embed_queue::parse_agent_component_entity_id(&hit.id)
        else {
            continue;
        };
        let score = (1.0 - hit.distance).max(0.0) as f64;
        let entry = out.entry(agent.render()).or_default();
        match component {
            crate::embed_queue::AgentManifestComponent::Primary => {
                entry.primary = entry.primary.max(score);
            }
            crate::embed_queue::AgentManifestComponent::WhenToUse => {
                entry.when_to_use = entry.when_to_use.max(score);
            }
            crate::embed_queue::AgentManifestComponent::AntiPatterns => {
                entry.anti_patterns = entry.anti_patterns.max(score);
            }
        }
    }
    Ok(out)
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

/// Common short English tokens that contribute pure noise to substring/word
/// matching. Kept tight (high-frequency function words only) so domain
/// terms still match. Length<3 tokens are filtered separately.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "are", "but", "with", "that", "this", "from", "into", "your",
    "they", "them", "their", "there", "when", "what", "which", "while", "would", "could", "should",
    "have", "has", "had", "was", "were", "been", "being", "will", "shall", "can", "may", "not",
    "use", "using", "use's", "yes", "no", "off", "any", "all", "some", "more", "most", "much",
    "many", "few", "such", "very", "just", "only", "than", "then", "now", "how", "why", "where",
    "who", "whom", "whose", "its", "his", "her", "him", "she", "out", "our", "ours", "owns", "own",
    "via", "per",
];

fn token_match(query_term: &str, target_token: &str) -> bool {
    if query_term == target_token {
        return true;
    }
    if query_term.len().min(target_token.len()) < 4 {
        return false;
    }
    // Stem-prefix match: the two tokens are morphologically related
    // when they share a long common prefix and neither side extends
    // beyond that prefix by more than 3 chars (typical English
    // suffixes: -s, -es, -ed, -ing, -ion, -ate, -ies, etc.). Catches
    // navigate/navigating, function/functional, rust/rusts; rejects
    // agent/agencies (LCP too short) and code/consider (suffix gap
    // too large).
    let q = query_term.as_bytes();
    let t = target_token.as_bytes();
    let lcp = q.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();
    if lcp < 4 {
        return false;
    }
    let max_len = q.len().max(t.len());
    max_len - lcp <= 3
}

fn tokenize_for_match(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|tok| {
            if tok.len() < 3 {
                return None;
            }
            if STOPWORDS.contains(&tok) {
                return None;
            }
            Some(tok.to_string())
        })
        .collect()
}

fn text_relevance(query_terms: &[&str], description: &str, lines: &[String]) -> f64 {
    let mut all_text = String::from(description);
    for line in lines {
        all_text.push(' ');
        all_text.push_str(line);
    }
    let target_tokens = tokenize_for_match(&all_text);
    if target_tokens.is_empty() {
        return 0.0;
    }
    let mut score = 0.0f64;
    for term in query_terms {
        // Filter out trivial query terms with the same rules as target
        // tokenization so substring noise (e.g. single-letter "a"
        // matching "agentic", or "me" matching "teach-mode") does not
        // inflate positive_score for unrelated agents.
        let term = term.trim();
        if term.len() < 3 {
            continue;
        }
        if STOPWORDS.contains(&term) {
            continue;
        }
        let term_lc = term.to_lowercase();
        // Loose stem-prefix match: a query term hits an agent token if
        // either is a prefix of the other and the shared prefix is at
        // least 4 chars. Catches navigate/navigating/navigates without
        // letting 3-char query tokens wildcard-match across the
        // dictionary.
        let count = target_tokens
            .iter()
            .filter(|t| token_match(&term_lc, t))
            .count();
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
    use super::super::types::{AgentEmbedding, AgentEmbeddingComponents};
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
    fn get_by_pinned_version_reads_historical_snapshot_after_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "reviewer-v1.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "reviewer",
                    "version": 1,
                    "manifest": {
                        "description": "First review agent.",
                        "when_to_use": ["before the upgrade"],
                        "brofile_inline": {"provider": "claude"}
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
                "reviewer-v2.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "reviewer",
                    "version": 2,
                    "supersedes": "reviewer",
                    "manifest": {
                        "description": "Second review agent.",
                        "when_to_use": ["after the upgrade"],
                        "brofile_inline": {"provider": "claude"}
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();

        let registry = AgentRegistry::new(&catalog);
        let current = registry.get("reviewer").unwrap().unwrap();
        assert_eq!(current.version, "2");
        assert_eq!(
            current.manifest.as_ref().unwrap().description,
            "Second review agent."
        );

        let pinned = registry.get("agent:reviewer@v1").unwrap().unwrap();
        assert_eq!(pinned.version, "1");
        assert!(!pinned.active);
        assert_eq!(
            pinned.manifest.as_ref().unwrap().description,
            "First review agent."
        );
        assert_eq!(pinned.metadata.superseded_by.as_deref(), Some("reviewer"));

        let history = registry
            .list(&ListFilter {
                include_superseded: true,
                ..Default::default()
            })
            .unwrap();
        let versions = history
            .iter()
            .filter(|row| row.name == "reviewer")
            .map(|row| (row.version.as_str(), row.active))
            .collect::<Vec<_>>();
        assert!(
            versions.contains(&("1", false)) && versions.contains(&("2", true)),
            "include_superseded should surface same-name history: {versions:?}"
        );
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
    fn legacy_embedding_shape_stays_parseable_and_pending() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "legacy.json".into(),
                &serde_json::json!({
                    "name": "legacy",
                    "version": 1,
                    "manifest": {
                        "description": "Legacy embedded agent manifest.",
                        "when_to_use": ["when checking compatibility"],
                        "brofile_inline": {"provider": "claude"},
                        "embedding": {
                            "model": "old-model",
                            "computed_at": "2026-01-01T00:00:00Z",
                            "vector_ref": "agent:legacy@v1"
                        }
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();
        let registry = AgentRegistry::new(&catalog);
        let results = registry.list(&ListFilter::default()).unwrap();
        let legacy = results.iter().find(|r| r.name == "legacy").unwrap();
        assert_eq!(legacy.embedding_pending, Some(true));
        assert!(registry.get("legacy").unwrap().unwrap().manifest.is_some());
    }

    #[test]
    fn anti_pattern_manifest_requires_anti_pattern_component_ref() {
        let manifest = AgentManifest {
            description: "Agent with anti pattern embeddings.".into(),
            when_to_use: vec!["when checking component coverage".into()],
            anti_patterns: vec!["when not to use it".into()],
            brofile_inline: Some(serde_json::json!({"provider": "claude"})),
            embedding: Some(AgentEmbedding {
                model: "test-model".into(),
                computed_at: "2026-01-01T00:00:00Z".into(),
                vector_ref: "agent_embed:component-test:v1:primary".into(),
                vector_route: Some("test-route".into()),
                components: AgentEmbeddingComponents {
                    primary: "agent_embed:component-test:v1:primary".into(),
                    when_to_use: Some("agent_embed:component-test:v1:when_to_use".into()),
                    anti_patterns: None,
                },
            }),
            ..Default::default()
        };
        assert!(manifest_embedding_pending("component-test", "1", &manifest));
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
    fn matched_anti_patterns_uses_filtered_tokens() {
        // Earlier reporting used raw split_whitespace query tokens
        // and a substring contains() check, so any anti-pattern
        // containing common short words like "use" or "of" looked
        // matched against any query — including queries that
        // shared zero domain tokens with the anti-pattern. The
        // filter logic itself was correct (it used the tokenized
        // anti_score), but the surfaced matched_anti_patterns list
        // was noise. Use the same tokenization/stem-prefix rules so
        // the surfaced list reflects real matches.
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        catalog
            .install_value(
                ArtifactKind::Agent,
                "match-test.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "match-test",
                    "version": 1,
                    "manifest": {
                        "description": "test agent for matched_anti_patterns reporting",
                        "when_to_use": ["when running the matched-AP unit test"],
                        "anti_patterns": [
                            "do not use for one-off code generation tasks",
                            "do not auto-apply proposals without explicit user approval"
                        ],
                        "brofile_inline": {"provider": "claude"},
                    },
                }),
                None,
                None,
                None,
            )
            .unwrap();
        let registry = AgentRegistry::new(&catalog);

        // Query that scores positive against the agent (so it isn't
        // bailed out by the early "no signal" filter) but shares no
        // domain tokens with either anti-pattern. Earlier behavior:
        // matched_anti_patterns surfaced both APs anyway because
        // common short words like "use" / "of" inside the AP text
        // substring-matched any query. Filtered tokenization should
        // surface neither.
        let results = registry
            .search(
                "running the unit test on this reporting agent",
                5,
                &SearchFilter::default(),
                false,
            )
            .unwrap();
        let row = results.iter().find(|r| r.name == "match-test").unwrap();
        assert!(
            row.matched_anti_patterns.is_empty(),
            "expected no AP matches for query with no domain overlap, got: {:?}",
            row.matched_anti_patterns
        );

        // Query that hits a real domain token in AP1 must surface that AP only.
        let results = registry
            .search(
                "write code generation reporting agent",
                5,
                &SearchFilter::default(),
                false,
            )
            .unwrap();
        let row = results.iter().find(|r| r.name == "match-test").unwrap();
        assert_eq!(
            row.matched_anti_patterns,
            vec!["do not use for one-off code generation tasks".to_string()],
            "expected only the code-generation AP, got: {:?}",
            row.matched_anti_patterns
        );
    }

    #[test]
    fn text_relevance_filters_short_and_stop_tokens() {
        // Earlier behavior: substring scoring with no tokenization let
        // single-letter "a" and stopword "me" inflate scores by
        // matching inside unrelated words ("agentic", "teach-mode").
        // A query that shares no domain-relevant tokens with the agent
        // text must score zero.
        let score = text_relevance(
            &["write", "me", "a", "rust", "function"],
            "long-lived agentic-corpus consultant that proposes durable artifacts",
            &["when a caller needs help navigating the agentic corpus".into()],
        );
        assert_eq!(
            score, 0.0,
            "short/stop tokens must not inflate score: {score}"
        );
    }

    #[test]
    fn text_relevance_stem_prefix_matches_morphology() {
        // A query token must match agent tokens that share a >=4-char
        // common prefix so morphology like navigate/navigates/navigating
        // counts as a hit.
        let score = text_relevance(
            &["navigate"],
            "",
            &["help navigating the agentic corpus".into()],
        );
        assert!(score > 0.0, "navigate must match navigating: {score}");
    }

    #[test]
    fn text_relevance_multi_occurrence_boosts() {
        let one = text_relevance(&["agent"], "agent", &[]);
        let two = text_relevance(&["agent"], "agent agent", &[]);
        assert!(
            two > one,
            "more occurrences should score higher: {one} vs {two}"
        );
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
