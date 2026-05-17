use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// AtomArtifact — top-level envelope for installed atom files
// ---------------------------------------------------------------------------

// kept: public atom artifact envelope type; deserialized via serde from installed atom files
#[allow(dead_code)]
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
    pub runtime: Option<crate::orchestration::allocator::RuntimeRequest>,

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

// ---------------------------------------------------------------------------
// Runtime supervision plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SupervisionPlan {
    pub classifier: SupervisionClassifierPlan,
    pub advisor: SupervisionAdvisorPlan,
    pub recovery: SupervisionRecoveryPlan,
    pub trigger: SupervisionTriggerPlan,
    pub tail_policy: SupervisionTailPolicy,
    pub alert_dedup: SupervisionAlertDedupPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisionClassifierPlan {
    pub mode: SupervisionClassifierMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atom_ref: Option<AtomRef>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_polls: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerting_classifications: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupervisionRuntimeIntent>,
}

impl Default for SupervisionClassifierPlan {
    fn default() -> Self {
        Self {
            mode: SupervisionClassifierMode::None,
            atom_ref: None,
            required: false,
            cadence_ms: None,
            max_polls: None,
            alerting_classifications: default_alerting_classifications(),
            runtime: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionClassifierMode {
    None,
    OnMechanicalAlert,
    Cadence,
    CadenceOrAlert,
    TurnEndOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisionAdvisorPlan {
    pub mode: SupervisionAdvisorMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atom_ref: Option<AtomRef>,
    #[serde(default = "default_advisor_durable")]
    pub durable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupervisionRuntimeIntent>,
}

impl Default for SupervisionAdvisorPlan {
    fn default() -> Self {
        Self {
            mode: SupervisionAdvisorMode::None,
            atom_ref: None,
            durable: default_advisor_durable(),
            runtime: None,
        }
    }
}

fn default_advisor_durable() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionAdvisorMode {
    None,
    OnAlert,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisionRecoveryPlan {
    #[serde(default = "default_recovery_max_attempts")]
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_ladder: Option<String>,
    #[serde(default)]
    pub tier_mode: SupervisionTierMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupervisionRuntimeIntent>,
}

impl Default for SupervisionRecoveryPlan {
    fn default() -> Self {
        Self {
            max_attempts: default_recovery_max_attempts(),
            tier_ladder: None,
            tier_mode: SupervisionTierMode::Exact,
            runtime: None,
        }
    }
}

fn default_recovery_max_attempts() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionTierMode {
    #[default]
    Exact,
    AtLeast,
    Bounded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisionTriggerPlan {
    #[serde(default)]
    pub mechanical_alerts: bool,
    #[serde(default)]
    pub turn_end: bool,
}

impl Default for SupervisionTriggerPlan {
    fn default() -> Self {
        Self {
            mechanical_alerts: true,
            turn_end: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisionTailPolicy {
    #[serde(default = "default_tail_events")]
    pub events: u32,
    #[serde(default = "default_tail_notes")]
    pub notes: u32,
    #[serde(default = "default_tail_assistant_bytes")]
    pub assistant_bytes: u32,
    #[serde(default = "default_tail_reports")]
    pub reports: u32,
}

impl Default for SupervisionTailPolicy {
    fn default() -> Self {
        Self {
            events: default_tail_events(),
            notes: default_tail_notes(),
            assistant_bytes: default_tail_assistant_bytes(),
            reports: default_tail_reports(),
        }
    }
}

fn default_tail_events() -> u32 {
    20
}
fn default_tail_notes() -> u32 {
    20
}
fn default_tail_assistant_bytes() -> u32 {
    4000
}
fn default_tail_reports() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisionAlertDedupPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_alert_dedup_window_ms")]
    pub window_ms: u64,
}

impl Default for SupervisionAlertDedupPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            window_ms: default_alert_dedup_window_ms(),
        }
    }
}

fn default_alert_dedup_window_ms() -> u64 {
    60_000
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionRuntimeIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_ladder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_mode: Option<SupervisionTierMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionPlanOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<SupervisionClassifierOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<SupervisionAdvisorOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<SupervisionRecoveryOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<SupervisionTriggerOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_policy: Option<SupervisionTailPolicyOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_dedup: Option<SupervisionAlertDedupOverride>,
}

impl SupervisionPlanOverride {
    pub fn validate_shape(&self) -> Result<(), String> {
        if let Some(classifier) = &self.classifier {
            if let Some(atom_ref) = &classifier.atom_ref {
                parse_supervision_atom_ref(atom_ref, "classifier.atom_ref")?;
            }
            if classifier.mode == Some(SupervisionClassifierMode::None)
                && classifier.atom_ref.is_some()
            {
                return Err("classifier.atom_ref is invalid when classifier.mode=none".into());
            }
            if classifier.cadence_ms == Some(0) {
                return Err("classifier.cadence_ms must be greater than zero".into());
            }
        }
        if let Some(advisor) = &self.advisor {
            if let Some(atom_ref) = &advisor.atom_ref {
                parse_supervision_atom_ref(atom_ref, "advisor.atom_ref")?;
            }
            if advisor.mode == Some(SupervisionAdvisorMode::None) && advisor.atom_ref.is_some() {
                return Err("advisor.atom_ref is invalid when advisor.mode=none".into());
            }
        }
        if let Some(recovery) = &self.recovery {
            if recovery.max_attempts == Some(0) {
                return Err("recovery.max_attempts must be greater than zero".into());
            }
            if recovery
                .tier_ladder
                .as_deref()
                .is_some_and(|ladder| ladder.trim().is_empty())
            {
                return Err("recovery.tier_ladder must not be empty".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionClassifierOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SupervisionClassifierMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atom_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_polls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alerting_classifications: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupervisionRuntimeIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionAdvisorOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SupervisionAdvisorMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atom_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupervisionRuntimeIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionRecoveryOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_ladder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_mode: Option<SupervisionTierMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupervisionRuntimeIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionTriggerOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanical_alerts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_end: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionTailPolicyOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reports: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupervisionAlertDedupOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SupervisionPlanDefaults {
    pub default_classifier_atom: Option<AtomRef>,
    pub default_classifier_mode: Option<SupervisionClassifierMode>,
    pub default_advisor_atom: Option<AtomRef>,
}

impl SupervisionPlan {
    pub fn normalize(
        manifest: Option<&AtomSupervisionPolicy>,
        binding_override: Option<&SupervisionPlanOverride>,
        caller_override: Option<&SupervisionPlanOverride>,
        defaults: &SupervisionPlanDefaults,
    ) -> Result<Self, String> {
        let mut plan = SupervisionPlan::default();
        if let Some(manifest) = manifest {
            plan.apply_manifest_policy(manifest, defaults)?;
        }
        if let Some(override_policy) = binding_override {
            plan.apply_override(override_policy)?;
        }
        if let Some(override_policy) = caller_override {
            plan.apply_override(override_policy)?;
        }
        plan.validate()?;
        Ok(plan)
    }

    fn apply_manifest_policy(
        &mut self,
        policy: &AtomSupervisionPolicy,
        defaults: &SupervisionPlanDefaults,
    ) -> Result<(), String> {
        match policy.oracle.as_str() {
            "none" => {
                self.classifier.mode = SupervisionClassifierMode::None;
                self.classifier.atom_ref = None;
            }
            "default" => {
                self.classifier.mode = defaults
                    .default_classifier_mode
                    .unwrap_or(SupervisionClassifierMode::CadenceOrAlert);
                self.classifier.atom_ref =
                    Some(defaults.default_classifier_atom.clone().ok_or_else(|| {
                        "supervision.oracle=default requires a configured default classifier atom"
                            .to_string()
                    })?);
            }
            raw => {
                self.classifier.mode = SupervisionClassifierMode::CadenceOrAlert;
                self.classifier.atom_ref = Some(parse_supervision_atom_ref(raw, "oracle")?);
            }
        }

        match policy.advisor.as_str() {
            "none" => {
                self.advisor.mode = SupervisionAdvisorMode::None;
                self.advisor.atom_ref = None;
            }
            "on_alert" => {
                self.advisor.mode = SupervisionAdvisorMode::OnAlert;
                self.advisor.atom_ref = defaults.default_advisor_atom.clone();
            }
            "always" => {
                self.advisor.mode = SupervisionAdvisorMode::Always;
                self.advisor.atom_ref = defaults.default_advisor_atom.clone();
            }
            raw => {
                self.advisor.mode = SupervisionAdvisorMode::OnAlert;
                self.advisor.atom_ref = Some(parse_supervision_atom_ref(raw, "advisor")?);
            }
        }
        Ok(())
    }

    pub fn apply_override(
        &mut self,
        override_policy: &SupervisionPlanOverride,
    ) -> Result<(), String> {
        if let Some(classifier) = &override_policy.classifier {
            if let Some(mode) = classifier.mode {
                self.classifier.mode = mode;
                if mode == SupervisionClassifierMode::None {
                    self.classifier.atom_ref = None;
                }
            }
            if let Some(atom_ref) = &classifier.atom_ref {
                self.classifier.atom_ref =
                    Some(parse_supervision_atom_ref(atom_ref, "classifier.atom_ref")?);
            }
            if let Some(required) = classifier.required {
                self.classifier.required = required;
            }
            if let Some(cadence_ms) = classifier.cadence_ms {
                self.classifier.cadence_ms = Some(cadence_ms);
            }
            if let Some(max_polls) = classifier.max_polls {
                self.classifier.max_polls = Some(max_polls);
            }
            if let Some(alerting) = &classifier.alerting_classifications {
                self.classifier.alerting_classifications = alerting.clone();
            }
            if let Some(runtime) = &classifier.runtime {
                let mut runtime = runtime.clone();
                ensure_runtime_capability(&mut runtime, "structured_output");
                self.classifier.runtime = Some(runtime);
            }
        }

        if let Some(advisor) = &override_policy.advisor {
            if let Some(mode) = advisor.mode {
                self.advisor.mode = mode;
                if mode == SupervisionAdvisorMode::None {
                    self.advisor.atom_ref = None;
                }
            }
            if let Some(atom_ref) = &advisor.atom_ref {
                self.advisor.atom_ref =
                    Some(parse_supervision_atom_ref(atom_ref, "advisor.atom_ref")?);
            }
            if let Some(durable) = advisor.durable {
                self.advisor.durable = durable;
            }
            if let Some(runtime) = &advisor.runtime {
                let mut runtime = runtime.clone();
                ensure_runtime_capability(&mut runtime, "structured_output");
                self.advisor.runtime = Some(runtime);
            }
        }

        if let Some(recovery) = &override_policy.recovery {
            if let Some(max_attempts) = recovery.max_attempts {
                self.recovery.max_attempts = max_attempts;
            }
            if let Some(tier_ladder) = &recovery.tier_ladder {
                self.recovery.tier_ladder = Some(tier_ladder.clone());
            }
            if let Some(tier_mode) = recovery.tier_mode {
                self.recovery.tier_mode = tier_mode;
            }
            if let Some(runtime) = &recovery.runtime {
                self.recovery.runtime = Some(runtime.clone());
            }
        }

        if let Some(trigger) = &override_policy.trigger {
            if let Some(mechanical_alerts) = trigger.mechanical_alerts {
                self.trigger.mechanical_alerts = mechanical_alerts;
            }
            if let Some(turn_end) = trigger.turn_end {
                self.trigger.turn_end = turn_end;
            }
        }

        if let Some(tail) = &override_policy.tail_policy {
            if let Some(events) = tail.events {
                self.tail_policy.events = events;
            }
            if let Some(notes) = tail.notes {
                self.tail_policy.notes = notes;
            }
            if let Some(assistant_bytes) = tail.assistant_bytes {
                self.tail_policy.assistant_bytes = assistant_bytes;
            }
            if let Some(reports) = tail.reports {
                self.tail_policy.reports = reports;
            }
        }

        if let Some(alert_dedup) = &override_policy.alert_dedup {
            if let Some(enabled) = alert_dedup.enabled {
                self.alert_dedup.enabled = enabled;
            }
            if let Some(window_ms) = alert_dedup.window_ms {
                self.alert_dedup.window_ms = window_ms;
            }
        }

        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.classifier.mode != SupervisionClassifierMode::None
            && self.classifier.atom_ref.is_none()
        {
            return Err("enabled classifier supervision requires classifier.atom_ref".into());
        }
        if self.classifier.mode == SupervisionClassifierMode::None
            && self.classifier.atom_ref.is_some()
        {
            return Err("classifier.atom_ref is invalid when classifier.mode=none".into());
        }
        if matches!(
            self.classifier.mode,
            SupervisionClassifierMode::Cadence | SupervisionClassifierMode::CadenceOrAlert
        ) && self.classifier.cadence_ms == Some(0)
        {
            return Err("classifier.cadence_ms must be greater than zero".into());
        }
        if self.advisor.mode != SupervisionAdvisorMode::None && self.advisor.atom_ref.is_none() {
            return Err("enabled advisor supervision requires advisor.atom_ref".into());
        }
        if self.advisor.mode == SupervisionAdvisorMode::None && self.advisor.atom_ref.is_some() {
            return Err("advisor.atom_ref is invalid when advisor.mode=none".into());
        }
        if self.recovery.max_attempts == 0 {
            return Err("recovery.max_attempts must be greater than zero".into());
        }
        if matches!(
            self.recovery.tier_mode,
            SupervisionTierMode::AtLeast | SupervisionTierMode::Bounded
        ) && self
            .recovery
            .tier_ladder
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Err("recovery tier_mode at_least/bounded requires tier_ladder".into());
        }
        validate_runtime_intent("classifier.runtime", self.classifier.runtime.as_ref())?;
        validate_runtime_intent("advisor.runtime", self.advisor.runtime.as_ref())?;
        validate_runtime_intent("recovery.runtime", self.recovery.runtime.as_ref())?;
        Ok(())
    }
}

fn ensure_runtime_capability(runtime: &mut SupervisionRuntimeIntent, capability: &str) {
    if !runtime
        .capabilities
        .iter()
        .any(|existing| existing == capability)
    {
        runtime.capabilities.push(capability.to_string());
    }
}

fn validate_runtime_intent(
    field: &str,
    runtime: Option<&SupervisionRuntimeIntent>,
) -> Result<(), String> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    if runtime
        .tier
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(format!("{field}.tier must not be empty"));
    }
    if runtime
        .tier_ladder
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(format!("{field}.tier_ladder must not be empty"));
    }
    if matches!(
        runtime.tier_mode,
        Some(SupervisionTierMode::AtLeast | SupervisionTierMode::Bounded)
    ) && runtime.tier_ladder.is_none()
    {
        return Err(format!(
            "{field}.tier_mode at_least/bounded requires tier_ladder"
        ));
    }
    for capability in &runtime.capabilities {
        if capability.trim().is_empty() {
            return Err(format!(
                "{field}.capabilities must not contain empty values"
            ));
        }
    }
    Ok(())
}

fn parse_supervision_atom_ref(raw: &str, field: &str) -> Result<AtomRef, String> {
    AtomRef::parse(raw)
        .ok_or_else(|| format!("supervision.{field} must be a typed atom ref (atom:name@vN)"))
}

fn default_alerting_classifications() -> Vec<String> {
    [
        "maybe_stuck",
        "maybe_scope_drift",
        "maybe_unsupported_claims",
        "maybe_tool_misuse",
        "needs_advisor",
        "classifier_failed",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
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

    // kept: public AtomRef constructor mirroring `pinned`; used by tests and external callers
    #[allow(dead_code)]
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
            runtime: None,
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
    fn supervision_manifest_defaults_normalize_disabled() {
        let plan =
            SupervisionPlan::normalize(None, None, None, &SupervisionPlanDefaults::default())
                .unwrap();

        assert_eq!(plan.classifier.mode, SupervisionClassifierMode::None);
        assert_eq!(plan.classifier.atom_ref, None);
        assert_eq!(plan.advisor.mode, SupervisionAdvisorMode::None);
        assert_eq!(plan.advisor.atom_ref, None);
    }

    #[test]
    fn supervision_oracle_default_resolves_configured_classifier() {
        let defaults = SupervisionPlanDefaults {
            default_classifier_atom: Some(AtomRef::pinned("behavior-classifier", 1)),
            default_classifier_mode: Some(SupervisionClassifierMode::OnMechanicalAlert),
            default_advisor_atom: None,
        };
        let manifest = AtomSupervisionPolicy {
            oracle: "default".into(),
            advisor: "none".into(),
        };

        let plan = SupervisionPlan::normalize(Some(&manifest), None, None, &defaults).unwrap();

        assert_eq!(
            plan.classifier.atom_ref,
            Some(AtomRef::pinned("behavior-classifier", 1))
        );
        assert_eq!(
            plan.classifier.mode,
            SupervisionClassifierMode::OnMechanicalAlert
        );
    }

    #[test]
    fn supervision_binding_override_can_disable_classifier() {
        let defaults = SupervisionPlanDefaults {
            default_classifier_atom: Some(AtomRef::pinned("behavior-classifier", 1)),
            default_classifier_mode: Some(SupervisionClassifierMode::CadenceOrAlert),
            default_advisor_atom: None,
        };
        let manifest = AtomSupervisionPolicy {
            oracle: "default".into(),
            advisor: "none".into(),
        };
        let override_policy = SupervisionPlanOverride {
            classifier: Some(SupervisionClassifierOverride {
                mode: Some(SupervisionClassifierMode::None),
                ..Default::default()
            }),
            ..Default::default()
        };

        let plan =
            SupervisionPlan::normalize(Some(&manifest), Some(&override_policy), None, &defaults)
                .unwrap();

        assert_eq!(plan.classifier.mode, SupervisionClassifierMode::None);
        assert_eq!(plan.classifier.atom_ref, None);
    }

    #[test]
    fn supervision_alerting_classifications_are_preserved() {
        let override_policy = SupervisionPlanOverride {
            classifier: Some(SupervisionClassifierOverride {
                mode: Some(SupervisionClassifierMode::Cadence),
                atom_ref: Some("atom:behavior-classifier@v1".into()),
                alerting_classifications: Some(vec![
                    "maybe_scope_drift".into(),
                    "needs_advisor".into(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let plan = SupervisionPlan::normalize(
            None,
            Some(&override_policy),
            None,
            &SupervisionPlanDefaults::default(),
        )
        .unwrap();

        assert_eq!(
            plan.classifier.alerting_classifications,
            vec!["maybe_scope_drift", "needs_advisor"]
        );
    }

    #[test]
    fn supervision_runtime_intent_derives_structured_output_for_judges() {
        let override_policy = SupervisionPlanOverride {
            classifier: Some(SupervisionClassifierOverride {
                mode: Some(SupervisionClassifierMode::Cadence),
                atom_ref: Some("atom:supervision-classifier@v1".into()),
                runtime: Some(SupervisionRuntimeIntent {
                    tier: Some("economy".into()),
                    ..SupervisionRuntimeIntent::default()
                }),
                ..Default::default()
            }),
            advisor: Some(SupervisionAdvisorOverride {
                mode: Some(SupervisionAdvisorMode::Always),
                atom_ref: Some("atom:supervision-advisor@v1".into()),
                runtime: Some(SupervisionRuntimeIntent {
                    tier: Some("standard".into()),
                    ..SupervisionRuntimeIntent::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let plan = SupervisionPlan::normalize(
            None,
            Some(&override_policy),
            None,
            &SupervisionPlanDefaults::default(),
        )
        .unwrap();
        assert!(
            plan.classifier
                .runtime
                .unwrap()
                .capabilities
                .contains(&"structured_output".to_string())
        );
        assert!(
            plan.advisor
                .runtime
                .unwrap()
                .capabilities
                .contains(&"structured_output".to_string())
        );
    }

    #[test]
    fn supervision_runtime_intent_fails_closed_without_ladder_for_bounded() {
        let override_policy = SupervisionPlanOverride {
            classifier: Some(SupervisionClassifierOverride {
                mode: Some(SupervisionClassifierMode::Cadence),
                atom_ref: Some("atom:supervision-classifier@v1".into()),
                runtime: Some(SupervisionRuntimeIntent {
                    tier_mode: Some(SupervisionTierMode::Bounded),
                    ..SupervisionRuntimeIntent::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = SupervisionPlan::normalize(
            None,
            Some(&override_policy),
            None,
            &SupervisionPlanDefaults::default(),
        )
        .unwrap_err();
        assert!(err.contains("requires tier_ladder"), "{err}");
    }

    #[test]
    fn supervision_malformed_override_fails_validation() {
        let override_policy = SupervisionPlanOverride {
            classifier: Some(SupervisionClassifierOverride {
                atom_ref: Some("behavior-classifier".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = override_policy.validate_shape().unwrap_err();
        assert!(err.contains("typed atom ref"), "{err}");
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
                runtime: None,
                provenance: None,
                embedding: None,
            },
        };
        let json = serde_json::to_string_pretty(&artifact).unwrap();
        let parsed: AtomArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, artifact);
    }
}
