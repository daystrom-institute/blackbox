use std::cmp::Ordering;
use std::collections::BTreeMap;

use anyhow::Result;

use crate::artifacts::{ArtifactCatalog, ArtifactKind, ArtifactListParams};

use super::types::{AtomCostClass, AtomManifest, AtomProvenance};

// ---------------------------------------------------------------------------
// ListFilter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct AtomListFilter {
    pub include_superseded: bool,
    pub cost_class: Option<AtomCostClass>,
    pub provenance_kind: Option<String>,
    pub subcontract: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AtomSearchFilter {
    pub cost_class: Option<AtomCostClass>,
    pub provenance_kind: Option<String>,
    pub subcontract: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AtomSearchResult {
    pub name: String,
    pub version: String,
    pub score: f64,
    pub description: String,
    pub when_to_use: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub cost_class: Option<AtomCostClass>,
    pub provenance_kind: Option<String>,
    pub subcontract: Option<String>,
    pub matched_anti_patterns: Vec<String>,
    pub sources: BTreeMap<String, f64>,
}

// ---------------------------------------------------------------------------
// AtomSummary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AtomSummary {
    pub name: String,
    pub version: String,
    pub active: bool,
    pub description: Option<String>,
    pub cost_class: Option<AtomCostClass>,
    pub provenance_kind: Option<String>,
    pub subcontract: Option<String>,
    pub installed_at: String,
    pub supersedes_chain: Vec<String>,
    pub implementation_kind: Option<String>,
}

// ---------------------------------------------------------------------------
// AtomRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AtomRecord {
    pub name: String,
    pub version: String,
    pub active: bool,
    pub installed_at: String,
    pub source: String,
    pub metadata: AtomRecordMeta,
    pub manifest: Option<AtomManifest>,
    pub manifest_parse_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AtomRecordMeta {
    pub supersedes: Option<String>,
    pub supersedes_chain: Vec<String>,
    pub superseded_by: Option<String>,
    pub install_warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// AtomRegistry — read-only projection over the artifact catalog
// ---------------------------------------------------------------------------

pub struct AtomRegistry<'a> {
    catalog: &'a ArtifactCatalog,
}

impl<'a> AtomRegistry<'a> {
    pub fn new(catalog: &'a ArtifactCatalog) -> Self {
        Self { catalog }
    }

    pub fn list(&self, filter: &AtomListFilter) -> Result<Vec<AtomSummary>> {
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Atom),
            name: None,
            include_superseded: filter.include_superseded,
        };
        let entries = self.catalog.list(&params)?;
        let mut out = Vec::new();
        for entry in entries {
            let (manifest, _parse_err) = self.load_manifest_degraded(&entry.name);
            let cost_class = manifest.as_ref().map(|m| m.cost_class);
            let provenance_kind = manifest.as_ref().and_then(|m| {
                m.provenance.as_ref().map(|p| match p {
                    AtomProvenance::HandAuthored { .. } => "hand_authored".to_string(),
                    AtomProvenance::Distilled { .. } => "distilled".to_string(),
                    AtomProvenance::Imported { .. } => "imported".to_string(),
                })
            });
            let description = manifest.as_ref().map(|m| m.description.clone());
            let implementation_kind = manifest.as_ref().map(|m| match &m.implementation {
                super::types::AtomImplementation::Profile { .. } => "profile".to_string(),
                super::types::AtomImplementation::Workflow { .. } => "workflow".to_string(),
                super::types::AtomImplementation::Deterministic { .. } => {
                    "deterministic".to_string()
                }
                super::types::AtomImplementation::Adapter { .. } => "adapter".to_string(),
            });

            // Read subcontract from raw artifact value
            let subcontract = self
                .catalog
                .load_artifact_value(ArtifactKind::Atom, &entry.name)
                .ok()
                .flatten()
                .and_then(|v| v.get("subcontract")?.as_str().map(String::from));

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
            if let Some(ref wanted_sub) = filter.subcontract {
                if subcontract.as_deref() != Some(wanted_sub) {
                    continue;
                }
            }
            out.push(AtomSummary {
                name: entry.name,
                version: entry.version,
                active: entry.active,
                description,
                cost_class,
                provenance_kind,
                subcontract,
                installed_at: entry.installed_at,
                supersedes_chain: entry.supersedes_chain,
                implementation_kind,
            });
        }
        Ok(out)
    }

    pub fn get(&self, name_or_ref: &str) -> Result<Option<AtomRecord>> {
        let (name, version_pin) = parse_name_or_ref(name_or_ref)?;
        if let Some(version) = version_pin {
            let Some(meta) =
                self.catalog
                    .metadata_for_version(ArtifactKind::Atom, &name, &version)?
            else {
                return Ok(None);
            };
            let (manifest, manifest_parse_error) =
                self.load_manifest_degraded_version(&name, &version);
            return Ok(Some(AtomRecord {
                name: meta.name,
                version: meta.version,
                active: meta.active,
                installed_at: meta.installed_at,
                source: meta.source,
                metadata: AtomRecordMeta {
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
            kind: Some(ArtifactKind::Atom),
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
            .metadata_for(ArtifactKind::Atom, &entry.name)
            .ok()
            .flatten();
        let supersedes = metadata.as_ref().and_then(|m| m.supersedes.clone());
        let install_warnings = metadata.map(|m| m.install_warnings).unwrap_or_default();
        Ok(Some(AtomRecord {
            name: entry.name,
            version: entry.version,
            active: entry.active,
            installed_at: entry.installed_at,
            source: entry.source,
            metadata: AtomRecordMeta {
                supersedes,
                supersedes_chain: entry.supersedes_chain,
                superseded_by: entry.superseded_by,
                install_warnings,
            },
            manifest,
            manifest_parse_error,
        }))
    }

    pub fn load_manifest_degraded(&self, name: &str) -> (Option<AtomManifest>, Option<String>) {
        let value = match self.catalog.load_artifact_value(ArtifactKind::Atom, name) {
            Ok(Some(v)) => v,
            _ => return (None, None),
        };
        load_manifest_degraded_value(value)
    }

    fn load_manifest_degraded_version(
        &self,
        name: &str,
        version: &str,
    ) -> (Option<AtomManifest>, Option<String>) {
        let value =
            match self
                .catalog
                .load_artifact_value_version(ArtifactKind::Atom, name, version)
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
        filter: &AtomSearchFilter,
        exclude_anti_pattern_matches: bool,
    ) -> Result<Vec<AtomSearchResult>> {
        let params = ArtifactListParams {
            kind: Some(ArtifactKind::Atom),
            name: None,
            include_superseded: false,
        };
        let entries = self.catalog.list(&params)?;
        let query_lower = query.to_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut candidates: Vec<AtomSearchResult> = Vec::new();

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
                AtomProvenance::HandAuthored { .. } => "hand_authored",
                AtomProvenance::Distilled { .. } => "distilled",
                AtomProvenance::Imported { .. } => "imported",
            });

            let subcontract = self
                .catalog
                .load_artifact_value(ArtifactKind::Atom, &entry.name)
                .ok()
                .flatten()
                .and_then(|v| v.get("subcontract")?.as_str().map(String::from));

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
            if let Some(ref wanted_sub) = filter.subcontract {
                if subcontract.as_deref() != Some(wanted_sub.as_str()) {
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
            let mut sources = BTreeMap::new();
            if positive_score > 0.0 {
                sources.insert("keyword".into(), positive_score);
            }
            let final_score = (positive_score - penalty).max(0.0);

            if final_score <= 0.0 {
                continue;
            }

            candidates.push(AtomSearchResult {
                name: entry.name,
                version: entry.version,
                score: final_score,
                description: manifest.description,
                when_to_use: manifest.when_to_use,
                anti_patterns: manifest.anti_patterns,
                cost_class: Some(cost_class),
                provenance_kind: provenance_kind.map(String::from),
                subcontract,
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

fn load_manifest_degraded_value(
    value: serde_json::Value,
) -> (Option<AtomManifest>, Option<String>) {
    let manifest_value = value.get("manifest").unwrap_or(&value);
    match serde_json::from_value(manifest_value.clone()) {
        Ok(m) => (Some(m), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

pub fn parse_name_or_ref(input: &str) -> Result<(String, Option<String>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("atom ref must not be empty");
    }
    if let Some(rest) = trimmed.strip_prefix("atom:") {
        if rest.is_empty() {
            anyhow::bail!("atom ref 'atom:' requires a name after the prefix");
        }
        if let Some(latest_rest) = rest.strip_suffix("@latest") {
            if latest_rest.is_empty() {
                anyhow::bail!("atom ref '@latest' requires a name before @latest");
            }
            return Ok((latest_rest.to_string(), None));
        }
        return parse_versioned(rest);
    }
    if let Some(latest_rest) = trimmed.strip_suffix("@latest") {
        if latest_rest.is_empty() {
            anyhow::bail!("atom ref '@latest' requires a name before @latest");
        }
        return Ok((latest_rest.to_string(), None));
    }
    parse_versioned(trimmed)
}

fn parse_versioned(input: &str) -> Result<(String, Option<String>)> {
    if let Some((name, ver)) = input.rsplit_once("@v") {
        if name.is_empty() {
            anyhow::bail!("atom ref '@v{ver}' requires a name before @v");
        }
        if ver.is_empty() {
            anyhow::bail!("atom ref '{name}@v' requires a version after @v");
        }
        let v: u64 = ver.parse().map_err(|_| {
            anyhow::anyhow!("atom ref version must be a positive integer, got '{ver}'")
        })?;
        if v == 0 {
            anyhow::bail!("atom ref version must be positive, got 0");
        }
        Ok((name.to_string(), Some(ver.to_string())))
    } else {
        Ok((input.to_string(), None))
    }
}

// ---------------------------------------------------------------------------
// Text relevance scoring (simplified BM25-like, shared with agent registry)
// ---------------------------------------------------------------------------

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "are", "but", "with", "that", "this", "from", "into", "your",
    "they", "them", "their", "there", "when", "what", "which", "while", "would", "could", "should",
    "have", "has", "had", "was", "were", "been", "being", "will", "shall", "can", "may", "not",
    "use", "using", "yes", "no", "off", "any", "all", "some", "more", "most", "much", "many",
    "few", "such", "very", "just", "only", "than", "then", "now", "how", "why", "where", "who",
    "whom", "whose", "its", "his", "her", "him", "she", "out", "our", "ours", "via", "per",
];

fn token_match(query_term: &str, target_token: &str) -> bool {
    if query_term == target_token {
        return true;
    }
    if query_term.len().min(target_token.len()) < 4 {
        return false;
    }
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
        let term = term.trim();
        if term.len() < 3 {
            continue;
        }
        if STOPWORDS.contains(&term) {
            continue;
        }
        let term_lc = term.to_lowercase();
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

fn cost_rank(cc: Option<AtomCostClass>) -> u8 {
    match cc {
        Some(AtomCostClass::Cheap) => 0,
        Some(AtomCostClass::Normal) => 1,
        Some(AtomCostClass::Expensive) => 2,
        None => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_catalog(dir: &tempfile::TempDir) -> ArtifactCatalog {
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Atom,
                "reviewer.json".into(),
                &serde_json::json!({
                    "_contract": "atom/v1",
                    "kind": "atom",
                    "name": "atom-reviewer",
                    "version": 1,
                    "manifest": {
                        "description": "Reviews code for quality and correctness.",
                        "when_to_use": ["after code changes", "on PR"],
                        "implementation": { "kind": "profile", "brofile_ref": "brofile:reviewer@v1" },
                        "cost_class": "expensive",
                        "provenance": {"kind": "hand_authored", "author": "user"}
                    }
                }),
                None, None, None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Atom,
                "scout.json".into(),
                &serde_json::json!({
                    "_contract": "atom/v1",
                    "kind": "atom",
                    "name": "scout",
                    "version": 1,
                    "manifest": {
                        "description": "Quick research scout for factual questions.",
                        "when_to_use": ["when you need a quick answer"],
                        "implementation": { "kind": "profile", "brofile_ref": "brofile:scout@v1" },
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
        let registry = AtomRegistry::new(&catalog);
        let results = registry.list(&AtomListFilter::default()).unwrap();
        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"atom-reviewer"));
        assert!(names.contains(&"scout"));
    }

    #[test]
    fn get_by_bare_name() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AtomRegistry::new(&catalog);
        let record = registry.get("atom-reviewer").unwrap().unwrap();
        assert_eq!(record.version, "1");
        assert!(record.active);
        assert!(record.manifest.is_some());
        assert_eq!(
            record.manifest.as_ref().unwrap().cost_class,
            AtomCostClass::Expensive
        );
    }

    #[test]
    fn get_by_atom_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AtomRegistry::new(&catalog);
        let record = registry.get("atom:atom-reviewer@v1").unwrap().unwrap();
        assert_eq!(record.version, "1");
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AtomRegistry::new(&catalog);
        assert!(registry.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn search_finds_by_description() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AtomRegistry::new(&catalog);
        let results = registry
            .search("code review", 5, &AtomSearchFilter::default(), false)
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "atom-reviewer");
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
    fn parse_name_or_ref_atom_prefix() {
        let (name, ver) = parse_name_or_ref("atom:reviewer@v1").unwrap();
        assert_eq!(name, "reviewer");
        assert_eq!(ver, Some("1".into()));
    }

    #[test]
    fn parse_name_or_ref_atom_latest() {
        let (name, ver) = parse_name_or_ref("atom:reviewer@latest").unwrap();
        assert_eq!(name, "reviewer");
        assert!(ver.is_none());
    }

    #[test]
    fn parse_name_or_ref_bare_latest() {
        let (name, ver) = parse_name_or_ref("reviewer@latest").unwrap();
        assert_eq!(name, "reviewer");
        assert!(ver.is_none());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_name_or_ref("").is_err());
    }

    #[test]
    fn parse_rejects_bare_atom_prefix() {
        assert!(parse_name_or_ref("atom:").is_err());
    }

    #[test]
    fn implementation_kind_in_summary() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = setup_catalog(&dir);
        let registry = AtomRegistry::new(&catalog);
        let results = registry.list(&AtomListFilter::default()).unwrap();
        let reviewer = results.iter().find(|r| r.name == "atom-reviewer").unwrap();
        assert_eq!(reviewer.implementation_kind.as_deref(), Some("profile"));
    }
}
