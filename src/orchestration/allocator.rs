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
use super::executor::ProviderBinaryLocation;
use super::providers::dispatch_prelude::*;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionPolicy {
    #[default]
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
    /// Brofile context-assembly policy at the time of original
    /// dispatch. Carried so raw `bro_resume(session_id, provider)`
    /// can re-enforce suppression intent against the runtime provider
    /// without the caller re-supplying the brofile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brofile_context: Option<crate::orchestration::brofile::BrofileContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeLeaseStore {
    #[serde(default)]
    pub leases: BTreeMap<String, RuntimeLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Present,
    Missing,
    Expired,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Known,
    Exhausted,
    ProbeFailed,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaConfidence {
    QuotaProbe,
    RuntimeRateLimit,
    PaygBalance,
    ActiveAcceptance,
    CredentialOnly,
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRecord {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default)]
    pub credential_status: CredentialStatus,
    #[serde(default)]
    pub quota_status: QuotaStatus,
    #[serde(default)]
    pub quota_confidence: QuotaConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_utilization: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day_utilization: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_capacity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_runtime_observation_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProbeStore {
    #[serde(default)]
    pub records: BTreeMap<String, ProbeRecord>,
}

/// Number of `raw_summary` bytes kept in compact inventory/preview rows. The
/// exact record stays reachable through the per-lane probe read.
const RAW_SUMMARY_PREVIEW_BYTES: usize = 256;

fn preview_raw_summary(summary: &str) -> Value {
    let mut end = RAW_SUMMARY_PREVIEW_BYTES.min(summary.len());
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut row = serde_json::json!({"text": &summary[..end]});
    if end < summary.len() {
        row["text_truncated"] = Value::Bool(true);
    }
    row
}

impl ProbeRecord {
    /// Compact caller-facing lane status. `raw_summary` is opaque audit text,
    /// not a credential sink: inventories preview it with an explicit
    /// truncation marker, and the exact per-lane record stays reachable
    /// through the probe read's bounded body pages.
    pub fn summary_view(&self, now: u64) -> Value {
        let mut view = serde_json::json!({
            "provider": self.provider.as_str(),
            "credential_status": self.credential_status,
            "quota_status": self.quota_status,
            "quota_confidence": self.quota_confidence,
        });
        if let Some(account) = &self.account {
            view["account"] = Value::String(account.clone());
        }
        for (field, value) in [
            ("five_hour_utilization", self.five_hour_utilization),
            ("seven_day_utilization", self.seven_day_utilization),
            ("balance_capacity", self.balance_capacity),
        ] {
            if let Some(value) = value {
                view[field] = serde_json::json!(value);
            }
        }
        for (field, value) in [
            ("cooldown_until", self.cooldown_until),
            ("last_probe_at", self.last_probe_at),
            (
                "last_runtime_observation_at",
                self.last_runtime_observation_at,
            ),
        ] {
            if let Some(value) = value {
                view[field] = serde_json::json!(value);
            }
        }
        if let Some(until) = self.cooldown_until {
            view["cooldown_active"] = Value::Bool(until > now);
        }
        if let Some(summary) = &self.raw_summary {
            view["raw_summary_preview"] = preview_raw_summary(summary);
        }
        view
    }
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

// kept: lane-key shape for in-progress lane-grouping work
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct LaneId {
    provider: Provider,
    account: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AllocationContext {
    pub in_flight: BTreeMap<String, usize>,
    pub probes: BTreeMap<String, ProbeRecord>,
    pub provider_binary_location: ProviderBinaryLocation,
}

static LEASE_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ALLOCATION_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// Acquire the process-wide allocation lock, blocking until it is free.
///
/// Runtime allocation (capability-/tier-/pool-based lane selection) serializes
/// so concurrent dispatches do not race on lease/probe state. Importantly,
/// the caller in `dispatch_fresh_bro_task` holds this guard for the full
/// dispatch — lane selection + trace write *and* through ambient build, arg
/// construction, provider spawn, task-store insertion, and lease recording.
/// The long hold is intentional: `allocation_context` only counts running
/// tasks that have a recorded lease, so releasing the lock between
/// `save_trace` and `record_lease` would let the next waiter allocate from
/// stale capacity and pick the same capped lane.
///
/// Previous shape was `try_lock` returning `error.allocation_busy: ...; retry
/// shortly`, but no caller-side retry helper existed; workflow runners just
/// propagated the error up through `on_failure: halt` and killed the arc.
/// Blocking is the right shape here.
///
/// Poisoned-mutex recovery is preserved (recover the inner data rather than
/// propagate panic state).
///
/// This is a `tokio::sync::Mutex`, not a `std::sync::Mutex`, and that is
/// forced by the shape above rather than chosen for taste: the guard is held
/// across the whole dispatch, and since the fleetd cutover the dispatch
/// `await`s (the executor seam is async). A `std` guard is not `Send`, so
/// holding one across an await makes every enclosing future non-`Send` and
/// `tokio::spawn` rejects it. It also blocked an executor thread for the
/// dispatch duration, which an async mutex does not.
///
/// This is the migration the previous note here prescribed for exactly this
/// trigger, with the same long-scope semantics preserved.
///
/// Poisoning is not a concern for a `tokio::sync::Mutex` (it has no poison
/// state), so the previous `into_inner` recovery arm is gone.
pub async fn acquire_allocation_lock() -> tokio::sync::MutexGuard<'static, ()> {
    ALLOCATION_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

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

fn probes_file(store_dir: &Path) -> PathBuf {
    allocator_dir(store_dir).join("probes.json")
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
            (Brodex, Some("gpt-5.3-codex-spark"), Some("low"), None),
            (Glm, Some("glm-4.5-air"), Some("low"), None),
            (Deepseek, Some("deepseek-v4-flash"), Some("low"), None),
            (Minimax, Some("MiniMax-M3"), Some("low"), None),
            (Kimi, Some("kimi-k2.7-code-highspeed"), Some("low"), None),
            (VibeBh, Some("devstral-small-latest"), Some("none"), None),
        ],
    );
    tier(
        "standard",
        vec![
            (Brodex, Some("gpt-5.5"), Some("medium"), None),
            (Glm, Some("glm-5-turbo"), Some("medium"), None),
            (Deepseek, Some("deepseek-v4-pro"), Some("medium"), None),
            (Minimax, Some("MiniMax-M3"), Some("medium"), None),
            (Kimi, Some("k3"), Some("medium"), None),
            (VibeBh, Some("mistral-medium-3.5"), Some("high"), None),
        ],
    );
    tier(
        "premium",
        vec![
            (Brodex, Some("gpt-5.5"), Some("high"), None),
            (Glm, Some("glm-5.2"), Some("high"), None),
            (Deepseek, Some("deepseek-v4-pro"), Some("high"), None),
            (Minimax, Some("MiniMax-M3"), Some("high"), None),
            (Kimi, Some("k3"), Some("high"), None),
            (VibeBh, Some("magistral-medium-latest"), Some("high"), None),
        ],
    );
    tier(
        "deepthink",
        vec![
            (Brodex, Some("gpt-5.5"), Some("xhigh"), None),
            (Deepseek, Some("deepseek-v4-pro"), Some("max"), None),
            (Minimax, Some("MiniMax-M3"), Some("max"), None),
            (Kimi, Some("k3"), Some("max"), None),
            (VibeBh, Some("magistral-medium-latest"), Some("high"), None),
        ],
    );
    tier(
        "super-el-cheapo-drones",
        vec![
            (Brodex, Some("gpt-5.3-codex-spark"), Some("low"), Some(1.0)),
            (Glm, Some("glm-4.5-air"), Some("low"), Some(0.8)),
            (Deepseek, Some("deepseek-v4-flash"), Some("low"), Some(0.8)),
            (Minimax, Some("MiniMax-M3"), Some("low"), Some(0.8)),
            (
                Kimi,
                Some("kimi-k2.7-code-highspeed"),
                Some("low"),
                Some(0.8),
            ),
            (
                VibeBh,
                Some("devstral-small-latest"),
                Some("none"),
                Some(0.8),
            ),
        ],
    );

    let mut pools = BTreeMap::new();
    pools.insert(
        "coding".into(),
        PoolConfig {
            providers: vec![Brodex, Glm, Kimi, Deepseek, Minimax, VibeBh],
            provider_weights: provider_weights(&[
                (Brodex, 1.0),
                (Glm, 1.0),
                (Kimi, 0.85),
                (Deepseek, 0.55),
                (Minimax, 0.55),
                (VibeBh, 0.45),
            ]),
            max_concurrent_per_account: Some(1),
        },
    );
    pools.insert(
        "any".into(),
        PoolConfig {
            providers: vec![Glm, Brodex, Kimi, Deepseek, Minimax, VibeBh],
            provider_weights: provider_weights(&[
                (Glm, 1.0),
                (Brodex, 0.68),
                (Kimi, 0.6),
                (Deepseek, 0.55),
                (Minimax, 0.55),
                (VibeBh, 0.45),
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

pub fn merge_runtime_request(
    base: Option<RuntimeRequest>,
    overlay: Option<RuntimeRequest>,
) -> Option<RuntimeRequest> {
    match (base, overlay) {
        (Some(mut request), Some(overlay)) => {
            apply_runtime_overlay(&mut request, overlay);
            Some(request)
        }
        (Some(request), None) | (None, Some(request)) => Some(request),
        (None, None) => None,
    }
}

// kept: public overlay helper alongside `runtime_request_from_optional_overlay`; consumed by allocator extensions
#[allow(dead_code)]
pub fn runtime_request_with_overlay(
    base: Option<RuntimeRequest>,
    overlay: RuntimeRequest,
) -> RuntimeRequest {
    let mut request = base.unwrap_or_default();
    apply_runtime_overlay(&mut request, overlay);
    request
}

fn apply_runtime_overlay(request: &mut RuntimeRequest, overlay: RuntimeRequest) {
    let tier_is_some = overlay.tier.is_some();
    let bounded_fields_present = overlay.min_tier.is_some() || overlay.max_tier.is_some();
    if tier_is_some {
        request.tier = overlay.tier;
    }
    if overlay.tier_ladder.is_some() {
        request.tier_ladder = overlay.tier_ladder;
    }
    if overlay.tier_mode != TierMode::Exact || tier_is_some || bounded_fields_present {
        request.tier_mode = overlay.tier_mode;
    }
    if overlay.min_tier.is_some() {
        request.min_tier = overlay.min_tier;
    }
    if overlay.max_tier.is_some() {
        request.max_tier = overlay.max_tier;
    }
    request.capabilities.extend(overlay.capabilities);
    request.capabilities.sort_by_key(|cap| format!("{cap:?}"));
    request.capabilities.dedup();
    request
        .derived_capabilities
        .extend(overlay.derived_capabilities);
    request
        .derived_capabilities
        .sort_by_key(|cap| format!("{cap:?}"));
    request.derived_capabilities.dedup();
    if overlay.durable {
        request.durable = true;
    }
    if overlay.pool.is_some() {
        request.pool = overlay.pool;
    }
    if overlay.selection_policy.is_some() {
        request.selection_policy = overlay.selection_policy;
    }
    if overlay.pin.is_some() {
        request.pin = overlay.pin;
    }
    if overlay.prefer.is_some() {
        request.prefer = overlay.prefer;
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

// kept: public no-probes shortcut for `allocation_context_with_probes`
#[allow(dead_code)]
pub fn allocation_context(task_store: &TaskStore, leases: &RuntimeLeaseStore) -> AllocationContext {
    allocation_context_with_probes(task_store, leases, ProbeStore::default())
}

pub fn allocation_context_with_probes(
    task_store: &TaskStore,
    leases: &RuntimeLeaseStore,
    probes: ProbeStore,
) -> AllocationContext {
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
    AllocationContext {
        in_flight,
        probes: probes.records,
        provider_binary_location: super::harness_provider_binary_location(),
    }
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

    if matches!(lane.provider, Provider::Workflow) {
        candidate.exclusion_reason = Some("provider_not_dispatchable".into());
        return candidate;
    }
    if ctx.provider_binary_location == ProviderBinaryLocation::DaemonHost
        && provider_binary_missing(lane.provider)
    {
        candidate.exclusion_reason = Some("provider_binary_missing".into());
        return candidate;
    }
    if let Some(model) = &lane.model {
        let models = lane.provider.models();
        if models.is_empty() || !models.iter().any(|info| info.id == model) {
            candidate.exclusion_reason = Some("provider_disallows_model".into());
            return candidate;
        }
    }
    if let Some(effort) = &lane.effort {
        // Effort validity is model-keyed: the effective model is the explicit
        // lane model, else the provider's default model. gpt-5.6 Sol/Terra
        // expose `ultra` while Luna does not, so gating on the flat provider
        // effort list would wrongly admit `ultra`+Luna.
        let effective_model = lane.model.as_deref().or_else(|| {
            lane.provider
                .models()
                .iter()
                .find(|m| m.default)
                .map(|m| m.id)
        });
        let valid = match effective_model {
            Some(model) => lane.provider.model_efforts(model),
            None => lane.provider.efforts().iter().map(|e| e.id).collect(),
        };
        if valid.is_empty() || !valid.iter().any(|id| *id == effort.as_str()) {
            candidate.exclusion_reason = Some("provider_disallows_effort".into());
            return candidate;
        }
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
    let probe = ctx.probes.get(&key);
    if probe.is_some_and(|probe| {
        matches!(
            probe.credential_status,
            CredentialStatus::Missing | CredentialStatus::Expired
        )
    }) {
        candidate.exclusion_reason = Some("credential_unavailable".into());
        return candidate;
    }
    let now = super::now_ms();
    if probe.is_some_and(|probe| {
        matches!(probe.quota_status, QuotaStatus::Exhausted)
            && !runtime_quota_observation_expired(probe, now)
    }) {
        candidate.exclusion_reason = Some("quota_exhausted".into());
        return candidate;
    }
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
    let quota_capacity = probe.map(|probe| quota_capacity(probe, now)).unwrap_or(0.5);
    let cooldown_capacity = probe
        .and_then(|probe| probe.cooldown_until)
        .map(|until| if until > now { 0.0 } else { 1.0 })
        .unwrap_or(1.0);
    if cooldown_capacity == 0.0 {
        candidate.exclusion_reason = Some("cooldown_active".into());
        return candidate;
    }
    let tier_fit = if tier.is_some() { 1.0 } else { 0.8 };
    candidate
        .score_components
        .insert("provider_preference".into(), provider_preference);
    candidate
        .score_components
        .insert("quota_capacity".into(), quota_capacity);
    candidate
        .score_components
        .insert("concurrency_capacity".into(), concurrency_capacity);
    candidate
        .score_components
        .insert("cooldown_capacity".into(), cooldown_capacity);
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
    let runtime_preference = runtime_preference_score(request, lane.provider);
    candidate
        .score_components
        .insert("selection_policy".into(), policy_score);
    candidate
        .score_components
        .insert("runtime_preference".into(), runtime_preference);
    candidate.score = provider_preference
        * quota_capacity
        * concurrency_capacity
        * cooldown_capacity
        * tier_fit
        * policy_score
        * runtime_preference;
    candidate.eligible = true;
    candidate
}

fn runtime_quota_observation_expired(probe: &ProbeRecord, now: u64) -> bool {
    matches!(probe.quota_confidence, QuotaConfidence::RuntimeRateLimit)
        && probe.cooldown_until.is_some_and(|until| until <= now)
}

fn quota_capacity(probe: &ProbeRecord, now: u64) -> f64 {
    // A runtime 429 is evidence for its cooldown, not permanent exhaustion.
    // Once expired, allow a new observation with unknown capacity. Preserve the
    // stored receipt and do not reinterpret it as successful provider admission.
    if runtime_quota_observation_expired(probe, now) {
        return 0.5;
    }
    match probe.quota_confidence {
        QuotaConfidence::QuotaProbe | QuotaConfidence::RuntimeRateLimit => {
            let utilization = probe
                .five_hour_utilization
                .into_iter()
                .chain(probe.seven_day_utilization)
                .fold(0.0_f64, f64::max);
            (1.0 - utilization).clamp(0.0, 1.0)
        }
        QuotaConfidence::PaygBalance => probe.balance_capacity.unwrap_or(0.5).clamp(0.0, 1.0),
        QuotaConfidence::ActiveAcceptance => 0.95,
        QuotaConfidence::CredentialOnly => 0.35,
        QuotaConfidence::None => 0.25,
    }
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

fn runtime_preference_score(request: &RuntimeRequest, provider: Provider) -> f64 {
    request
        .prefer
        .as_ref()
        .and_then(|prefer| prefer.provider)
        .map(|preferred| if preferred == provider { 1.25 } else { 0.9 })
        .unwrap_or(1.0)
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
    // Accounts are global named configurations, not membership declarations
    // for every provider. An unrelated account can both mislabel a dispatch
    // and inject transport environment into it. Resolve the same default as
    // execution, without a second native lane that aliases that default and
    // could bypass its concurrency accounting.
    vec![
        bro_config
            .provider_defaults
            .get(&provider)
            .map(|default| default.account.clone()),
    ]
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
        .unwrap_or(Provider::Brodex);
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

pub(crate) fn lane_key(provider: Provider, account: Option<&str>) -> String {
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
    if let Err(error) = write_json_atomic(&leases_file(store_dir), store) {
        tracing::warn!(target: "blackbox::allocator", %error, "lease store save failed");
    }
}

pub fn probe_store_load(store_dir: &Path) -> ProbeStore {
    fs::read_to_string(probes_file(store_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the probe store. Errors are real: the previous file (if any) is
/// untouched because publication is an atomic rename, so callers must
/// propagate the failure instead of reporting a durable update.
pub fn probe_store_save(store_dir: &Path, store: &ProbeStore) -> std::io::Result<()> {
    write_json_atomic(&probes_file(store_dir), store)
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
    lookup_lease_for_session_filtered(store_dir, task_store, Some(provider), session_id)
}

pub fn lookup_lease_for_session_any_provider(
    store_dir: &Path,
    task_store: &TaskStore,
    session_id: &str,
) -> Option<RuntimeLease> {
    lookup_lease_for_session_filtered(store_dir, task_store, None, session_id)
}

fn lookup_lease_for_session_filtered(
    store_dir: &Path,
    task_store: &TaskStore,
    provider: Option<Provider>,
    session_id: &str,
) -> Option<RuntimeLease> {
    let leases = lease_store_load(store_dir);
    let live_task_lease = task_store
        .all_tasks()
        .into_iter()
        .filter_map(|task| {
            let inner = task.inner.lock();
            (provider.is_none_or(|provider| inner.provider == provider)
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
        .map(|(_, lease)| lease);
    live_task_lease.or_else(|| {
        leases
            .leases
            .values()
            .filter(|lease| {
                lease.durable
                    && provider.is_none_or(|provider| lease.provider == provider)
                    && lease.session_id == session_id
            })
            .max_by_key(|lease| (lease.last_seen_at, lease.created_at))
            .cloned()
    })
}

pub fn lookup_lease_for_task(store_dir: &Path, task_id: &str) -> Option<RuntimeLease> {
    lease_store_load(store_dir).leases.get(task_id).cloned()
}

pub fn save_trace(store_dir: &Path, trace: &SelectionTrace) {
    let path = traces_dir(store_dir).join(format!("{}.json", trace.id));
    if let Err(error) = write_json_atomic(&path, trace) {
        tracing::warn!(target: "blackbox::allocator", %error, "selection trace save failed");
    }
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

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(data.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn lease_from_allocation(
    task_id: String,
    session_id: String,
    allocation: &Allocation,
    project_dir: Option<String>,
    cwd: Option<String>,
    brofile_context: Option<crate::orchestration::brofile::BrofileContext>,
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
        brofile_context,
    }
}

pub fn lease_for_resume_task(
    previous: &RuntimeLease,
    task_id: String,
    session_id: String,
    cwd: Option<String>,
) -> RuntimeLease {
    RuntimeLease {
        task_id,
        session_id,
        provider: previous.provider,
        account: previous.account.clone(),
        model: previous.model.clone(),
        effort: previous.effort.clone(),
        tier: previous.tier.clone(),
        durable: previous.durable,
        capabilities: previous.capabilities.clone(),
        project_dir: previous.project_dir.clone(),
        cwd: cwd.or_else(|| previous.cwd.clone()),
        selection_trace_id: previous.selection_trace_id.clone(),
        created_at: super::now_ms(),
        last_seen_at: super::now_ms(),
        brofile_context: previous.brofile_context.clone(),
    }
}

pub fn exec_opts_for_lane(lane: &RuntimeLane) -> Option<ExecOpts> {
    (lane.model.is_some() || lane.effort.is_some()).then(|| ExecOpts {
        model: lane.model.clone(),
        effort: lane.effort.clone(),
        provider_defaults: None,
        // Code-mode is not an allocation dimension; folded in later from the
        // dispatch's resolved value (FreshDispatchRequest.code_mode).
        code_mode: None,
        // Service tier is not an allocation dimension; folded in later from
        // the dispatch's resolved value.
        service_tier: None,
        // Output schema is delivered from the agent manifest at dispatch, not an
        // allocation dimension.
        output_schema: None,
    })
}

pub fn with_derived_capability(
    mut request: Option<RuntimeRequest>,
    capability: Capability,
) -> Option<RuntimeRequest> {
    let request = request.get_or_insert_with(RuntimeRequest::default);
    request.derived_capabilities.push(capability);
    request
        .derived_capabilities
        .sort_by_key(|cap| format!("{cap:?}"));
    request.derived_capabilities.dedup();
    Some(request.clone())
}

impl RuntimeRequest {
    /// True when the request carries any signal that should drive
    /// cross-provider allocation: a tier (or tier bounds/ladder), a pool,
    /// a pin, a preference, a selection policy, or explicit capability
    /// requirements. A request that lacks all of these is *inert* — it was
    /// synthesized purely from the `durable` flag and/or
    /// `derived_capabilities` (e.g. `StructuredOutput` forced by an output
    /// schema) and expresses no provider preference of its own.
    pub fn expresses_selection_intent(&self) -> bool {
        self.tier.is_some()
            || self.tier_ladder.is_some()
            || self.min_tier.is_some()
            || self.max_tier.is_some()
            || self.pool.is_some()
            || self.pin.is_some()
            || self.prefer.is_some()
            || self.selection_policy.is_some()
            || !self.capabilities.is_empty()
    }
}

/// Honor a brofile's static provider when its allocation request is inert.
///
/// A brofile with a static `provider` but no `runtime` block still reaches
/// the allocator as `Some(RuntimeRequest)` whenever a derived capability
/// (output-schema `StructuredOutput`) or the durable flag forces one into
/// existence. With no tier/pool/pin, the allocator then free-selects across
/// `Provider::ALL` and silently overrides the static provider. When the
/// request expresses no selection intent of its own, seed an artifact-level
/// pin from the static provider/model/effort so the allocator resolves the
/// declared provider instead of free-selecting. No-op when the request
/// already expresses selection intent, or when there is no request at all
/// (a `None` request never reaches the allocator).
pub fn pin_static_provider_if_inert(
    runtime: Option<RuntimeRequest>,
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
) -> Option<RuntimeRequest> {
    let mut request = runtime?;
    if request.expresses_selection_intent() {
        return Some(request);
    }
    request.pin = Some(RuntimePin {
        provider: Some(provider),
        account: None,
        model,
        effort,
        authority: PinAuthority::Artifact,
    });
    Some(request)
}

/// Narrow a request's provider pool to exclude providers already taken by
/// cohort siblings — the cohort-diversity-floor primitive. The ensemble
/// dispatch loop accumulates the providers its earlier members resolved and
/// passes them here for each subsequent member, so a tiered cohort spreads
/// across distinct providers instead of all collapsing onto the same
/// tiebreak winner. Reuses the existing `pool.providers` intersection in
/// `resolve_provider_pool`; tier/pin/scoring are otherwise untouched.
///
/// Guarantees:
/// - No-op when `runtime` is `None` (no allocation happens) or `exclude` is
///   empty (floor already satisfied, or no floor configured).
/// - Never empties the candidate pool: if every available provider is already
///   taken (floor exceeds distinct providers), it allows repeats rather than
///   failing allocation — a soft floor, not a hard refusal.
pub fn exclude_providers_for_diversity(
    runtime: Option<RuntimeRequest>,
    exclude: &[Provider],
) -> Option<RuntimeRequest> {
    let mut request = runtime?;
    if exclude.is_empty() {
        return Some(request);
    }
    let excluded: HashSet<Provider> = exclude.iter().copied().collect();
    // Candidate universe: an explicit pool if the request already carries one,
    // otherwise the same Provider::ALL base resolve_provider_pool starts from.
    let base: Vec<Provider> = match &request.pool {
        Some(pool) if !pool.providers.is_empty() => pool.providers.clone(),
        _ => Provider::ALL.to_vec(),
    };
    let allowed: Vec<Provider> = base
        .into_iter()
        .filter(|provider| !excluded.contains(provider))
        .collect();
    if allowed.is_empty() {
        // Soft floor: don't strand the dispatch with an empty pool.
        return Some(request);
    }
    let name = request.pool.as_ref().and_then(|pool| pool.name.clone());
    request.pool = Some(PoolRef {
        name,
        providers: allowed,
    });
    Some(request)
}

pub fn provider_candidates_for_request(
    request: &RuntimeRequest,
    config: &AllocatorConfig,
) -> Vec<Provider> {
    resolve_provider_pool(request, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_accounts_are_provider_scoped() {
        let mut config = BroConfig::default();
        config
            .accounts
            .insert("openrouter".into(), Default::default());
        config.provider_defaults.insert(
            Provider::Glm,
            super::super::brofile::ProviderDefault {
                account: "openrouter".into(),
            },
        );
        assert_eq!(
            account_candidates(Provider::Brodex, None, &config),
            vec![None]
        );
        assert_eq!(
            account_candidates(Provider::Glm, None, &config),
            vec![Some("openrouter".into())]
        );

        config.provider_defaults.insert(
            Provider::Brodex,
            super::super::brofile::ProviderDefault {
                account: "coding".into(),
            },
        );
        assert_eq!(
            account_candidates(Provider::Brodex, None, &config),
            vec![Some("coding".into())]
        );
        let pin = RuntimePin {
            account: Some("explicit".into()),
            ..Default::default()
        };
        assert_eq!(
            account_candidates(Provider::Brodex, Some(&pin), &config),
            vec![Some("explicit".into())]
        );
    }

    struct EnvRestore {
        prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self {
                prior: keys
                    .iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in self.prior.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    fn with_provider_bins<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::util::test_env_lock();
        let keys = [
            "CLAUDE_BIN",
            "CODEX_BIN",
            "GEMINI_BIN",
            "VIBE_BIN",
            "COPILOT_BIN",
            "BRO_HARNESS_BIN",
        ];
        let _restore = EnvRestore::capture(&keys);
        for key in keys {
            unsafe {
                std::env::set_var(key, "sh");
            }
        }
        f()
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
                // Effort validity is model-keyed: check the tier's mapped model
                // (else the provider default) so a tier can't pair an effort
                // with a model that doesn't support it (e.g. `ultra` + Luna).
                let model = entry
                    .model
                    .as_deref()
                    .or_else(|| provider.models().iter().find(|m| m.default).map(|m| m.id));
                let valid = match model {
                    Some(model) => provider.model_efforts(model),
                    None => provider.efforts().iter().map(|e| e.id).collect(),
                };
                assert!(
                    valid.iter().any(|id| *id == effort),
                    "{tier}.{provider} maps effort `{effort}` unsupported by model {model:?}"
                );
            }
        }
    }

    #[test]
    fn executor_host_binary_resolution_does_not_require_daemon_binary() {
        let _guard = crate::util::test_env_lock();
        let _restore = EnvRestore::capture(&["BRO_HARNESS_BIN"]);
        unsafe {
            std::env::set_var(
                "BRO_HARNESS_BIN",
                "definitely_missing_bro_harness_binary_for_allocator_test",
            );
        }
        let request = RuntimeRequest {
            pin: Some(RuntimePin {
                provider: Some(Provider::Brodex),
                ..Default::default()
            }),
            durable: true,
            ..Default::default()
        };
        let local = allocate(
            request.clone(),
            &built_in_config(),
            &BroConfig::default(),
            &AllocationContext::default(),
        );
        assert_eq!(
            local.trace.candidates[0].exclusion_reason.as_deref(),
            Some("provider_binary_missing")
        );

        let remote = allocate(
            request,
            &built_in_config(),
            &BroConfig::default(),
            &AllocationContext {
                provider_binary_location: ProviderBinaryLocation::ExecutorHost,
                ..Default::default()
            },
        );
        assert!(remote.trace.error.is_none(), "{:?}", remote.trace.error);
        assert_eq!(remote.lane.provider, Provider::Brodex);
        assert_eq!(remote.trace.candidates[0].exclusion_reason, None);
    }

    #[test]
    fn workflow_is_excluded_before_executor_binary_policy() {
        let request = RuntimeRequest {
            pin: Some(RuntimePin {
                provider: Some(Provider::Workflow),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(provider_candidates_for_request(&request, &built_in_config()).is_empty());
    }

    #[test]
    fn allocation_intersects_named_pool_and_provider_filter() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("coding".into()),
                    providers: vec![Provider::Brodex],
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
                    ..Default::default()
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Brodex);
            assert_eq!(allocation.lane.model.as_deref(), Some("gpt-5.5"));
            assert_eq!(allocation.lane.effort.as_deref(), Some("medium"));
        });
    }

    #[test]
    fn inert_structured_output_request_honors_static_provider_pin() {
        with_provider_bins(|| {
            // Simulate corpus-pathfinder: a static-provider brofile with no
            // runtime block whose output schema forces a StructuredOutput
            // derived capability into existence. The request carries no
            // tier/pool/pin of its own — it is inert.
            let inert = with_derived_capability(None, Capability::StructuredOutput);
            assert!(
                !inert.as_ref().unwrap().expresses_selection_intent(),
                "derived-only request must be inert"
            );
            // The dispatch seam seeds a pin from the brofile's static provider.
            let request = pin_static_provider_if_inert(
                inert,
                Provider::Brodex,
                Some("gpt-5.5".into()),
                Some("medium".into()),
            )
            .expect("inert request seeded");

            let allocation = allocate(
                request,
                &built_in_config(),
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            // Honors the declared provider instead of free-selecting across
            // Provider::ALL — the pre-fix bug landed on claude-opus-4-8.
            assert_eq!(allocation.lane.provider, Provider::Brodex);
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
                    providers: vec![Provider::Deepseek],
                }),
                // Vision is satisfied by no harness provider (StructuredOutput is
                // now satisfiable via the harness `final_result` tool), so this
                // still exercises the fail-closed-on-missing-capability path.
                capabilities: vec![Capability::Vision],
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
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
    fn operator_model_pin_overrides_tier_mapping_when_valid() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pin: Some(RuntimePin {
                    provider: Some(Provider::Brodex),
                    model: Some("gpt-5.3-codex-spark".into()),
                    authority: PinAuthority::Operator,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Brodex);
            assert_eq!(
                allocation.lane.model.as_deref(),
                Some("gpt-5.3-codex-spark")
            );
        });
    }

    #[test]
    fn invalid_operator_pin_fails_closed() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pin: Some(RuntimePin {
                    provider: Some(Provider::Brodex),
                    model: Some("not-a-real-codex-model".into()),
                    authority: PinAuthority::Operator,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                allocation
                    .trace
                    .candidates
                    .iter()
                    .any(|candidate| candidate.exclusion_reason.as_deref()
                        == Some("provider_disallows_model")),
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
    fn hard_model_pin_fails_closed_when_provider_has_no_model_surface() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                pin: Some(RuntimePin {
                    provider: Some(Provider::VibeBh),
                    model: Some("ignored-model".into()),
                    authority: PinAuthority::Operator,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                allocation
                    .trace
                    .candidates
                    .iter()
                    .any(|candidate| candidate.exclusion_reason.as_deref()
                        == Some("provider_disallows_model")),
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
    fn hard_effort_pin_fails_closed_when_effort_unsupported() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            // Every dispatchable provider now advertises an effort surface, so
            // fail-closed is exercised by an effort the provider does not list
            // (the allocator excludes the lane the same way it would an empty
            // surface — allocator.rs `provider_disallows_effort`).
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pin: Some(RuntimePin {
                    provider: Some(Provider::Deepseek),
                    effort: Some("nonexistent-effort".into()),
                    authority: PinAuthority::Operator,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                allocation
                    .trace
                    .candidates
                    .iter()
                    .any(|candidate| candidate.exclusion_reason.as_deref()
                        == Some("provider_disallows_effort")),
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
    fn effort_gate_is_model_keyed_for_gpt_56_ultra() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            // gpt-5.6 Luna does not expose `ultra` — pinning it must fail closed
            // through the model-keyed gate (allocator.rs `provider_disallows_effort`),
            // even though `ultra` IS valid for Sol/Terra on the same provider.
            let luna = allocate(
                RuntimeRequest {
                    pin: Some(RuntimePin {
                        provider: Some(Provider::Brodex),
                        model: Some("gpt-5.6-luna".into()),
                        effort: Some("ultra".into()),
                        authority: PinAuthority::Operator,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                luna.trace
                    .candidates
                    .iter()
                    .any(|c| c.exclusion_reason.as_deref() == Some("provider_disallows_effort")),
                "ultra+luna should be excluded: {:?}",
                luna.trace.candidates
            );

            // The same effort on Sol is admitted — the gate must not reject it
            // for effort reasons.
            let sol = allocate(
                RuntimeRequest {
                    pin: Some(RuntimePin {
                        provider: Some(Provider::Brodex),
                        model: Some("gpt-5.6-sol".into()),
                        effort: Some("ultra".into()),
                        authority: PinAuthority::Operator,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                !sol.trace
                    .candidates
                    .iter()
                    .any(|c| c.exclusion_reason.as_deref() == Some("provider_disallows_effort")),
                "ultra+sol must not be excluded for effort: {:?}",
                sol.trace.candidates
            );
        });
    }

    #[test]
    fn provider_preference_scores_availability_policy() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("coding".into()),
                    providers: vec![Provider::Glm, Provider::Glm],
                }),
                prefer: Some(RuntimePreference {
                    provider: Some(Provider::Glm),
                }),
                ..Default::default()
            };
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    ..Default::default()
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Glm);
            assert!(
                allocation.trace.candidates.iter().any(|candidate| candidate
                    .score_components
                    .get("runtime_preference")
                    .is_some_and(|score| *score != 1.0)),
                "{:?}",
                allocation.trace.candidates
            );
        });
    }

    #[test]
    fn probe_quota_capacity_participates_in_scoring() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("coding".into()),
                    providers: vec![Provider::Glm, Provider::Deepseek],
                }),
                ..Default::default()
            };
            let probes = BTreeMap::from([
                (
                    // High-utilization lane → low quota_capacity score (< 0.1).
                    lane_key(Provider::Deepseek, None),
                    ProbeRecord {
                        provider: Provider::Deepseek,
                        account: None,
                        credential_status: CredentialStatus::Present,
                        quota_status: QuotaStatus::Known,
                        quota_confidence: QuotaConfidence::QuotaProbe,
                        five_hour_utilization: Some(0.95),
                        seven_day_utilization: Some(0.9),
                        balance_capacity: None,
                        cooldown_until: None,
                        last_probe_at: Some(crate::orchestration::now_ms()),
                        last_runtime_observation_at: None,
                        raw_summary: None,
                    },
                ),
                (
                    lane_key(Provider::Glm, None),
                    ProbeRecord {
                        provider: Provider::Glm,
                        account: None,
                        credential_status: CredentialStatus::Present,
                        quota_status: QuotaStatus::Known,
                        quota_confidence: QuotaConfidence::QuotaProbe,
                        five_hour_utilization: Some(0.1),
                        seven_day_utilization: Some(0.2),
                        balance_capacity: None,
                        cooldown_until: None,
                        last_probe_at: Some(crate::orchestration::now_ms()),
                        last_runtime_observation_at: None,
                        raw_summary: None,
                    },
                ),
            ]);
            let allocation = allocate(
                request,
                &cfg,
                &BroConfig::default(),
                &AllocationContext {
                    in_flight: BTreeMap::new(),
                    probes,
                    ..Default::default()
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Glm);
            assert!(allocation.trace.candidates.iter().any(|candidate| {
                candidate
                    .score_components
                    .get("quota_capacity")
                    .is_some_and(|score| *score < 0.1)
            }));
        });
    }

    #[test]
    fn runtime_rate_limit_expires_without_erasing_authoritative_quota_or_credentials() {
        let now = super::super::now_ms();
        let request = RuntimeRequest {
            pin: Some(RuntimePin {
                provider: Some(Provider::Glm),
                model: Some("glm-5.3-flash".into()),
                authority: PinAuthority::Operator,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut probe = ProbeRecord {
            provider: Provider::Glm,
            account: None,
            credential_status: CredentialStatus::Present,
            quota_status: QuotaStatus::Exhausted,
            quota_confidence: QuotaConfidence::RuntimeRateLimit,
            five_hour_utilization: Some(1.0),
            seven_day_utilization: Some(1.0),
            balance_capacity: None,
            cooldown_until: Some(now + 3_600_000),
            last_probe_at: None,
            last_runtime_observation_at: Some(now - 300_000),
            raw_summary: Some("historical runtime 429".into()),
        };
        let preview = |probe: ProbeRecord| {
            allocate(
                request.clone(),
                &built_in_config(),
                &BroConfig::default(),
                &AllocationContext {
                    provider_binary_location: ProviderBinaryLocation::ExecutorHost,
                    probes: BTreeMap::from([(lane_key(Provider::Glm, None), probe)]),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            preview(probe.clone()).trace.candidates[0]
                .exclusion_reason
                .as_deref(),
            Some("quota_exhausted")
        );
        probe.cooldown_until = Some(now - 1);
        let recovered = preview(probe.clone());
        assert!(recovered.trace.error.is_none(), "{:?}", recovered.trace);
        assert_eq!(
            recovered.trace.candidates[0].score_components["quota_capacity"],
            0.5
        );
        assert!(matches!(probe.quota_status, QuotaStatus::Exhausted));
        assert_eq!(probe.raw_summary.as_deref(), Some("historical runtime 429"));
        assert!(runtime_quota_observation_expired(&probe, now - 1));
        assert!(!runtime_quota_observation_expired(&probe, now - 2));
        probe.credential_status = CredentialStatus::Expired;
        assert_eq!(
            preview(probe.clone()).trace.candidates[0]
                .exclusion_reason
                .as_deref(),
            Some("credential_unavailable")
        );
        probe.credential_status = CredentialStatus::Present;
        probe.quota_confidence = QuotaConfidence::QuotaProbe;
        assert_eq!(
            preview(probe.clone()).trace.candidates[0]
                .exclusion_reason
                .as_deref(),
            Some("quota_exhausted")
        );
        probe.quota_confidence = QuotaConfidence::RuntimeRateLimit;
        probe.cooldown_until = None;
        assert_eq!(
            preview(probe).trace.candidates[0]
                .exclusion_reason
                .as_deref(),
            Some("quota_exhausted")
        );
    }

    #[test]
    fn selection_policy_scores_participate_in_ranking() {
        with_provider_bins(|| {
            let cfg = built_in_config();
            let request = RuntimeRequest {
                tier: Some("standard".into()),
                pool: Some(PoolRef {
                    name: Some("coding".into()),
                    providers: vec![Provider::Glm, Provider::Brodex],
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
                    ..Default::default()
                },
            );
            assert!(
                allocation.trace.error.is_none(),
                "{:?}",
                allocation.trace.error
            );
            assert_eq!(allocation.lane.provider, Provider::Brodex);
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
                ..Default::default()
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
                    providers: vec![Provider::Brodex],
                    provider_weights: BTreeMap::from([(Provider::Brodex, 1.0)]),
                    max_concurrent_per_account: None,
                },
            );
            cfg.pools.insert(
                "strict".into(),
                PoolConfig {
                    providers: vec![Provider::Brodex],
                    provider_weights: BTreeMap::from([(Provider::Brodex, 1.0)]),
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
                    in_flight: BTreeMap::from([(lane_key(Provider::Brodex, None), 1)]),
                    ..Default::default()
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

    #[test]
    fn task_lease_lookup_returns_exact_non_durable_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = RuntimeLease {
            task_id: "task-1".into(),
            session_id: "session-1".into(),
            provider: Provider::Brodex,
            account: Some("codex-alt".into()),
            model: Some("gpt-5.3-codex-spark".into()),
            effort: Some("low".into()),
            tier: Some("economy".into()),
            durable: false,
            capabilities: Vec::new(),
            project_dir: None,
            cwd: None,
            selection_trace_id: "alloc-0123456789abcdef0123456789abcdef".into(),
            created_at: 1,
            last_seen_at: 1,
            brofile_context: None,
        };
        lease_store_save(
            tmp.path(),
            &RuntimeLeaseStore {
                leases: BTreeMap::from([(lease.task_id.clone(), lease.clone())]),
            },
        );

        let loaded = lookup_lease_for_task(tmp.path(), "task-1").unwrap();
        assert!(!loaded.durable);
        assert_eq!(loaded.provider, Provider::Brodex);
        assert_eq!(loaded.model.as_deref(), Some("gpt-5.3-codex-spark"));
    }

    #[test]
    fn session_lease_lookup_falls_back_to_latest_durable_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let older = RuntimeLease {
            task_id: "task-1".into(),
            session_id: "session-1".into(),
            provider: Provider::Brodex,
            account: Some("codex-old".into()),
            model: Some("gpt-5.4".into()),
            effort: Some("low".into()),
            tier: Some("economy".into()),
            durable: true,
            capabilities: Vec::new(),
            project_dir: None,
            cwd: None,
            selection_trace_id: "alloc-0123456789abcdef0123456789abcdef".into(),
            created_at: 1,
            last_seen_at: 1,
            brofile_context: None,
        };
        let newer = RuntimeLease {
            task_id: "task-2".into(),
            session_id: "session-1".into(),
            provider: Provider::Brodex,
            account: Some("codex-new".into()),
            model: Some("gpt-5.3-codex-spark".into()),
            effort: Some("medium".into()),
            tier: Some("standard".into()),
            durable: true,
            capabilities: vec![Capability::ToolUse],
            project_dir: None,
            cwd: None,
            selection_trace_id: "alloc-fedcba9876543210fedcba9876543210".into(),
            created_at: 2,
            last_seen_at: 3,
            brofile_context: None,
        };
        let wrong_provider = RuntimeLease {
            provider: Provider::Glm,
            account: Some("claude".into()),
            ..newer.clone()
        };
        lease_store_save(
            tmp.path(),
            &RuntimeLeaseStore {
                leases: BTreeMap::from([
                    (older.task_id.clone(), older),
                    (newer.task_id.clone(), newer.clone()),
                    ("task-3".into(), wrong_provider),
                ]),
            },
        );

        let loaded =
            lookup_lease_for_session(tmp.path(), &TaskStore::new(), Provider::Brodex, "session-1")
                .unwrap();
        assert_eq!(loaded.task_id, "task-2");
        assert_eq!(loaded.account.as_deref(), Some("codex-new"));
        assert_eq!(loaded.model.as_deref(), Some("gpt-5.3-codex-spark"));
    }

    #[test]
    fn session_lease_lookup_any_provider_returns_runtime_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = RuntimeLease {
            task_id: "task-runtime".into(),
            session_id: "session-allocator-swapped".into(),
            provider: Provider::Glm,
            account: Some("claude-alt".into()),
            model: Some("claude-sonnet-4-6".into()),
            effort: Some("medium".into()),
            tier: Some("standard".into()),
            durable: true,
            capabilities: vec![Capability::ToolUse],
            project_dir: None,
            cwd: None,
            selection_trace_id: "alloc-0123456789abcdef0123456789abcdef".into(),
            created_at: 1,
            last_seen_at: 1,
            brofile_context: Some(crate::orchestration::brofile::BrofileContext {
                provider_defaults: Some(
                    crate::orchestration::brofile::ProviderDefaultsMode::StrictSuppress,
                ),
            }),
        };
        lease_store_save(
            tmp.path(),
            &RuntimeLeaseStore {
                leases: BTreeMap::from([(lease.task_id.clone(), lease)]),
            },
        );

        assert!(
            lookup_lease_for_session(
                tmp.path(),
                &TaskStore::new(),
                Provider::Brodex,
                "session-allocator-swapped",
            )
            .is_none(),
            "provider-filtered lookup should not pretend nominal provider equals runtime provider",
        );
        let loaded = lookup_lease_for_session_any_provider(
            tmp.path(),
            &TaskStore::new(),
            "session-allocator-swapped",
        )
        .unwrap();
        assert_eq!(loaded.provider, Provider::Glm);
        assert_eq!(
            loaded.brofile_context.and_then(|ctx| ctx.provider_defaults),
            Some(crate::orchestration::brofile::ProviderDefaultsMode::StrictSuppress)
        );
    }

    #[test]
    fn resume_task_lease_preserves_selected_lane() {
        let previous = RuntimeLease {
            task_id: "task-1".into(),
            session_id: "session-1".into(),
            provider: Provider::Brodex,
            account: Some("codex-alt".into()),
            model: Some("gpt-5.3-codex-spark".into()),
            effort: Some("low".into()),
            tier: Some("economy".into()),
            durable: false,
            capabilities: vec![Capability::ToolUse],
            project_dir: Some("/repo".into()),
            cwd: Some("/repo".into()),
            selection_trace_id: "alloc-0123456789abcdef0123456789abcdef".into(),
            created_at: 1,
            last_seen_at: 1,
            brofile_context: None,
        };

        let resumed = lease_for_resume_task(
            &previous,
            "task-2".into(),
            "session-1".into(),
            Some("/repo/subdir".into()),
        );
        assert_eq!(resumed.task_id, "task-2");
        assert_eq!(resumed.provider, Provider::Brodex);
        assert_eq!(resumed.account.as_deref(), Some("codex-alt"));
        assert_eq!(resumed.model.as_deref(), Some("gpt-5.3-codex-spark"));
        assert_eq!(resumed.effort.as_deref(), Some("low"));
        assert_eq!(resumed.capabilities, vec![Capability::ToolUse]);
        assert_eq!(resumed.project_dir.as_deref(), Some("/repo"));
        assert_eq!(resumed.cwd.as_deref(), Some("/repo/subdir"));
        assert!(!resumed.durable);
    }

    #[test]
    fn with_derived_capability_adds_and_dedupes_capabilities() {
        let request = RuntimeRequest {
            derived_capabilities: vec![Capability::StructuredOutput],
            ..Default::default()
        };
        let updated = with_derived_capability(Some(request), Capability::StructuredOutput).unwrap();
        assert_eq!(
            updated.derived_capabilities,
            vec![Capability::StructuredOutput]
        );

        let created = with_derived_capability(None, Capability::ToolUse).unwrap();
        assert_eq!(created.derived_capabilities, vec![Capability::ToolUse]);
    }

    #[test]
    fn pin_static_provider_seeds_pin_for_inert_request() {
        // A request synthesized purely from a derived StructuredOutput
        // capability carries no selection intent → must be pinned to the
        // brofile's static provider instead of free-selecting.
        let inert = RuntimeRequest {
            derived_capabilities: vec![Capability::StructuredOutput],
            ..Default::default()
        };
        assert!(!inert.expresses_selection_intent());
        let pinned = pin_static_provider_if_inert(
            Some(inert),
            Provider::Brodex,
            Some("gpt-5.5".to_string()),
            Some("medium".to_string()),
        )
        .unwrap();
        let pin = pinned.pin.expect("pin seeded");
        assert_eq!(pin.provider, Some(Provider::Brodex));
        assert_eq!(pin.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(pin.effort.as_deref(), Some("medium"));
        assert_eq!(pin.authority, PinAuthority::Artifact);
        // Derived capability preserved alongside the seeded pin.
        assert_eq!(
            pinned.derived_capabilities,
            vec![Capability::StructuredOutput]
        );
    }

    #[test]
    fn pin_static_provider_noop_when_request_has_selection_intent() {
        // A request that already names a tier expresses intent → untouched.
        let tiered = RuntimeRequest {
            tier: Some("premium".to_string()),
            durable: true,
            derived_capabilities: vec![Capability::StructuredOutput],
            ..Default::default()
        };
        assert!(tiered.expresses_selection_intent());
        let result =
            pin_static_provider_if_inert(Some(tiered), Provider::Brodex, None, None).unwrap();
        assert!(result.pin.is_none(), "tiered request must not be pinned");

        // The durable flag alone is NOT selection intent — a durable
        // executor with a static-provider brofile and no tier should still
        // honor the declared provider.
        let durable_only = RuntimeRequest {
            durable: true,
            ..Default::default()
        };
        assert!(!durable_only.expresses_selection_intent());
        let seeded =
            pin_static_provider_if_inert(Some(durable_only), Provider::Glm, None, None).unwrap();
        assert_eq!(seeded.pin.and_then(|p| p.provider), Some(Provider::Glm));

        // No request at all stays None (never reaches the allocator).
        assert!(pin_static_provider_if_inert(None, Provider::Brodex, None, None).is_none());
    }

    #[test]
    fn exclude_providers_for_diversity_narrows_pool_and_resolves_distinct_lane() {
        with_provider_bins(|| {
            // Premium tiered request (the phase-decompose panel shape). On its
            // own it would resolve to the tiebreak winner (brodex, sorts first).
            let base = || RuntimeRequest {
                tier: Some("premium".to_string()),
                durable: true,
                ..Default::default()
            };
            let cfg = built_in_config();
            let ctx = AllocationContext {
                in_flight: BTreeMap::new(),
                ..Default::default()
            };

            // Member 1: no exclusion → tiebreak winner.
            let first = allocate(base(), &cfg, &BroConfig::default(), &ctx);
            assert!(first.trace.error.is_none());
            let p1 = first.lane.provider;

            // Member 2: exclude what member 1 took → a DIFFERENT premium lane.
            let r2 = exclude_providers_for_diversity(Some(base()), &[p1]).unwrap();
            assert!(
                r2.pool.as_ref().is_some_and(|p| !p.providers.contains(&p1)),
                "taken provider must be removed from the pool"
            );
            let second = allocate(r2, &cfg, &BroConfig::default(), &ctx);
            assert!(second.trace.error.is_none());
            assert_ne!(
                second.lane.provider, p1,
                "member 2 must spread off member 1's provider"
            );
            // Still a premium lane — tier is preserved, only the pool narrowed.
            assert_eq!(second.lane.tier.as_deref(), Some("premium"));
        });
    }

    #[test]
    fn exclude_providers_for_diversity_is_soft_and_inert_safe() {
        // No-op cases: None request, empty exclude.
        assert!(exclude_providers_for_diversity(None, &[Provider::Brodex]).is_none());
        let untouched =
            exclude_providers_for_diversity(Some(RuntimeRequest::default()), &[]).unwrap();
        assert!(
            untouched.pool.is_none(),
            "empty exclude must not set a pool"
        );

        // Soft floor: excluding every provider does NOT empty the pool — it
        // falls back to allowing repeats rather than stranding the dispatch.
        let all: Vec<Provider> = Provider::ALL.to_vec();
        let req = RuntimeRequest {
            tier: Some("premium".to_string()),
            ..Default::default()
        };
        let result = exclude_providers_for_diversity(Some(req), &all).unwrap();
        // Pool left unset (no narrowing applied) since allowed would be empty.
        assert!(
            result.pool.is_none(),
            "excluding all providers must not produce an empty pool"
        );
    }

    /// Regression for the `try_lock`-fails-on-contention bug that used to
    /// surface `error.allocation_busy` to callers. Parks N tasks on a barrier
    /// so they all race for the lock at the same instant, holds it briefly
    /// inside the guarded section, and asserts every task acquired and
    /// released with peak in-section concurrency of exactly 1. With the old
    /// `try_lock` shape, the loser(s) would error immediately.
    ///
    /// Tokio tasks on a multi-thread runtime rather than OS threads: the lock
    /// is a `tokio::sync::Mutex` since the fleetd cutover (the guard is held
    /// across the dispatch's awaits), so it can only be taken from async
    /// context. `tokio::time::sleep` inside the section keeps the hold
    /// genuinely overlapping in wall-clock terms without blocking a worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn acquire_allocation_lock_queues_concurrent_callers() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        const TASKS: usize = 8;
        let barrier = Arc::new(tokio::sync::Barrier::new(TASKS));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));
        let acquired = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..TASKS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let in_flight = Arc::clone(&in_flight);
                let max_observed = Arc::clone(&max_observed);
                let acquired = Arc::clone(&acquired);
                tokio::spawn(async move {
                    // Park every task at the same point so the acquisition is
                    // genuinely contended rather than staggered by spawn
                    // latency.
                    barrier.wait().await;
                    let _guard = acquire_allocation_lock().await;
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    // Peak concurrency inside the guarded section; mutual
                    // exclusion means this must stay at 1.
                    max_observed.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    acquired.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

        for handle in handles {
            handle
                .await
                .expect("task panicked acquiring allocation lock");
        }

        assert_eq!(
            acquired.load(Ordering::SeqCst),
            TASKS,
            "every caller should successfully acquire the allocation lock"
        );
        assert_eq!(
            max_observed.load(Ordering::SeqCst),
            1,
            "allocation lock must serialize callers"
        );
    }
}
