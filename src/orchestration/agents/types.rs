use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// AgentArtifact — top-level wrapper for installed agent files
// ---------------------------------------------------------------------------

#[allow(dead_code)] // used by tests in same file and badgey/mod.rs test module
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentArtifact {
    pub kind: String,
    pub name: String,
    pub version: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    pub manifest: AgentManifest,
}

// ---------------------------------------------------------------------------
// AgentManifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentManifest {
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_to_use: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anti_patterns: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brofile_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brofile_inline: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_overlay: Option<AgentFilterOverlay>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<AgentInputSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<AgentOutputSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<AgentComposition>,

    #[serde(default = "default_cost_class")]
    pub cost_class: AgentCostClass,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_adapter: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<AgentProvenance>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<AgentEmbedding>,
}

// ---------------------------------------------------------------------------
// AgentFilterOverlay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentFilterOverlay {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallow: Vec<String>,
}

impl Default for AgentFilterOverlay {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            disallow: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentInputSpec / AgentOutputSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInputSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentOutputSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_density: Option<EvidenceDensity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceDensity {
    Low,
    Medium,
    High,
}

// ---------------------------------------------------------------------------
// AgentComposition + CompositionShape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentComposition {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chainable_after: Vec<String>,
    #[serde(default)]
    pub parallel_safe: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out_aggregator: Option<String>,
}

#[allow(dead_code)] // used by tests in same file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositionShape {
    Chain,
    FanOut,
    Escalation,
}

// ---------------------------------------------------------------------------
// CostClass
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCostClass {
    Cheap,
    Normal,
    Expensive,
}

fn default_cost_class() -> AgentCostClass {
    AgentCostClass::Normal
}

impl fmt::Display for AgentCostClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AgentCostClass::Cheap => "cheap",
            AgentCostClass::Normal => "normal",
            AgentCostClass::Expensive => "expensive",
        })
    }
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentProvenance {
    HandAuthored {
        author: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    Distilled {
        distilled_by: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        evidence_session_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        created_from_threads: Vec<String>,
        #[serde(default)]
        accept_count: u32,
        #[serde(default)]
        reject_count: u32,
    },
    Imported {
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        import_at: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Embedding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentEmbedding {
    /// Back-compat primary vector reference. New code should read
    /// `components.primary`, but this field keeps the manifest easy to scan.
    pub model: String,
    pub computed_at: String,
    #[serde(default)]
    pub vector_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_route: Option<String>,
    #[serde(default)]
    pub components: AgentEmbeddingComponents,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEmbeddingComponents {
    #[serde(default)]
    pub primary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anti_patterns: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentRef
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentRef {
    pub name: String,
    pub version: u32,
}

impl AgentRef {
    pub fn render(&self) -> String {
        format!("agent:{}@v{}", self.name, self.version)
    }

    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let rest = input.strip_prefix("agent:")?;
        if rest.contains(':') {
            return None;
        }
        let (name, version_str) = rest.rsplit_once("@v")?;
        if name.is_empty() {
            return None;
        }
        let version: u32 = version_str.parse().ok()?;
        if version == 0 {
            return None;
        }
        Some(Self {
            name: name.to_string(),
            version,
        })
    }
}

impl fmt::Display for AgentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl FromStr for AgentRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("invalid agent ref: {s}"))
    }
}

// ---------------------------------------------------------------------------
// AgentSession
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSession {
    pub session_id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    pub agent: AgentRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

// ---------------------------------------------------------------------------
// BadgeyAgentArgs
// ---------------------------------------------------------------------------

#[allow(dead_code)] // used by tests in same file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BadgeyAgentArgs {
    pub prompt: String,
    pub badgey_id: String,
}

// ---------------------------------------------------------------------------
// MergedFilters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergedFilters {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub disallow: Vec<String>,
}

impl Default for MergedFilters {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            disallow: Vec::new(),
        }
    }
}

impl Default for AgentManifest {
    fn default() -> Self {
        Self {
            description: String::new(),
            when_to_use: Vec::new(),
            anti_patterns: Vec::new(),
            brofile_ref: None,
            brofile_inline: None,
            filter_overlay: None,
            inputs: None,
            outputs: None,
            composition: None,
            cost_class: default_cost_class(),
            dispatch_adapter: None,
            provenance: None,
            embedding: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (§4.3 lints — focused, not the full validator)
// ---------------------------------------------------------------------------

pub fn validate_description_length(desc: &str) -> Result<(), String> {
    let len = desc.len();
    if len < 10 {
        return Err(format!("description too short ({len} chars, minimum 10)"));
    }
    if len > 500 {
        return Err(format!("description too long ({len} chars, maximum 500)"));
    }
    Ok(())
}

pub fn validate_when_to_use_nonempty(items: &[String]) -> Result<(), String> {
    if items.is_empty() {
        return Err("when_to_use must be non-empty".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_manifest_serde_round_trip() {
        let manifest = AgentManifest {
            description: "Reviews code for security and correctness.".into(),
            when_to_use: vec![
                "after writing or modifying code".into(),
                "when the user asks for a review".into(),
            ],
            anti_patterns: vec!["do not use for one-line typo fixes".into()],
            brofile_ref: Some("code-reviewer-persona".into()),
            brofile_inline: None,
            filter_overlay: Some(AgentFilterOverlay {
                allow: vec!["mcp__blackbox__bbox_*".into(), "Read".into()],
                disallow: vec!["Bash".into()],
            }),
            inputs: Some(AgentInputSpec {
                schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "diff": {"type": "string"}
                    },
                    "required": ["diff"]
                })),
                prompt_template: Some("Review the following diff:\n\n{{diff}}".into()),
            }),
            outputs: Some(AgentOutputSpec {
                schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "verdict": {"enum": ["approve", "revise", "reject"]}
                    }
                })),
                evidence_density: Some(EvidenceDensity::High),
            }),
            composition: Some(AgentComposition {
                chainable_after: vec!["test-author".into()],
                parallel_safe: true,
                fan_out_aggregator: Some("vote-majority".into()),
            }),
            cost_class: AgentCostClass::Normal,
            dispatch_adapter: None,
            provenance: Some(AgentProvenance::HandAuthored {
                author: "user".into(),
                created_at: Some("2026-04-30T12:00:00Z".into()),
            }),
            embedding: None,
        };
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: AgentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn agent_manifest_minimal_required_fields() {
        let json = serde_json::json!({
            "description": "A minimal agent"
        });
        let manifest: AgentManifest = serde_json::from_value(json).unwrap();
        assert_eq!(manifest.description, "A minimal agent");
        assert!(manifest.when_to_use.is_empty());
        assert!(manifest.anti_patterns.is_empty());
        assert!(manifest.brofile_ref.is_none());
        assert!(manifest.brofile_inline.is_none());
        assert!(manifest.filter_overlay.is_none());
        assert_eq!(manifest.cost_class, AgentCostClass::Normal);
        assert!(manifest.dispatch_adapter.is_none());
        assert!(manifest.provenance.is_none());
    }

    #[test]
    fn agent_ref_round_trip() {
        let r = AgentRef {
            name: "code-reviewer".into(),
            version: 3,
        };
        assert_eq!(r.render(), "agent:code-reviewer@v3");
        let parsed = AgentRef::parse(&r.render()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn agent_ref_parse_rejects_missing_prefix() {
        assert!(AgentRef::parse("code-reviewer@v3").is_none());
    }

    #[test]
    fn agent_ref_parse_rejects_bad_version() {
        assert!(AgentRef::parse("agent:reviewer@vabc").is_none());
    }

    #[test]
    fn agent_ref_parse_rejects_empty_name() {
        assert!(AgentRef::parse("agent:@v1").is_none());
    }

    #[test]
    fn agent_ref_parse_rejects_colon_in_name() {
        assert!(AgentRef::parse("agent:bad:name@v1").is_none());
    }

    #[test]
    fn agent_ref_parse_rejects_version_zero() {
        assert!(AgentRef::parse("agent:reviewer@v0").is_none());
    }

    #[test]
    fn agent_ref_from_str_works() {
        let r: AgentRef = "agent:foo@v1".parse().unwrap();
        assert_eq!(r.name, "foo");
        assert_eq!(r.version, 1);
    }

    #[test]
    fn agent_ref_from_str_rejects_invalid() {
        assert!("bad-ref".parse::<AgentRef>().is_err());
    }

    #[test]
    fn agent_session_serde_round_trip() {
        let session = AgentSession {
            session_id: "sess-abc123".into(),
            provider: "claude".into(),
            project_dir: Some("/repo/project".into()),
            agent: AgentRef {
                name: "reviewer".into(),
                version: 2,
            },
            task_id: Some("task-xyz".into()),
        };
        let json = serde_json::to_string(&session).unwrap();
        let parsed: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, session);
    }

    #[test]
    fn provenance_distilled_round_trip() {
        let prov = AgentProvenance::Distilled {
            distilled_by: "badgey-01".into(),
            evidence_session_ids: vec!["sess-1".into(), "sess-2".into()],
            created_from_threads: vec!["thread-abc".into()],
            accept_count: 5,
            reject_count: 1,
        };
        let json = serde_json::to_string(&prov).unwrap();
        let parsed: AgentProvenance = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, prov);
    }

    #[test]
    fn provenance_imported_round_trip() {
        let prov = AgentProvenance::Imported {
            source: "claude-native".into(),
            import_at: Some("2026-05-01T00:00:00Z".into()),
        };
        let json = serde_json::to_string(&prov).unwrap();
        let parsed: AgentProvenance = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, prov);
    }

    #[test]
    fn filter_overlay_defaults_empty() {
        let overlay = AgentFilterOverlay::default();
        assert!(overlay.allow.is_empty());
        assert!(overlay.disallow.is_empty());
    }

    #[test]
    fn cost_class_display() {
        assert_eq!(AgentCostClass::Cheap.to_string(), "cheap");
        assert_eq!(AgentCostClass::Normal.to_string(), "normal");
        assert_eq!(AgentCostClass::Expensive.to_string(), "expensive");
    }

    #[test]
    fn manifest_description_required() {
        let json = serde_json::json!({});
        let result = serde_json::from_value::<AgentManifest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn badgey_agent_args_serde_round_trip() {
        let args = BadgeyAgentArgs {
            prompt: "Review this code".into(),
            badgey_id: "badgey-01".into(),
        };
        let json = serde_json::to_string(&args).unwrap();
        let parsed: BadgeyAgentArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, args);
    }

    #[test]
    fn composition_shape_serde_round_trip() {
        for shape in &[
            CompositionShape::Chain,
            CompositionShape::FanOut,
            CompositionShape::Escalation,
        ] {
            let json = serde_json::to_string(&shape).unwrap();
            let parsed: CompositionShape = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *shape);
        }
    }

    #[test]
    fn agent_artifact_serde_round_trip() {
        let artifact = AgentArtifact {
            kind: "agent".into(),
            name: "code-reviewer".into(),
            version: serde_json::json!(1),
            supersedes: None,
            manifest: AgentManifest {
                description: "Reviews code for security.".into(),
                when_to_use: vec!["after writing code".into()],
                brofile_ref: Some("reviewer-persona".into()),
                ..Default::default()
            },
        };
        let json = serde_json::to_string_pretty(&artifact).unwrap();
        let parsed: AgentArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, artifact);
    }

    #[test]
    fn validate_description_length_accepts_valid() {
        assert!(validate_description_length("This is a valid description.").is_ok());
    }

    #[test]
    fn validate_description_length_rejects_too_short() {
        let result = validate_description_length("short");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too short"));
    }

    #[test]
    fn validate_description_length_rejects_too_long() {
        let long = "x".repeat(501);
        let result = validate_description_length(&long);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too long"));
    }

    #[test]
    fn validate_description_length_accepts_boundary_10() {
        assert!(validate_description_length("1234567890").is_ok());
    }

    #[test]
    fn validate_description_length_accepts_boundary_500() {
        assert!(validate_description_length(&"x".repeat(500)).is_ok());
    }

    #[test]
    fn validate_when_to_use_nonempty_rejects_empty() {
        let result = validate_when_to_use_nonempty(&[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-empty"));
    }

    #[test]
    fn validate_when_to_use_nonempty_accepts_nonempty() {
        assert!(validate_when_to_use_nonempty(&["after writing code".into()]).is_ok());
    }
}
