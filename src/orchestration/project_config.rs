//! Accepted project bro configuration.
//!
//! In catalog mode the daemon holds no checkout authority, so repo-owned
//! project configuration (`.bro/brofiles/<name>.json`,
//! `.bro/teamplates/<name>.json`, `.bbox/mcp.json`, plus the committed
//! `.bbox/config.toml` that reports MCP enablement) is read only from the
//! project's accepted publication and written only through the checkout-owner
//! mutation lane. This module is the typed view over that accepted lane:
//! parsing, exact-scope lookup, and the project-first/global resolution that
//! dispatch uses. It never touches the filesystem for project configuration;
//! global fallback reads only the daemon-owned global stores.
//!
//! Resolution falls back to global configuration only when the accepted view
//! proves the requested project override absent. A missing publication, an
//! accepted generation without the configuration lane, and configuration
//! that does not parse or exceeds a bound are errors that name their remedy,
//! never a silent global fallback. Error text carries paths and positions,
//! never configuration bytes: MCP stores may hold credentials.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use bbox_code_source::{PROJECT_CONFIG_TOML_PATH, ProjectConfigTargetV1};
use bbox_corpus_core::identity::PublishedScope;
use serde::Serialize;

use super::brofile::Brofile;
use super::mcp::McpStore;
use super::team::Teamplate;

/// Why the accepted configuration view cannot answer. Every variant refuses
/// the read: none of them proves a project override absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectConfigError {
    /// The project has no verified accepted publication, or the accepted
    /// publication could not be verified.
    PublicationUnavailable { project_id: String, detail: String },
    /// The accepted generation carries no configuration lane: its producer,
    /// or the daemon that accepted it, predates the lane.
    LaneUnsupported {
        project_id: String,
        generation_id: String,
    },
    /// The accepted configuration does not parse or exceeds a bound.
    Invalid { project_id: String, detail: String },
}

impl ProjectConfigError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PublicationUnavailable { .. } => "error.project_config_publication_unavailable",
            Self::LaneUnsupported { .. } => "error.project_config_lane_unsupported",
            Self::Invalid { .. } => "error.project_config_invalid",
        }
    }
}

impl fmt::Display for ProjectConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicationUnavailable { project_id, detail } => write!(
                formatter,
                "{}: project {project_id} has no verified accepted publication ({detail}). \
                 In catalog mode project bro configuration is read only from accepted \
                 publication: enable published knowledge for the project on its checkout-owner \
                 collector, commit, and let the publisher accept it. No global fallback was applied",
                self.code()
            ),
            Self::LaneUnsupported {
                project_id,
                generation_id,
            } => write!(
                formatter,
                "{}: accepted generation {generation_id} of project {project_id} carries no \
                 configuration lane (its collector or the accepting daemon predates the lane). \
                 Upgrade the checkout-owner collector and publish a commit. No global fallback \
                 was applied",
                self.code()
            ),
            Self::Invalid { project_id, detail } => write!(
                formatter,
                "{}: accepted configuration of project {project_id} is invalid: {detail}. Fix \
                 the committed file in the owning checkout and publish again. No global \
                 fallback was applied",
                self.code()
            ),
        }
    }
}

impl std::error::Error for ProjectConfigError {}

/// Where an accepted snapshot came from, echoed on every attributed read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectConfigProvenance {
    pub project_id: String,
    pub accepted_generation: String,
    pub accepted_commit: String,
}

/// One project's accepted configuration, parsed from exactly one accepted
/// generation. Immutable: a new publication is a new snapshot.
#[derive(Debug, Clone)]
pub struct ProjectConfigSnapshot {
    provenance: ProjectConfigProvenance,
    scope: PublishedScope,
    brofiles: BTreeMap<String, Brofile>,
    teamplates: BTreeMap<String, Teamplate>,
    mcp_store: Option<McpStore>,
    mcp_enabled: Option<bool>,
    accepted_bytes: BTreeMap<ProjectConfigTargetV1, String>,
}

impl ProjectConfigSnapshot {
    /// Parse one accepted configuration lane. `sources` are the lane's
    /// repository-relative files and exact bytes. Any file that is not a
    /// configuration input of `scope`, is not UTF-8, or does not parse as its
    /// type makes the whole snapshot invalid: a partial view could resolve a
    /// name to global configuration while the project meant to override it.
    pub fn parse<'a>(
        provenance: ProjectConfigProvenance,
        scope: &PublishedScope,
        sources: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<Self, ProjectConfigError> {
        let invalid = |detail: String| ProjectConfigError::Invalid {
            project_id: provenance.project_id.clone(),
            detail,
        };
        let mut snapshot = Self {
            provenance: provenance.clone(),
            scope: scope.clone(),
            brofiles: BTreeMap::new(),
            teamplates: BTreeMap::new(),
            mcp_store: None,
            mcp_enabled: None,
            accepted_bytes: BTreeMap::new(),
        };
        for (filename, bytes) in sources {
            let relative =
                bbox_knowledge_source::config_source_scope_relative_path(scope, filename)
                    .ok_or_else(|| {
                        invalid(format!(
                            "{} is not a configuration input of the published scope",
                            bounded_path(filename)
                        ))
                    })?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| invalid(format!("{relative} is not UTF-8 text")))?;
            if relative == PROJECT_CONFIG_TOML_PATH {
                let config = crate::config::parse_project_config_source(text).map_err(|_| {
                    invalid(format!(
                        "{relative} does not parse as project configuration"
                    ))
                })?;
                snapshot.mcp_enabled = config.mcp.enabled;
                continue;
            }
            let Some(target) = ProjectConfigTargetV1::from_relative_path(relative) else {
                return Err(invalid(format!(
                    "{relative} is not a project configuration target"
                )));
            };
            match &target {
                ProjectConfigTargetV1::Brofile(name) => {
                    snapshot
                        .brofiles
                        .insert(name.clone(), parse_json(relative, text).map_err(invalid)?);
                }
                ProjectConfigTargetV1::Teamplate(name) => {
                    snapshot
                        .teamplates
                        .insert(name.clone(), parse_json(relative, text).map_err(invalid)?);
                }
                ProjectConfigTargetV1::McpStore => {
                    snapshot.mcp_store = Some(parse_json(relative, text).map_err(invalid)?);
                }
            }
            snapshot.accepted_bytes.insert(target, text.to_string());
        }
        Ok(snapshot)
    }

    pub fn provenance(&self) -> &ProjectConfigProvenance {
        &self.provenance
    }

    pub fn scope(&self) -> &PublishedScope {
        &self.scope
    }

    /// Exact project-scope lookup by file name; never falls back.
    pub fn brofile(&self, name: &str) -> Option<&Brofile> {
        self.brofiles.get(name)
    }

    pub fn brofiles(&self) -> impl Iterator<Item = (&str, &Brofile)> {
        self.brofiles
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    pub fn teamplate(&self, name: &str) -> Option<&Teamplate> {
        self.teamplates.get(name)
    }

    pub fn teamplates(&self) -> impl Iterator<Item = (&str, &Teamplate)> {
        self.teamplates
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    /// The accepted `.bbox/mcp.json`, `None` when the project publishes none.
    pub fn mcp_store(&self) -> Option<&McpStore> {
        self.mcp_store.as_ref()
    }

    /// `[mcp] enabled` from the committed `.bbox/config.toml`; `None` when
    /// the key (or the file) is absent.
    pub fn mcp_enabled(&self) -> Option<bool> {
        self.mcp_enabled
    }

    /// Exact accepted bytes of one writable target, the publication base a
    /// guarded mutation reconciles against.
    pub fn accepted_bytes(&self, target: &ProjectConfigTargetV1) -> Option<&str> {
        self.accepted_bytes.get(target).map(String::as_str)
    }
}

/// Which store answered an attributed resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProjectConfigSource {
    /// The project's accepted configuration.
    Project(ProjectConfigProvenance),
    /// The daemon's global store, because the accepted project view proves
    /// the override absent.
    GlobalFallback(ProjectConfigProvenance),
    /// The daemon's global store; no project context was selected.
    Global,
    /// Bridge mode: the daemon-local checkout, project first then global.
    Local,
}

#[derive(Debug, Clone)]
pub struct Resolved<T> {
    pub value: T,
    pub source: ProjectConfigSource,
}

/// Project-first/global resolution over an accepted snapshot. The project
/// view answers first; only its proven absence consults the global store.
pub fn resolve_brofile(
    project: Option<&ProjectConfigSnapshot>,
    name: &str,
    store_dir: &Path,
) -> Option<Resolved<Brofile>> {
    if let Some(snapshot) = project {
        if let Some(brofile) = snapshot.brofile(name) {
            return Some(Resolved {
                value: brofile.clone(),
                source: ProjectConfigSource::Project(snapshot.provenance.clone()),
            });
        }
    }
    super::brofile::resolve_brofile(name, store_dir, None).map(|value| Resolved {
        value,
        source: fallback_source(project),
    })
}

/// Same precedence for teamplates.
pub fn resolve_teamplate(
    project: Option<&ProjectConfigSnapshot>,
    name: &str,
    store_dir: &Path,
) -> Option<Resolved<Teamplate>> {
    if let Some(snapshot) = project {
        if let Some(teamplate) = snapshot.teamplate(name) {
            return Some(Resolved {
                value: teamplate.clone(),
                source: ProjectConfigSource::Project(snapshot.provenance.clone()),
            });
        }
    }
    super::team::resolve_teamplate(name, store_dir, None).map(|value| Resolved {
        value,
        source: fallback_source(project),
    })
}

fn fallback_source(project: Option<&ProjectConfigSnapshot>) -> ProjectConfigSource {
    match project {
        Some(snapshot) => ProjectConfigSource::GlobalFallback(snapshot.provenance.clone()),
        None => ProjectConfigSource::Global,
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(relative: &str, text: &str) -> Result<T, String> {
    // Serde messages can quote the offending value, and an MCP store may hold
    // credentials, so only the failure class and position are reported.
    serde_json::from_str(text).map_err(|error| {
        let class = match error.classify() {
            serde_json::error::Category::Io => "unreadable",
            serde_json::error::Category::Syntax => "malformed JSON",
            serde_json::error::Category::Data => "a value of the wrong shape",
            serde_json::error::Category::Eof => "truncated JSON",
        };
        format!(
            "{relative} has {class} at line {}, column {}",
            error.line(),
            error.column()
        )
    })
}

fn bounded_path(path: &str) -> String {
    const LIMIT: usize = 160;
    if path.chars().count() <= LIMIT {
        return path.to_string();
    }
    let kept: String = path.chars().take(LIMIT).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> ProjectConfigProvenance {
        ProjectConfigProvenance {
            project_id: "p_00000000000000000000000000000001".into(),
            accepted_generation: "apg_test".into(),
            accepted_commit: "1".repeat(40),
        }
    }

    fn scope(relative: &str) -> PublishedScope {
        PublishedScope::try_new("repo_example", relative).unwrap()
    }

    fn brofile_json(name: &str, model: &str) -> String {
        serde_json::json!({"name": name, "provider": "claude", "model": model}).to_string()
    }

    fn parse(
        scope: &PublishedScope,
        files: &[(&str, String)],
    ) -> Result<ProjectConfigSnapshot, ProjectConfigError> {
        ProjectConfigSnapshot::parse(
            provenance(),
            scope,
            files.iter().map(|(path, bytes)| (*path, bytes.as_bytes())),
        )
    }

    #[test]
    fn snapshot_parses_every_input_and_keeps_exact_bytes() {
        let brofile = brofile_json("reviewer", "opus");
        let snapshot = parse(
            &scope("services/api"),
            &[
                (
                    "services/api/.bbox/config.toml",
                    "[mcp]\nenabled = false\n".into(),
                ),
                (
                    "services/api/.bbox/mcp.json",
                    r#"{"version":1,"servers":{},"filters":{"allow":[],"disallow":["x"]}}"#.into(),
                ),
                ("services/api/.bro/brofiles/reviewer.json", brofile.clone()),
                (
                    "services/api/.bro/teamplates/squad.json",
                    r#"{"name":"squad","members":[{"brofile":"reviewer","count":1}]}"#.into(),
                ),
            ],
        )
        .unwrap();
        assert_eq!(snapshot.mcp_enabled(), Some(false));
        assert_eq!(
            snapshot.brofile("reviewer").unwrap().model.as_deref(),
            Some("opus")
        );
        assert!(snapshot.teamplate("squad").is_some());
        assert_eq!(snapshot.mcp_store().unwrap().filters.disallow, vec!["x"]);
        assert_eq!(
            snapshot.accepted_bytes(&ProjectConfigTargetV1::Brofile("reviewer".into())),
            Some(brofile.as_str())
        );
        assert_eq!(
            snapshot.accepted_bytes(&ProjectConfigTargetV1::Brofile("other".into())),
            None
        );
    }

    #[test]
    fn enablement_reports_true_false_and_absent() {
        for (source, expected) in [
            ("[mcp]\nenabled = true\n", Some(true)),
            ("[mcp]\nenabled = false\n", Some(false)),
            ("[project]\naliases = []\n", None),
        ] {
            let snapshot = parse(&scope("."), &[(".bbox/config.toml", source.into())]).unwrap();
            assert_eq!(snapshot.mcp_enabled(), expected, "{source}");
        }
        let without_file = parse(&scope("."), &[]).unwrap();
        assert_eq!(without_file.mcp_enabled(), None);
    }

    #[test]
    fn malformed_inputs_refuse_the_whole_snapshot_without_echoing_bytes() {
        for (path, bytes) in [
            (
                ".bbox/config.toml",
                "[mcp]\nenabled = \"sk-secret-value\"\n",
            ),
            (
                ".bbox/mcp.json",
                r#"{"version":1,"servers":"sk-secret-value"}"#,
            ),
            (
                ".bro/brofiles/reviewer.json",
                r#"{"name":"sk-secret-value"}"#,
            ),
            (".bro/teamplates/squad.json", "{not json sk-secret-value"),
        ] {
            let error = parse(&scope("."), &[(path, bytes.into())]).unwrap_err();
            assert_eq!(error.code(), "error.project_config_invalid");
            let text = error.to_string();
            assert!(text.contains(path), "{text}");
            assert!(!text.contains("sk-secret-value"), "{text}");
        }
        let error =
            parse(&scope("."), &[(".bro/brofiles/nested/x.json", "{}".into())]).unwrap_err();
        assert_eq!(error.code(), "error.project_config_invalid");
        let non_text = ProjectConfigSnapshot::parse(
            provenance(),
            &scope("."),
            [(".bbox/mcp.json", [0xff_u8, 0xfe].as_slice())],
        )
        .unwrap_err();
        assert_eq!(non_text.code(), "error.project_config_invalid");
    }

    #[test]
    fn resolution_prefers_the_project_and_attributes_global_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().canonicalize().unwrap();
        std::fs::create_dir_all(store.join("brofiles")).unwrap();
        std::fs::write(
            store.join("brofiles/reviewer.json"),
            brofile_json("reviewer", "global-model"),
        )
        .unwrap();
        std::fs::write(
            store.join("brofiles/writer.json"),
            brofile_json("writer", "global-writer"),
        )
        .unwrap();
        let snapshot = parse(
            &scope("."),
            &[(
                ".bro/brofiles/reviewer.json",
                brofile_json("reviewer", "project-model"),
            )],
        )
        .unwrap();

        let project = resolve_brofile(Some(&snapshot), "reviewer", &store).unwrap();
        assert_eq!(project.value.model.as_deref(), Some("project-model"));
        assert_eq!(project.source, ProjectConfigSource::Project(provenance()));

        let fallback = resolve_brofile(Some(&snapshot), "writer", &store).unwrap();
        assert_eq!(fallback.value.model.as_deref(), Some("global-writer"));
        assert_eq!(
            fallback.source,
            ProjectConfigSource::GlobalFallback(provenance())
        );

        let global = resolve_brofile(None, "reviewer", &store).unwrap();
        assert_eq!(global.value.model.as_deref(), Some("global-model"));
        assert_eq!(global.source, ProjectConfigSource::Global);

        assert!(resolve_brofile(Some(&snapshot), "missing", &store).is_none());
    }

    #[test]
    fn errors_name_their_code_and_remedy() {
        let unavailable = ProjectConfigError::PublicationUnavailable {
            project_id: "p".into(),
            detail: "no accepted publication".into(),
        };
        assert!(unavailable.to_string().starts_with(unavailable.code()));
        assert!(unavailable.to_string().contains("No global fallback"));
        let unsupported = ProjectConfigError::LaneUnsupported {
            project_id: "p".into(),
            generation_id: "g".into(),
        };
        assert!(
            unsupported
                .to_string()
                .contains("Upgrade the checkout-owner collector")
        );
    }
}
