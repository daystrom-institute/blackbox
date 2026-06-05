use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use bro_capabilities::{
    CellLoadOutput, CellLoadRequest, CellRegisterOutput, CellRegisterRequest, CellScheduleOutput,
    CellScheduleRequest, DurableCellRegisterOutput, DurableCellRegisterRequest,
};
use chrono::Utc;
use cron::Schedule;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::artifacts::{ArtifactCatalog, ArtifactKind};
use crate::server::state::SharedState;

fn default_cell_tier() -> CellTier {
    CellTier::Reusable
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CellTier {
    Reusable,
    Durable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CellArtifact {
    pub kind: String,
    #[serde(default = "default_cell_tier")]
    pub tier: CellTier,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub handle: String,
    pub source: String,
    pub contract: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

pub(crate) fn register_cell(
    catalog: &ArtifactCatalog,
    request: CellRegisterRequest,
) -> Result<CellRegisterOutput> {
    let handle = cell_handle(&request.name, &request.version);
    let artifact = CellArtifact {
        kind: "cell".to_string(),
        tier: CellTier::Reusable,
        name: request.name.clone(),
        version: request.version.clone(),
        description: request.description,
        handle: handle.clone(),
        source: request.source,
        contract: request.contract_json,
        source_cell: None,
        supersedes: request.supersedes.clone(),
    };
    let value = serde_json::to_value(&artifact)?;
    let meta = catalog.install_value(
        ArtifactKind::Cell,
        "narf_register".to_string(),
        &value,
        Some(request.name),
        Some(request.version),
        request.supersedes,
    )?;
    Ok(CellRegisterOutput {
        handle,
        artifact_ref: format!("cell:{}@{}", meta.name, meta.version),
        name: meta.name,
        version: meta.version,
    })
}

pub(crate) fn register_durable_cell(
    catalog: &ArtifactCatalog,
    request: DurableCellRegisterRequest,
) -> Result<DurableCellRegisterOutput> {
    let source = load_cell(
        catalog,
        CellLoadRequest {
            handle: request.cell_handle.clone(),
        },
    )?;
    let handle = durable_cell_handle(&request.name, &request.version);
    let artifact = CellArtifact {
        kind: "cell".to_string(),
        tier: CellTier::Durable,
        name: request.name.clone(),
        version: request.version.clone(),
        description: request.description,
        handle: handle.clone(),
        source: source.source,
        contract: source.contract_json,
        source_cell: Some(source.handle.clone()),
        supersedes: request.supersedes.clone(),
    };
    let value = serde_json::to_value(&artifact)?;
    let meta = catalog.install_value(
        ArtifactKind::Cell,
        "narf_registerWorkflow".to_string(),
        &value,
        Some(request.name),
        Some(request.version),
        request.supersedes,
    )?;
    Ok(DurableCellRegisterOutput {
        handle,
        artifact_ref: format!("cell:{}@{}", meta.name, meta.version),
        name: meta.name,
        version: meta.version,
        source_cell: source.handle,
    })
}

pub(crate) fn load_cell(
    catalog: &ArtifactCatalog,
    request: CellLoadRequest,
) -> Result<CellLoadOutput> {
    let parsed = parse_cell_handle(&request.handle)?;
    let value = match parsed.version.as_deref() {
        Some(version) => {
            catalog.load_artifact_value_version(ArtifactKind::Cell, &parsed.name, version)?
        }
        None => catalog.load_artifact_value(ArtifactKind::Cell, &parsed.name)?,
    }
    .ok_or_else(|| anyhow!("registered cell `{}` not found", request.handle))?;
    let artifact: CellArtifact = serde_json::from_value(value)?;
    if artifact.kind != "cell" {
        bail!("artifact `{}` is not a cell", request.handle);
    }
    Ok(CellLoadOutput {
        handle: artifact.handle,
        artifact_ref: format!("cell:{}@{}", artifact.name, artifact.version),
        name: artifact.name,
        version: artifact.version,
        source: artifact.source,
        contract_json: artifact.contract,
    })
}

fn cell_handle(name: &str, version: &str) -> String {
    format!("atom:{name}@{version}")
}

fn durable_cell_handle(name: &str, version: &str) -> String {
    format!("cell:{name}@{version}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedCellHandle {
    name: String,
    version: Option<String>,
}

fn parse_cell_handle(handle: &str) -> Result<ParsedCellHandle> {
    let rest = handle
        .strip_prefix("atom:")
        .or_else(|| handle.strip_prefix("cell:"))
        .ok_or_else(|| anyhow!("cell handle must start with `atom:` or `cell:`"))?;
    let (name, version) = match rest.rsplit_once('@') {
        Some((name, version)) if !name.trim().is_empty() && !version.trim().is_empty() => {
            (name.to_string(), Some(version.to_string()))
        }
        None if !rest.trim().is_empty() => (rest.to_string(), None),
        _ => bail!("invalid cell handle `{handle}`"),
    };
    Ok(ParsedCellHandle { name, version })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CellScheduleSpec {
    pub kind: String,
    pub name: String,
    pub cell_handle: String,
    pub schedule: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
}

fn default_concurrency() -> u32 {
    1
}

#[derive(Default)]
struct ScheduleRunState {
    in_flight: u32,
}

#[derive(Default)]
pub(crate) struct CellScheduleRegistry {
    specs: RwLock<HashMap<String, CellScheduleSpec>>,
    handles: RwLock<HashMap<String, JoinHandle<()>>>,
    state: RwLock<HashMap<String, ScheduleRunState>>,
}

impl CellScheduleRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn install(&self, state: Arc<SharedState>, spec: CellScheduleSpec) {
        let name = spec.name.clone();
        self.specs.write().insert(name.clone(), spec.clone());
        if let Some(handle) = self
            .handles
            .write()
            .insert(name, spawn_schedule_loop(state, spec))
        {
            handle.abort();
        }
    }

    fn try_claim(&self, name: &str, cap: u32) -> bool {
        let mut state = self.state.write();
        let entry = state.entry(name.to_string()).or_default();
        if cap == 0 {
            entry.in_flight = entry.in_flight.saturating_add(1);
            return true;
        }
        if entry.in_flight >= cap {
            return false;
        }
        entry.in_flight += 1;
        true
    }

    fn mark_done(&self, name: &str) {
        if let Some(entry) = self.state.write().get_mut(name) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
    }
}

pub(crate) fn schedule_cell(
    state: Arc<SharedState>,
    request: CellScheduleRequest,
) -> Result<CellScheduleOutput> {
    validate_schedule(&request.schedule)?;
    let spec = CellScheduleSpec {
        kind: "cell_schedule".to_string(),
        name: request.name.clone(),
        cell_handle: request.cell_handle.clone(),
        schedule: request.schedule.clone(),
        input: request.input_json,
        concurrency: request.concurrency,
    };
    let dir = state.store_dir.join("cell-schedules");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", sanitize_schedule_name(&spec.name)?));
    crate::json_store::atomic_write_json_locked(&path, &spec)?;
    state.cell_schedules.install(state.clone(), spec);
    Ok(CellScheduleOutput {
        name: request.name,
        cell_handle: request.cell_handle,
        schedule: request.schedule,
        status: "scheduled".to_string(),
    })
}

pub(crate) fn load_schedules(dir: &std::path::Path) -> Vec<CellScheduleSpec> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))
            .and_then(|raw| {
                serde_json::from_str::<CellScheduleSpec>(&raw)
                    .with_context(|| format!("parsing {}", path.display()))
            }) {
            Ok(spec) => out.push(spec),
            Err(e) => tracing::warn!("cells::load_schedules: {e:#}"),
        }
    }
    out
}

pub(crate) fn restore_schedules(state: Arc<SharedState>) {
    let dir = state.store_dir.join("cell-schedules");
    for spec in load_schedules(&dir) {
        state.cell_schedules.install(state.clone(), spec);
    }
}

fn validate_schedule(expr: &str) -> Result<()> {
    Schedule::from_str(expr)
        .map(|_| ())
        .map_err(|e| anyhow!("cell schedule '{expr}': {e}"))
}

fn sanitize_schedule_name(name: &str) -> Result<String> {
    if name.trim().is_empty() {
        bail!("schedule name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        bail!("schedule name cannot contain path separators");
    }
    Ok(name.to_string())
}

fn spawn_schedule_loop(state: Arc<SharedState>, spec: CellScheduleSpec) -> JoinHandle<()> {
    tokio::spawn(async move {
        let schedule = match Schedule::from_str(&spec.schedule) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "cell schedule '{}': failed to parse schedule '{}': {e}",
                    spec.name,
                    spec.schedule
                );
                return;
            }
        };
        loop {
            let Some(next) = schedule.upcoming(Utc).next() else {
                tracing::warn!("cell schedule '{}': no next tick", spec.name);
                return;
            };
            let sleep_for = match (next - Utc::now()).to_std() {
                Ok(d) => d,
                Err(_) => std::time::Duration::from_secs(0),
            };
            tokio::time::sleep(sleep_for).await;
            if !state.cell_schedules.try_claim(&spec.name, spec.concurrency) {
                tracing::info!(
                    "cell schedule '{}': skipping tick; prior run active",
                    spec.name
                );
                continue;
            }
            let tick_at = Utc::now().to_rfc3339();
            let input = scheduled_input(&spec, tick_at);
            let run_state = state.clone();
            let run_name = spec.name.clone();
            let handle = spec.cell_handle.clone();
            tokio::spawn(async move {
                if let Err(e) = run_cell_once(run_state.clone(), &handle, input).await {
                    tracing::warn!("scheduled cell '{run_name}' failed: {e:#}");
                }
                run_state.cell_schedules.mark_done(&run_name);
            });
        }
    })
}

fn scheduled_input(spec: &CellScheduleSpec, tick_at: String) -> Value {
    match spec.input.clone() {
        Value::Object(mut map) => {
            map.insert(
                "schedule_name".to_string(),
                Value::String(spec.name.clone()),
            );
            map.insert("tick_at".to_string(), Value::String(tick_at));
            Value::Object(map)
        }
        other => json!({
            "schedule_name": spec.name,
            "tick_at": tick_at,
            "input": other,
        }),
    }
}

pub(crate) async fn run_cell_once(
    state: Arc<SharedState>,
    handle: &str,
    input_json: Value,
) -> Result<Value> {
    let loaded = load_cell(
        &state.artifacts.read(),
        CellLoadRequest {
            handle: handle.to_string(),
        },
    )?;
    let contract: bro_script::CellContract = serde_json::from_value(loaded.contract_json)
        .map_err(|e| anyhow!("cell contract parse: {e}"))?;
    let caps = bro_script::Capabilities {
        atoms: Arc::new(crate::orchestration::capabilities::DaemonAtoms {
            state: state.clone(),
        }),
        refactor: Arc::new(crate::orchestration::capabilities::DaemonRefactor::new(
            state.clone(),
        )),
        tools: None,
        kv: Arc::new(bro_harness::capabilities::KvStore::default()),
    };
    let runtime = bro_script::ScriptRuntime::new(caps, bro_script::SupervisionPolicy::default())
        .await
        .map_err(|e| anyhow!("cell runtime init: {e:#}"))?;
    let output = runtime
        .call_cell(loaded.source, contract, input_json)
        .await
        .map_err(|e| anyhow!("cell run failed: {e:#}"))?;
    serde_json::from_str(&output).or_else(|_| Ok(Value::String(output)))
}
