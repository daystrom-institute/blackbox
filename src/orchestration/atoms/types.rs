use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// AtomArtifact — top-level envelope for installed atom files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AtomArtifact {
    pub _contract: String,
    pub kind: String,
    pub name: String,
    pub version: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subcontract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    pub manifest: AtomManifest,
}

// ---------------------------------------------------------------------------
// AtomManifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AtomManifest {
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_to_use: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anti_patterns: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<AtomInputSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<AtomOutputSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects: Option<AtomEffects>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<AtomComposition>,

    pub implementation: AtomImplementation,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<AtomSupervisionPolicy>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<AtomTracePolicy>,

    #[serde(default = "default_cost_class")]
    pub cost_class: AtomCostClass,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<AtomProvenance>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<AtomEmbedding>,
}

// ---------------------------------------------------------------------------
// AtomInputSpec / AtomOutputSpec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AtomInputSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AtomOutputSpec {
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
// AtomEffects
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AtomEffects {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes_files: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatches_runs: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses_network: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// AtomComposition + MayInvokeAtoms
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtomComposition {
    pub may_invoke_atoms: MayInvokeAtoms,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MayInvokeAtoms {
    None,
    Any,
    Allowed { atoms: Vec<String> },
}

// ---------------------------------------------------------------------------
// AtomImplementation — closed tagged union
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AtomImplementation {
    Profile { brofile_ref: String },
    Workflow { workflow_ref: String },
    Deterministic { runner: String },
    Adapter { adapter_name: String },
}

// ---------------------------------------------------------------------------
// AtomSupervisionPolicy / AtomTracePolicy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtomSupervisionPolicy {
    #[serde(default = "default_supervision_oracle")]
    pub oracle: String,
    #[serde(default = "default_supervision_advisor")]
    pub advisor: String,
}

fn default_supervision_oracle() -> String {
    "none".to_string()
}
fn default_supervision_advisor() -> String {
    "none".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtomTracePolicy {
    #[serde(default = "default_trace_retain")]
    pub retain: String,
    #[serde(default = "default_portal_focus")]
    pub portal_focus: String,
}

fn default_trace_retain() -> String {
    "summary".to_string()
}
fn default_portal_focus() -> String {
    "on_request".to_string()
}

// ---------------------------------------------------------------------------
// CostClass
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtomCostClass {
    Cheap,
    Normal,
    Expensive,
}

fn default_cost_class() -> AtomCostClass {
    AtomCostClass::Normal
}

impl fmt::Display for AtomCostClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AtomCostClass::Cheap => "cheap",
            AtomCostClass::Normal => "normal",
            AtomCostClass::Expensive => "expensive",
        })
    }
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AtomProvenance {
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
pub struct AtomEmbedding {
    pub model: String,
    pub computed_at: String,
    #[serde(default)]
    pub vector_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_route: Option<String>,
    #[serde(default)]
    pub components: AtomEmbeddingComponents,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtomEmbeddingComponents {
    #[serde(default)]
    pub primary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anti_patterns: Option<String>,
}

// ---------------------------------------------------------------------------
// AtomRef — typed reference: atom:name@vN or atom:name@latest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AtomRef {
    pub name: String,
    pub version: AtomRefVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtomRefVersion {
    Pinned(u32),
    Latest,
}

impl AtomRef {
    pub fn pinned(name: &str, version: u32) -> Self {
        Self {
            name: name.to_string(),
            version: AtomRefVersion::Pinned(version),
        }
    }

    pub fn latest(name: &str) -> Self {
        Self {
            name: name.to_string(),
            version: AtomRefVersion::Latest,
        }
    }

    pub fn render(&self) -> String {
        match &self.version {
            AtomRefVersion::Pinned(v) => format!("atom:{}@v{}", self.name, v),
            AtomRefVersion::Latest => format!("atom:{}@latest", self.name),
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let rest = input.strip_prefix("atom:")?;
        if rest.contains(':') {
            return None;
        }
        if let Some((name, "latest")) = rest.rsplit_once("@") {
            if name.is_empty() {
                return None;
            }
            return Some(Self {
                name: name.to_string(),
                version: AtomRefVersion::Latest,
            });
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
            version: AtomRefVersion::Pinned(version),
        })
    }
}

impl fmt::Display for AtomRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl FromStr for AtomRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("invalid atom ref: {s}"))
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
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
    fn atom_manifest_serde_round_trip() {
        let manifest = AtomManifest {
            description: "Reviews code for security and correctness.".into(),
            when_to_use: vec!["after writing code".into()],
            anti_patterns: vec!["one-line typo fixes".into()],
            inputs: Some(AtomInputSpec {
                schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "diff": { "type": "string" } },
                    "required": ["diff"]
                })),
                prompt_template: Some("Review: {{diff}}".into()),
            }),
            outputs: Some(AtomOutputSpec {
                schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "verdict": { "enum": ["approve", "revise", "reject"] } }
                })),
                evidence_density: Some(EvidenceDensity::High),
            }),
            effects: Some(AtomEffects {
                writes_files: Some(serde_json::json!(false)),
                dispatches_runs: Some(serde_json::json!(0)),
                max_depth: Some(serde_json::json!(0)),
                uses_network: Some(serde_json::json!(false)),
            }),
            composition: Some(AtomComposition {
                may_invoke_atoms: MayInvokeAtoms::None,
            }),
            implementation: AtomImplementation::Profile {
                brofile_ref: "brofile:rust-refactor-persona@v1".into(),
            },
            supervision: Some(AtomSupervisionPolicy {
                oracle: "default".into(),
                advisor: "on_alert".into(),
            }),
            trace: Some(AtomTracePolicy {
                retain: "summary".into(),
                portal_focus: "on_request".into(),
            }),
            cost_class: AtomCostClass::Normal,
            provenance: Some(AtomProvenance::HandAuthored {
                author: "user".into(),
                created_at: Some("2026-05-13T00:00:00Z".into()),
            }),
            embedding: None,
        };
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: AtomManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn atom_ref_pinned_round_trip() {
        let r = AtomRef::pinned("code-reviewer", 3);
        assert_eq!(r.render(), "atom:code-reviewer@v3");
        let parsed = AtomRef::parse(&r.render()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn atom_ref_latest_round_trip() {
        let r = AtomRef::latest("code-reviewer");
        assert_eq!(r.render(), "atom:code-reviewer@latest");
        let parsed = AtomRef::parse(&r.render()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn atom_ref_parse_rejects_missing_prefix() {
        assert!(AtomRef::parse("code-reviewer@v3").is_none());
    }

    #[test]
    fn atom_ref_parse_rejects_bare_name() {
        assert!(AtomRef::parse("atom:reviewer").is_none());
    }

    #[test]
    fn atom_ref_parse_rejects_bad_version() {
        assert!(AtomRef::parse("atom:reviewer@vabc").is_none());
    }

    #[test]
    fn atom_ref_parse_rejects_empty_name() {
        assert!(AtomRef::parse("atom:@v1").is_none());
    }

    #[test]
    fn atom_ref_parse_rejects_colon_in_name() {
        assert!(AtomRef::parse("atom:bad:name@v1").is_none());
    }

    #[test]
    fn atom_ref_parse_rejects_version_zero() {
        assert!(AtomRef::parse("atom:reviewer@v0").is_none());
    }

    #[test]
    fn atom_ref_from_str_works() {
        let r: AtomRef = "atom:foo@v1".parse().unwrap();
        assert_eq!(r.name, "foo");
        assert_eq!(r.version, AtomRefVersion::Pinned(1));
    }

    #[test]
    fn atom_ref_from_str_latest() {
        let r: AtomRef = "atom:foo@latest".parse().unwrap();
        assert_eq!(r.name, "foo");
        assert_eq!(r.version, AtomRefVersion::Latest);
    }

    #[test]
    fn atom_ref_from_str_rejects_invalid() {
        assert!("bad-ref".parse::<AtomRef>().is_err());
    }

    #[test]
    fn implementation_profile_serde() {
        let imp = AtomImplementation::Profile {
            brofile_ref: "brofile:reviewer@v1".into(),
        };
        let json = serde_json::to_string(&imp).unwrap();
        let parsed: AtomImplementation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, imp);
    }

    #[test]
    fn implementation_deterministic_serde() {
        let imp = AtomImplementation::Deterministic {
            runner: "refactor-plan-validate".into(),
        };
        let json = serde_json::to_string(&imp).unwrap();
        let parsed: AtomImplementation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, imp);
    }

    #[test]
    fn may_invoke_atoms_serde() {
        let none = MayInvokeAtoms::None;
        let json = serde_json::to_string(&none).unwrap();
        assert!(json.contains("\"kind\":\"none\""));
        let parsed: MayInvokeAtoms = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, none);

        let allowed = MayInvokeAtoms::Allowed {
            atoms: vec!["atom:x@v1".into()],
        };
        let json = serde_json::to_string(&allowed).unwrap();
        assert!(json.contains("\"kind\":\"allowed\""));
        let parsed: MayInvokeAtoms = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, allowed);
    }

    #[test]
    fn cost_class_display() {
        assert_eq!(AtomCostClass::Cheap.to_string(), "cheap");
        assert_eq!(AtomCostClass::Normal.to_string(), "normal");
        assert_eq!(AtomCostClass::Expensive.to_string(), "expensive");
    }

    #[test]
    fn validate_description_length_accepts_valid() {
        assert!(validate_description_length("This is a valid description.").is_ok());
    }

    #[test]
    fn validate_description_length_rejects_too_short() {
        let result = validate_description_length("short");
        assert!(result.is_err());
    }

    #[test]
    fn validate_when_to_use_nonempty_rejects_empty() {
        assert!(validate_when_to_use_nonempty(&[]).is_err());
    }

    #[test]
    fn validate_when_to_use_nonempty_accepts_nonempty() {
        assert!(validate_when_to_use_nonempty(&["after writing code".into()]).is_ok());
    }

    #[test]
    fn atom_artifact_serde_round_trip() {
        let artifact = AtomArtifact {
            _contract: "atom/v1".into(),
            kind: "atom".into(),
            name: "rust-refactor-plan".into(),
            version: serde_json::json!(1),
            subcontract: Some("refactor/v1".into()),
            supersedes: None,
            manifest: AtomManifest {
                description: "Plans and validates structural refactors.".into(),
                when_to_use: vec!["when refactoring Rust code".into()],
                anti_patterns: vec![],
                inputs: None,
                outputs: None,
                effects: Some(AtomEffects {
                    writes_files: Some(serde_json::json!(true)),
                    dispatches_runs: Some(serde_json::json!(0)),
                    max_depth: Some(serde_json::json!(0)),
                    uses_network: Some(serde_json::json!(false)),
                }),
                composition: Some(AtomComposition {
                    may_invoke_atoms: MayInvokeAtoms::None,
                }),
                implementation: AtomImplementation::Profile {
                    brofile_ref: "brofile:rust-refactor-persona@v1".into(),
                },
                supervision: None,
                trace: None,
                cost_class: AtomCostClass::Normal,
                provenance: None,
                embedding: None,
            },
        };
        let json = serde_json::to_string_pretty(&artifact).unwrap();
        let parsed: AtomArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, artifact);
    }
}
