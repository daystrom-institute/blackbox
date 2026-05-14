use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::TaskStore;
use super::brofile::BroConfig;
use super::providers::{self, Capability, ExecOpts, Provider};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PoolRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PinAuthority {
    #[default]
    Artifact,
    Operator,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RuntimePin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub authority: PinAuthority,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RuntimePreference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TierMode {
    #[default]
    Exact,
    AtLeast,
    Bounded,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RuntimeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_ladder: Option<String>,
    #[serde(default)]
    pub tier_mode: TierMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_capabilities: Vec<Capability>,
    #[serde(default)]
    pub durable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<RuntimePin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<RuntimePreference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderTierEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PoolConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_weights: BTreeMap<Provider, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_per_account: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionPolicy {
    Availability,
    Economy,
    Quality,
    Spread,
    Sticky,
    Deterministic,
    RoundRobin {
        #[serde(default)]
        tie_break: Vec<String>,
    },
    Score {
        #[serde(default)]
        score: BTreeMap<String, f64>,
        #[serde(default)]
        tie_break: Vec<String>,
    },
}

impl Default for SelectionPolicy {
    fn default() -> Self {
        Self::Availability
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AllocatorConfig {
    #[serde(default)]
    pub tiers: BTreeMap<String, BTreeMap<Provider, ProviderTierEntry>>,
    #[serde(default)]
    pub tier_ladders: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub pools: BTreeMap<String, PoolConfig>,
    #[serde(default)]
    pub selection_policies: BTreeMap<String, SelectionPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLease {
    pub task_id: String,
    pub session_id: String,
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default)]
    pub durable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub selection_trace_id: String,
    pub created_at: u64,
    pub last_seen_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeLeaseStore {
    #[serde(default)]
    pub leases: BTreeMap<String, RuntimeLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLane {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateTrace {
    pub lane: RuntimeLane,
    pub eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub score_components: BTreeMap<String, f64>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionTrace {
    pub id: String,
    pub request: RuntimeRequest,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_tiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<Capability>,
    pub candidates: Vec<CandidateTrace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<RuntimeLane>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Allocation {
    pub lane: RuntimeLane,
    pub trace: SelectionTrace,
}

#[derive(Debug, Clone)]
struct LaneId {
    provider: Provider,
    account: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AllocationContext {
    pub in_flight: BTreeMap<String, usize>,
}

static LEASE_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn config_file(store_dir: &Path) -> PathBuf {
    store_dir.join("allocator.json")
}

fn project_config_file(project_dir: &Path) -> PathBuf {
    project_dir.join(".bro").join("allocator.json")
}

fn allocator_dir(store_dir: &Path) -> PathBuf {
    store_dir.join("allocator")
}

fn leases_file(store_dir: &Path) -> PathBuf {
    allocator_dir(store_dir).join("leases.json")
}

fn traces_dir(store_dir: &Path) -> PathBuf {
    allocator_dir(store_dir).join("traces")
}

pub fn built_in_config() -> AllocatorConfig {
    use Provider::*;

    let mut tiers: BTreeMap<String, BTreeMap<Provider, ProviderTierEntry>> = BTreeMap::new();
    let mut tier =
        |name: &str, entries: Vec<(Provider, Option<&str>, Option<&str>, Option<f64>)>| {
            tiers.insert(
                name.to_string(),
                entries
                    .into_iter()
                    .map(|(provider, model, effort, weight)| {
                        (
                            provider,
                            ProviderTierEntry {
                                model: model.map(str::to_string),
                                effort: effort.map(str::to_string),
                                weight,
                            },
                        )
                    })
                    .collect(),
            );
        };

    tier(
        "economy",
        vec![
            (Claude, Some("claude-haiku-4-5-20251001"), Some("low"), None),
            (Codex, Some("gpt-5.3-codex-spark"), Some("low"), None),
            (Glm, Some("glm-4.5-air"), Some("low"), None),
            (Deepseek, Some("deepseek-v4-flash"), Some("low"), None),
            (Gemini, Some("gemini-3.1-flash-lite-preview"), None, None),
            (Vibe, None, None, None),
        ],
    );
    tier(
        "standard",
        vec![
            (Claude, Some("claude-sonnet-4-6"), Some("high"), None),
            (Codex, Some("gpt-5.5"), Some("medium"), None),
            (Glm, Some("glm-5-turbo"), Some("medium"), None),
            (Deepseek, Some("deepseek-v4-pro"), Some("medium"), None),
            (Inception, Some("inception/mercury-2"), Some("medium"), None),
            (Gemini, Some("gemini-3-flash-preview"), None, None),
            (Vibe, None, None, None),
        ],
    );
    tier(
        "premium",
        vec![
            (Claude, Some("claude-opus-4-7"), Some("xhigh"), None),
            (Codex, Some("gpt-5.5"), Some("high"), None),
            (Glm, Some("glm-5.1"), Some("high"), None),
            (Deepseek, Some("deepseek-v4-pro"), Some("high"), None),
            (Gemini, Some("gemini-3.1-pro-preview"), None, None),
        ],
    );
    tier(
        "super-el-cheapo-drones",
        vec![
            (Codex, Some("gpt-5.3-codex-spark"), Some("low"), Some(1.0)),
            (Glm, Some("glm-4.5-air"), Some("low"), Some(0.8)),
        ],
    );

    let mut pools = BTreeMap::new();
    pools.insert(
        "coding".into(),
        PoolConfig {
            providers: vec![Codex, Claude, Glm, Deepseek],
            provider_weights: provider_weights(&[
                (Glm, 1.0),
                (Claude, 0.82),
                (Codex, 0.68),
                (Deepseek, 0.55),
            ]),
            max_concurrent_per_account: Some(1),
        },
    );
    pools.insert(
        "any".into(),
        PoolConfig {
            providers: vec![Glm, Claude, Codex, Deepseek, Gemini, Inception, Vibe],
            provider_weights: provider_weights(&[
                (Glm, 1.0),
                (Claude, 0.82),
                (Codex, 0.68),
                (Deepseek, 0.55),
                (Gemini, 0.45),
                (Inception, 0.35),
                (Vibe, 0.25),
            ]),
            max_concurrent_per_account: Some(1),
        },
    );

    AllocatorConfig {
        tiers,
        tier_ladders: BTreeMap::from([(
            "coding-quality".into(),
            vec![
                "super-el-cheapo-drones".into(),
                "economy".into(),
                "standard".into(),
                "premium".into(),
            ],
        )]),
        pools,
        selection_policies: BTreeMap::from([
            ("availability".into(), SelectionPolicy::Availability),
            ("economy".into(), SelectionPolicy::Economy),
            ("quality".into(), SelectionPolicy::Quality),
            ("spread".into(), SelectionPolicy::Spread),
            ("sticky".into(), SelectionPolicy::Sticky),
            ("deterministic".into(), SelectionPolicy::Deterministic),
        ]),
    }
}

fn provider_weights(values: &[(Provider, f64)]) -> BTreeMap<Provider, f64> {
    values.iter().copied().collect()
}

pub fn load_effective_config(store_dir: &Path, project_dir: Option<&str>) -> AllocatorConfig {
    let mut cfg = built_in_config();
    if let Some(global) = load_config_file(&config_file(store_dir)) {
        cfg.merge(global);
    }
    if let Some(project_dir) = project_dir {
        if let Some(project) = load_config_file(&project_config_file(Path::new(project_dir))) {
            cfg.merge(project);
        }
    }
    cfg
}

fn load_config_file(path: &Path) -> Option<AllocatorConfig> {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

impl AllocatorConfig {
    fn merge(&mut self, other: AllocatorConfig) {
        for (tier, provider_map) in other.tiers {
            self.tiers.entry(tier).or_default().extend(provider_map);
        }
        self.tier_ladders.extend(other.tier_ladders);
        self.pools.extend(other.pools);
        self.selection_policies.extend(other.selection_policies);
    }
}

pub fn parse_capabilities(values: &[String]) -> Result<Vec<Capability>, String> {
    values
        .iter()
        .map(|value| {
            Capability::from_str(value).map_err(|_| format!("unknown capability tag: {value}"))
        })
        .collect()
}

pub fn allocation_context(task_store: &TaskStore, leases: &RuntimeLeaseStore) -> AllocationContext {
    let mut in_flight = BTreeMap::new();
    for task in task_store.all_tasks() {
        let inner = task.inner.lock();
        if !inner.status.is_terminal() {
            if let Some(lease) = leases.leases.get(&inner.id) {
                *in_flight
                    .entry(lane_key(lease.provider, lease.account.as_deref()))
                    .or_insert(0) += 1;
            }
        }
    }
    AllocationContext { in_flight }
}

pub fn allocate(
    request: RuntimeRequest,
    config: &AllocatorConfig,
    bro_config: &BroConfig,
    ctx: &AllocationContext,
) -> Allocation {
    let trace_id = format!("alloc-{}", uuid::Uuid::new_v4().simple());
    let mut trace = SelectionTrace {
        id: trace_id,
        request: request.clone(),
        candidate_tiers: candidate_tiers(&request, config).unwrap_or_default(),
        required_capabilities: required_capabilities(&request),
        candidates: Vec::new(),
        selected: None,
        error: None,
    };

    let candidate_tiers = match candidate_tiers(&request, config) {
        Ok(tiers) => tiers,
        Err(err) => {
            trace.error = Some(err);
            return Allocation {
                lane: fallback_lane(&request),
                trace,
            };
        }
    };
    trace.candidate_tiers = candidate_tiers.clone();

    let providers = resolve_provider_pool(&request, config);
    if providers.is_empty() {
        trace.error = Some("error.no_candidates: provider pool is empty after intersection".into());
        return Allocation {
            lane: fallback_lane(&request),
            trace,
        };
    }

    let required = required_capabilities(&request);
    trace.required_capabilities = required.clone();
    let mut eligible = Vec::new();
    let scoring_pool = effective_pool_config(&request, config);
    let selection_policy = resolve_selection_policy(&request, config);

    for provider in providers {
        let account_names = account_candidates(provider, request.pin.as_ref(), bro_config);
        for account in account_names {
            if candidate_tiers.is_empty() {
                let lane = untiered_lane(provider, account.clone(), request.pin.as_ref());
                push_candidate(
                    &mut trace,
                    lane,
                    &required,
                    bro_config,
                    ctx,
                    &scoring_pool,
                    &selection_policy,
                    &request,
                    &candidate_tiers,
                    None,
                    &mut eligible,
                );
            } else {
                for tier in &candidate_tiers {
                    let Some(entry) = config.tiers.get(tier).and_then(|m| m.get(&provider)) else {
                        continue;
                    };
                    let lane =
                        mapped_lane(provider, account.clone(), tier, entry, request.pin.as_ref());
                    push_candidate(
                        &mut trace,
                        lane,
                        &required,
                        bro_config,
                        ctx,
                        &scoring_pool,
                        &selection_policy,
                        &request,
                        &candidate_tiers,
                        Some(tier),
                        &mut eligible,
                    );
                }
            }
        }
    }

    eligible.sort_by(|a: &CandidateTrace, b: &CandidateTrace| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| lane_stable_key(&a.lane).cmp(&lane_stable_key(&b.lane)))
    });

    if let Some(best) = eligible.first() {
        trace.selected = Some(best.lane.clone());
        Allocation {
            lane: best.lane.clone(),
            trace,
        }
    } else {
        trace.error = Some("error.no_candidates: no lane satisfied hard eligibility".into());
        Allocation {
            lane: fallback_lane(&request),
            trace,
        }
    }
}

fn push_candidate(
    trace: &mut SelectionTrace,
    lane: RuntimeLane,
    required: &[Capability],
    bro_config: &BroConfig,
    ctx: &AllocationContext,
    scoring_pool: &PoolConfig,
    selection_policy: &SelectionPolicy,
    request: &RuntimeRequest,
    candidate_tiers: &[String],
    tier: Option<&String>,
    eligible: &mut Vec<CandidateTrace>,
) {
    let candidate = score_candidate(
        lane,
        required,
        bro_config,
        ctx,
        scoring_pool,
        selection_policy,
        request,
        candidate_tiers,
        tier,
    );
    if candidate.eligible {
        eligible.push(candidate.clone());
    }
    trace.candidates.push(candidate);
}

fn score_candidate(
    lane: RuntimeLane,
    required: &[Capability],
    bro_config: &BroConfig,
    ctx: &AllocationContext,
    scoring_pool: &PoolConfig,
    selection_policy: &SelectionPolicy,
    request: &RuntimeRequest,
    candidate_tiers: &[String],
    tier: Option<&String>,
) -> CandidateTrace {
    let mut candidate = CandidateTrace {
        lane: lane.clone(),
        eligible: false,
        exclusion_reason: None,
        score_components: BTreeMap::new(),
        score: 0.0,
    };

    if provider_binary_missing(lane.provider) {
        candidate.exclusion_reason = Some("provider_binary_missing".into());
        return candidate;
    }
    if let Some(account) = lane
        .account
        .as_deref()
        .and_then(|name| bro_config.accounts.get(name))
    {
        if account.disabled {
            candidate.exclusion_reason = Some("account_disabled".into());
            return candidate;
        }
        if let Some(tier) = tier {
            if !account.allowed_tiers.is_empty() && !account.allowed_tiers.iter().any(|t| t == tier)
            {
                candidate.exclusion_reason = Some("account_disallows_tier".into());
                return candidate;
            }
        }
        if let Some(model) = &lane.model {
            if !account.allowed_models.is_empty()
                && !account
                    .allowed_models
                    .iter()
                    .any(|allowed| allowed == model)
            {
                candidate.exclusion_reason = Some("account_disallows_model".into());
                return candidate;
            }
        }
    }

    let mut available_caps: HashSet<Capability> = lane.capabilities.iter().copied().collect();
    if let Some(account_caps) = lane
        .account
        .as_deref()
        .and_then(|name| bro_config.accounts.get(name))
        .map(|account| &account.capabilities)
        .filter(|caps| !caps.is_empty())
    {
        let account_caps: HashSet<Capability> = account_caps.iter().copied().collect();
        available_caps = available_caps
            .intersection(&account_caps)
            .copied()
            .collect();
    }
    if let Some(missing) = required.iter().find(|cap| !available_caps.contains(cap)) {
        candidate.exclusion_reason = Some(format!("missing_capability:{missing:?}"));
        return candidate;
    }

    let key = lane_key(lane.provider, lane.account.as_deref());
    let in_flight = *ctx.in_flight.get(&key).unwrap_or(&0);
    let max_concurrent = lane
        .account
        .as_deref()
        .and_then(|name| bro_config.accounts.get(name))
        .and_then(|account| account.max_concurrent)
        .or(scoring_pool.max_concurrent_per_account)
        .unwrap_or(usize::MAX);
    if in_flight >= max_concurrent {
        candidate.exclusion_reason = Some("max_concurrent_reached".into());
        return candidate;
    }

    let provider_preference = provider_weight(scoring_pool, lane.provider);
    let concurrency_capacity = if max_concurrent == usize::MAX {
        1.0
    } else {
        1.0 - (in_flight as f64 / max_concurrent as f64)
    }
    .clamp(0.0, 1.0);
    let tier_fit = if tier.is_some() { 1.0 } else { 0.8 };
    candidate
        .score_components
        .insert("provider_preference".into(), provider_preference);
    candidate
        .score_components
        .insert("quota_capacity".into(), 0.5);
    candidate
        .score_components
        .insert("concurrency_capacity".into(), concurrency_capacity);
    candidate
        .score_components
        .insert("cooldown_capacity".into(), 1.0);
    candidate
        .score_components
        .insert("tier_fit".into(), tier_fit);
    let policy_score = selection_policy_score(
        selection_policy,
        &lane,
        request,
        candidate_tiers,
        tier,
        provider_preference,
        concurrency_capacity,
    );
    candidate
        .score_components
        .insert("selection_policy".into(), policy_score);
    candidate.score = provider_preference * 0.5 * concurrency_capacity * tier_fit * policy_score;
    candidate.eligible = true;
    candidate
}

fn resolve_selection_policy(request: &RuntimeRequest, config: &AllocatorConfig) -> SelectionPolicy {
    let Some(value) = &request.selection_policy else {
        return SelectionPolicy::Availability;
    };
    if let Some(name) = value.as_str() {
        return config
            .selection_policies
            .get(name)
            .cloned()
            .unwrap_or(SelectionPolicy::Availability);
    }
    serde_json::from_value::<SelectionPolicy>(value.clone())
        .unwrap_or(SelectionPolicy::Availability)
}

fn selection_policy_score(
    policy: &SelectionPolicy,
    lane: &RuntimeLane,
    request: &RuntimeRequest,
    candidate_tiers: &[String],
    tier: Option<&String>,
    provider_preference: f64,
    concurrency_capacity: f64,
) -> f64 {
    match policy {
        SelectionPolicy::Availability => 1.0,
        SelectionPolicy::Economy => tier_position_score(candidate_tiers, tier, false),
        SelectionPolicy::Quality => tier_position_score(candidate_tiers, tier, true),
        SelectionPolicy::Spread => concurrency_capacity.max(0.05),
        SelectionPolicy::Sticky => request
            .prefer
            .as_ref()
            .and_then(|prefer| prefer.provider)
            .map(|provider| if provider == lane.provider { 1.5 } else { 0.75 })
            .unwrap_or(1.0),
        SelectionPolicy::Deterministic => stable_lane_score(lane),
        SelectionPolicy::RoundRobin { tie_break } => {
            stable_lane_score(lane) * tie_break_score(tie_break, lane.provider.as_str())
        }
        SelectionPolicy::Score { score, tie_break } => {
            score
                .get(lane.provider.as_str())
                .copied()
                .unwrap_or(provider_preference)
                * tie_break_score(tie_break, lane.provider.as_str())
        }
    }
}

fn tier_position_score(
    candidate_tiers: &[String],
    tier: Option<&String>,
    prefer_high: bool,
) -> f64 {
    let Some(tier) = tier else {
        return 1.0;
    };
    let Some(idx) = candidate_tiers
        .iter()
        .position(|candidate| candidate == tier)
    else {
        return 1.0;
    };
    if candidate_tiers.len() <= 1 {
        return 1.0;
    }
    let normalized = idx as f64 / (candidate_tiers.len() - 1) as f64;
    let quality = 0.75 + normalized * 0.5;
    if prefer_high { quality } else { 1.5 - quality }
}

fn stable_lane_score(lane: &RuntimeLane) -> f64 {
    let mut hash = 0u64;
    for byte in lane_stable_key(lane).bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
    }
    0.75 + (hash % 500) as f64 / 1000.0
}

fn tie_break_score(tie_break: &[String], provider: &str) -> f64 {
    tie_break
        .iter()
        .position(|value| value == provider)
        .map(|idx| 1.0 + ((tie_break.len() - idx) as f64 / tie_break.len().max(1) as f64) * 0.1)
        .unwrap_or(1.0)
}

fn provider_binary_missing(provider: Provider) -> bool {
    if matches!(provider, Provider::Workflow) {
        return true;
    }
    providers::resolve_bin(&provider.bin()).is_none()
}

fn candidate_tiers(
    request: &RuntimeRequest,
    config: &AllocatorConfig,
) -> Result<Vec<String>, String> {
    let Some(tier) = request.tier.as_deref() else {
        return Ok(Vec::new());
    };
    match request.tier_mode {
        TierMode::Exact => Ok(vec![tier.to_string()]),
        TierMode::AtLeast | TierMode::Bounded => {
            let ladder_name = request.tier_ladder.as_deref().ok_or_else(|| {
                "error.bad_allocation_request: tier_ladder is required for fallback tier modes"
                    .to_string()
            })?;
            let ladder = config.tier_ladders.get(ladder_name).ok_or_else(|| {
                format!("error.bad_allocation_request: unknown tier_ladder `{ladder_name}`")
            })?;
            let start = ladder
                .iter()
                .position(|value| value == tier)
                .ok_or_else(|| format!("error.bad_allocation_request: tier `{tier}` not present in ladder `{ladder_name}`"))?;
            if request.tier_mode == TierMode::AtLeast {
                return Ok(ladder[start..].to_vec());
            }
            let min = request.min_tier.as_deref().unwrap_or(tier);
            let max = request.max_tier.as_deref().unwrap_or(tier);
            let min_idx = ladder
                .iter()
                .position(|value| value == min)
                .ok_or_else(|| format!("error.bad_allocation_request: min_tier `{min}` not present in ladder `{ladder_name}`"))?;
            let max_idx = ladder
                .iter()
                .position(|value| value == max)
                .ok_or_else(|| format!("error.bad_allocation_request: max_tier `{max}` not present in ladder `{ladder_name}`"))?;
            if min_idx > max_idx {
                return Err("error.bad_allocation_request: min_tier sorts after max_tier".into());
            }
            Ok(ladder[min_idx..=max_idx].to_vec())
        }
    }
}

fn resolve_provider_pool(request: &RuntimeRequest, config: &AllocatorConfig) -> Vec<Provider> {
    let mut providers: HashSet<Provider> = Provider::ALL.iter().copied().collect();
    if let Some(pool) = &request.pool {
        if let Some(name) = pool.name.as_deref() {
            if let Some(cfg) = config.pools.get(name) {
                providers = providers
                    .intersection(&cfg.providers.iter().copied().collect())
                    .copied()
                    .collect();
            } else {
                providers.clear();
            }
        }
        if !pool.providers.is_empty() {
            providers = providers
                .intersection(&pool.providers.iter().copied().collect())
                .copied()
                .collect();
        }
    }
    if let Some(pin_provider) = request.pin.as_ref().and_then(|pin| pin.provider) {
        providers.retain(|provider| *provider == pin_provider);
    }
    let mut providers: Vec<_> = providers.into_iter().collect();
    providers.sort_by_key(|provider| provider.as_str());
    providers
}

fn effective_pool_config(request: &RuntimeRequest, config: &AllocatorConfig) -> PoolConfig {
    let mut pool = request
        .pool
        .as_ref()
        .and_then(|pool| pool.name.as_deref())
        .and_then(|name| config.pools.get(name))
        .cloned()
        .unwrap_or_default();
    if let Some(request_pool) = &request.pool {
        if !request_pool.providers.is_empty() {
            let requested: HashSet<Provider> = request_pool.providers.iter().copied().collect();
            pool.providers
                .retain(|provider| requested.contains(provider));
            pool.provider_weights
                .retain(|provider, _| requested.contains(provider));
            for provider in &request_pool.providers {
                pool.provider_weights.entry(*provider).or_insert(0.5);
            }
        }
    }
    pool
}

fn account_candidates(
    provider: Provider,
    pin: Option<&RuntimePin>,
    bro_config: &BroConfig,
) -> Vec<Option<String>> {
    if let Some(account) = pin.and_then(|pin| pin.account.clone()) {
        return vec![Some(account)];
    }
    let mut accounts = Vec::new();
    if let Some(default) = bro_config.provider_defaults.get(&provider) {
        accounts.push(Some(default.account.clone()));
    }
    for name in bro_config.accounts.keys() {
        accounts.push(Some(name.clone()));
    }
    accounts.push(None);
    accounts.sort();
    accounts.dedup();
    accounts
}

fn mapped_lane(
    provider: Provider,
    account: Option<String>,
    tier: &str,
    entry: &ProviderTierEntry,
    pin: Option<&RuntimePin>,
) -> RuntimeLane {
    let model = match pin.and_then(|pin| pin.model.clone()) {
        Some(model) if pin.is_some_and(|pin| pin.authority == PinAuthority::Operator) => {
            Some(model)
        }
        Some(model) if entry.model.as_deref() == Some(model.as_str()) => Some(model),
        Some(_) => entry.model.clone(),
        None => entry.model.clone(),
    };
    RuntimeLane {
        provider,
        account,
        tier: Some(tier.to_string()),
        model,
        effort: pin
            .and_then(|pin| pin.effort.clone())
            .or_else(|| entry.effort.clone()),
        capabilities: provider.capabilities().into_iter().collect(),
    }
}

fn untiered_lane(
    provider: Provider,
    account: Option<String>,
    pin: Option<&RuntimePin>,
) -> RuntimeLane {
    RuntimeLane {
        provider,
        account,
        tier: None,
        model: pin
            .and_then(|pin| pin.model.clone())
            .or_else(|| default_model(provider).map(str::to_string)),
        effort: pin.and_then(|pin| pin.effort.clone()),
        capabilities: provider.capabilities().into_iter().collect(),
    }
}

fn fallback_lane(request: &RuntimeRequest) -> RuntimeLane {
    let provider = request
        .pin
        .as_ref()
        .and_then(|pin| pin.provider)
        .or_else(|| {
            request
                .pool
                .as_ref()
                .and_then(|pool| pool.providers.first().copied())
        })
        .unwrap_or(Provider::Codex);
    untiered_lane(provider, None, request.pin.as_ref())
}

fn default_model(provider: Provider) -> Option<&'static str> {
    provider
        .models()
        .iter()
        .find(|model| model.default)
        .map(|model| model.id)
}

fn required_capabilities(request: &RuntimeRequest) -> Vec<Capability> {
    let mut set = HashSet::new();
    for cap in request
        .capabilities
        .iter()
        .chain(request.derived_capabilities.iter())
    {
        set.insert(*cap);
    }
    if request.durable {
        set.insert(Capability::Resume);
    }
    let mut caps: Vec<_> = set.into_iter().collect();
    caps.sort_by_key(|cap| format!("{cap:?}"));
    caps
}

fn lane_key(provider: Provider, account: Option<&str>) -> String {
    format!("{}:{}", provider.as_str(), account.unwrap_or("default"))
}

fn lane_stable_key(lane: &RuntimeLane) -> String {
    format!(
        "{}:{}:{}:{}",
        lane.provider.as_str(),
        lane.account.as_deref().unwrap_or("default"),
        lane.model.as_deref().unwrap_or(""),
        lane.effort.as_deref().unwrap_or("")
    )
}

fn provider_weight(pool: &PoolConfig, provider: Provider) -> f64 {
    pool.provider_weights.get(&provider).copied().unwrap_or(0.5)
}

pub fn lease_store_load(store_dir: &Path) -> RuntimeLeaseStore {
    fs::read_to_string(leases_file(store_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn lease_store_save(store_dir: &Path, store: &RuntimeLeaseStore) {
    write_json_atomic(&leases_file(store_dir), store);
}

pub fn record_lease(store_dir: &Path, lease: RuntimeLease) {
    let _guard = LEASE_STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = lease_store_load(store_dir);
    store.leases.insert(lease.task_id.clone(), lease);
    lease_store_save(store_dir, &store);
}

pub fn lookup_lease_for_session(
    store_dir: &Path,
    task_store: &TaskStore,
    provider: Provider,
    session_id: &str,
) -> Option<RuntimeLease> {
    let leases = lease_store_load(store_dir);
    task_store
        .all_tasks()
        .into_iter()
        .filter_map(|task| {
            let inner = task.inner.lock();
            (inner.provider == provider
                && inner.session_id == session_id
                && !inner.status.is_terminal())
            .then(|| {
                leases
                    .leases
                    .get(&inner.id)
                    .filter(|lease| lease.durable)
                    .cloned()
                    .map(|lease| (inner.started_at, lease))
            })
            .flatten()
        })
        .max_by_key(|(started_at, _)| *started_at)
        .map(|(_, lease)| lease)
}

pub fn save_trace(store_dir: &Path, trace: &SelectionTrace) {
    let path = traces_dir(store_dir).join(format!("{}.json", trace.id));
    write_json_atomic(&path, trace);
}

pub fn load_trace(store_dir: &Path, selection_trace_id: &str) -> Option<SelectionTrace> {
    if !valid_trace_id(selection_trace_id) {
        return None;
    }
    let path = traces_dir(store_dir).join(format!("{selection_trace_id}.json"));
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn valid_trace_id(selection_trace_id: &str) -> bool {
    selection_trace_id
        .strip_prefix("alloc-")
        .is_some_and(|suffix| suffix.len() == 32 && suffix.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(data) = serde_json::to_string_pretty(value) {
        if let Ok(mut file) = fs::File::create(&tmp) {
            let _ = file.write_all(data.as_bytes());
            let _ = file.sync_all();
            let _ = fs::rename(tmp, path);
        }
    }
}

pub fn lease_from_allocation(
    task_id: String,
    session_id: String,
    allocation: &Allocation,
    project_dir: Option<String>,
    cwd: Option<String>,
) -> RuntimeLease {
    RuntimeLease {
        task_id,
        session_id,
        provider: allocation.lane.provider,
        account: allocation.lane.account.clone(),
        model: allocation.lane.model.clone(),
        effort: allocation.lane.effort.clone(),
        tier: allocation.lane.tier.clone(),
        durable: allocation.trace.request.durable,
        capabilities: allocation.lane.capabilities.clone(),
        project_dir,
        cwd,
        selection_trace_id: allocation.trace.id.clone(),
        created_at: super::now_ms(),
        last_seen_at: super::now_ms(),
    }
}

pub fn exec_opts_for_lane(lane: &RuntimeLane) -> Option<ExecOpts> {
    (lane.model.is_some() || lane.effort.is_some()).then(|| ExecOpts {
        model: lane.model.clone(),
        effort: lane.effort.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_provider_bins<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::util::test_env_lock();
        let keys = [
            "CLAUDE_BIN",
            "CODEX_BIN",
            "GEMINI_BIN",
            "VIBE_BIN",
            "OPENCODE_BIN",
            "COPILOT_BIN",
        ];
        let prior: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            unsafe {
                std::env::set_var(key, "sh");
            }
        }
        let result = f();
        for (key, value) in prior {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        result
    }

    #[test]
    fn built_in_tier_mappings_use_live_model_slugs() {
        let cfg = built_in_config();
        for (tier, mappings) in &cfg.tiers {
            for (provider, entry) in mappings {
                let Some(model) = entry.model.as_deref() else {
                    continue;
                };
                assert!(
                    provider.models().iter().any(|info| info.id == model),
                    "{tier}.{provider} maps unknown model `{model}`"
                );
            }
        }
    }

    #[test]
    fn built_in_tier_mappings_use_valid_efforts() {
        let cfg = built_in_config();
        for (tier, mappings) in &cfg.tiers {
            for (provider, entry) in mappings {
                let Some(effort) = entry.effort.as_deref() else {
                    continue;
                };
                assert!(
                    provider.efforts().iter().any(|info| info.id == effort),
                    "{tier}.{provider} maps unknown effort `{effort}`"
                );
            }
        }
    }

    #[test]
    fn allocation_intersects_named_pool_and_provider_filter() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("coding".into()),
                    providers: vec![Provider::Codex],
                }),
                durable: true,
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Codex);
            assert_eq!(allocation.lane.model.as_deref(), Some("gpt-5.5"));
            assert_eq!(allocation.lane.effort.as_deref(), Some("medium"));
        });
    }

    #[test]
    fn allocation_fails_closed_on_missing_capability() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: None,
                    providers: vec![Provider::Gemini],
                }),
                capabilities: vec![Capability::StructuredOutput],
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                },
            );
            assert!(
                allocation
                    .trace
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("no lane satisfied")),
                "{:?}",
                allocation.trace.error
            );
            assert!(allocation.trace.candidates.iter().all(|c| !c.eligible));
        });
    }

    #[test]
    fn selection_policy_scores_participate_in_ranking() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("coding".into()),
                    providers: vec![Provider::Glm, Provider::Codex],
                }),
                selection_policy: Some(serde_json::json!({
                    "kind": "score",
                    "score": {
                        "codex": 10.0,
                        "glm": 1.0
                    }
                })),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Codex);
            assert!(
                allocation
                    .trace
                    .candidates
                    .iter()
                    .any(|candidate| candidate.score_components.contains_key("selection_policy"))
            );
        });
    }

    #[test]
    fn fallback_modes_require_a_named_ladder() {
        let cfg = built_in_config();
        let request = RuntimeRequest {
            tier: Some("economy".into()),
            tier_mode: TierMode::AtLeast,
            ..Default::default()
        };
        let allocation = allocate(
            request,
            &cfg,
            &BroConfig::default(),
            &AllocationContext {
                in_flight: BTreeMap::new(),
            },
        );
        assert!(
            allocation
                .trace
                .error
                .as_deref()
                .is_some_and(|err| err.contains("tier_ladder is required")),
            "{:?}",
            allocation.trace.error
        );
    }

    #[test]
    fn requested_pool_supplies_concurrency_policy() {
        with_provider_bins(|| {
            let mut cfg = built_in_config();
            cfg.pools.insert(
                "loose".into(),
                PoolConfig {
                    providers: vec![Provider::Codex],
                    provider_weights: BTreeMap::from([(Provider::Codex, 1.0)]),
                    max_concurrent_per_account: None,
                },
            );
            cfg.pools.insert(
                "strict".into(),
                PoolConfig {
                    providers: vec![Provider::Codex],
                    provider_weights: BTreeMap::from([(Provider::Codex, 1.0)]),
                    max_concurrent_per_account: Some(1),
                },
            );
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("strict".into()),
                    providers: Vec::new(),
                }),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::from([(lane_key(Provider::Codex, None), 1)]),
                },
            );
            assert!(
                allocation
                    .trace
                    .candidates
                    .iter()
                    .any(|candidate| candidate.exclusion_reason.as_deref()
                        == Some("max_concurrent_reached")),
                "{:?}",
                allocation.trace.candidates
            );
            assert!(
                allocation
                    .trace
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("no lane satisfied")),
                "{:?}",
                allocation.trace.error
            );
        });
    }

    #[test]
    fn trace_lookup_rejects_non_allocator_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let trace = SelectionTrace {
            id: "alloc-0123456789abcdef0123456789abcdef".into(),
            request: RuntimeRequest::default(),
            candidate_tiers: Vec::new(),
            required_capabilities: Vec::new(),
            candidates: Vec::new(),
            selected: None,
            error: Some("test".into()),
        };
        save_trace(tmp.path(), &trace);
        assert!(load_trace(tmp.path(), &trace.id).is_some());
        assert!(load_trace(tmp.path(), "../leases").is_none());
        assert!(load_trace(tmp.path(), "alloc-../../leases").is_none());
        assert!(load_trace(tmp.path(), "not-a-trace-id").is_none());
    }
}
