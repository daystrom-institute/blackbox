//! Session-owned immutable embedding reports. Continuation never calls a producer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use parking_lot::Mutex;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::server::state::SharedState;

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct EmbedStatusParams {
    /// Include provider/model configuration and zero/null diagnostic counters.
    /// This only expands the reply; expensive scans and probes remain opt-in.
    #[serde(default)]
    pub debug: bool,
    /// Compute exact embedding coverage by walking every source document.
    /// Disabled by default because a production corpus scan can take minutes;
    /// queue/provider health remains available on the cheap path.
    #[serde(default)]
    pub include_coverage: Option<bool>,
    /// Include explicit HNSW graph diagnostics. Cheap status leaves graph
    /// fields absent; this opt-in walk can be expensive on large partitions.
    #[serde(default)]
    pub include_diagnostics: Option<bool>,
    /// Optional bounded vector-route set for graph diagnostics. When omitted,
    /// at most 64 currently loaded routes are inspected.
    #[serde(default)]
    pub diagnostic_routes: Option<Vec<String>>,
    /// Cooperative graph-diagnostic deadline in milliseconds (default 2000,
    /// clamped to 1..=30000).
    #[serde(default)]
    pub diagnostic_deadline_ms: Option<u64>,
    /// Optional vector route (partition name, e.g. "voyage-1024") to run a
    /// sampled HNSW self-recall probe against (gap-1168b0bd c). The probe is
    /// O(sample × search), seconds on large partitions — and errors with
    /// "busy" if the partition is mid-rebuild instead of blocking.
    #[serde(default)]
    pub recall_probe_route: Option<String>,
    /// Probe every Nth active vector (default 50). Lower is more accurate
    /// and proportionally slower.
    #[serde(default)]
    pub probe_sample_every: Option<usize>,
    /// Top-k window the probed vector must appear in (default 10).
    #[serde(default)]
    pub probe_k: Option<usize>,
    /// Continue the immutable report captured by this MCP session. Repeat the
    /// same scan/probe selectors; continuation never collects health again.
    /// Snapshots expire after ten minutes or eviction by newer reports.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Exact JSON body bytes per page (4..=4096). Forces snapshot capture even
    /// for small reports. Oversized replies automatically enter this mode.
    #[serde(default)]
    pub body_limit: Option<usize>,
}

const TTL: Duration = Duration::from_secs(600);
const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPORTS: usize = 4;

struct Snapshot {
    id: String,
    selection: String,
    body: String,
    captured_at: String,
    status: Option<String>,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct StatusSnapshots {
    reports: VecDeque<Snapshot>,
}

impl EmbedStatusParams {
    fn validate(&self) -> Result<()> {
        if self.body_limit.is_some_and(|n| !(4..=4096).contains(&n)) {
            bail!("error.bad_input: body_limit must be 4..=4096");
        }
        if self.cursor.as_ref().is_some_and(|c| c.len() > 128) {
            bail!("error.bad_input: invalid snapshot cursor");
        }
        if let Some(routes) = &self.diagnostic_routes {
            if routes.is_empty()
                || routes.len() > 64
                || routes.iter().any(|s| s.trim().is_empty() || s.len() > 256)
            {
                bail!(
                    "error.bad_input: diagnostic_routes requires 1..=64 nonempty names of at most 256 bytes"
                );
            }
            if self.include_diagnostics == Some(false) {
                bail!("error.bad_input: diagnostic_routes contradicts include_diagnostics=false");
            }
        }
        if self.diagnostic_deadline_ms.is_some()
            && !self.include_diagnostics.unwrap_or(false)
            && self.diagnostic_routes.is_none()
        {
            bail!("error.bad_input: diagnostic_deadline_ms requires diagnostics");
        }
        if self
            .recall_probe_route
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 256)
        {
            bail!(
                "error.bad_input: recall_probe_route requires a nonempty name of at most 256 bytes"
            );
        }
        if (self.probe_sample_every.is_some() || self.probe_k.is_some())
            && self.recall_probe_route.is_none()
        {
            bail!("error.bad_input: probe controls require recall_probe_route");
        }
        Ok(())
    }

    fn selection(&self) -> Result<String> {
        let mut value = serde_json::to_value(self)?;
        let object = value.as_object_mut().unwrap();
        object.remove("cursor");
        object.remove("body_limit");
        Ok(serde_json::to_string(&value)?)
    }
}

/// Capture once, then page immutable bytes under the owning MCP session.
/// Collection happens outside the cache lock. Failed admission never evicts
/// an existing report, and cache misses never silently repeat expensive work.
pub(crate) fn read_status(
    cache: &Mutex<StatusSnapshots>,
    p: &EmbedStatusParams,
    collect: impl FnOnce() -> Result<Value>,
) -> Result<String> {
    p.validate()?;
    let selection = p.selection()?;
    if let Some(cursor) = &p.cursor {
        let (id, offset) = cursor
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("error.bad_input: use body.next_cursor"))?;
        let offset = offset
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("error.bad_input: invalid snapshot cursor offset"))?;
        let mut cache = cache.lock();
        cache.reports.retain(|r| r.created.elapsed() < TTL);
        let report = cache.reports.iter().find(|r| r.id == id).ok_or_else(|| anyhow::anyhow!(
            "error.snapshot_unavailable: report expired, was evicted, or belongs to another MCP session; remove cursor to explicitly collect a new report"
        ))?;
        if report.selection != selection {
            bail!("error.bad_input: snapshot selectors changed; repeat the original selectors");
        }
        return page(report, offset, p.body_limit);
    }
    let value = collect()?;
    let body = serde_json::to_string(&value)?;
    // Budget both representations even though this adapter currently emits text.
    let envelope = json!({"content":[{"type":"text","text": &body}],"structuredContent":value,"isError":false});
    if p.body_limit.is_none()
        && serde_json::to_vec(&envelope)?.len()
            <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
    {
        return Ok(body);
    }
    if body.len() > MAX_REPORT_BYTES {
        bail!(
            "error.snapshot_too_large: collection completed but the report exceeds the 8 MiB snapshot limit; no snapshot stored; explicitly narrow diagnostic_routes or scan/probe opt-ins before collecting again"
        );
    }
    let report = Snapshot {
        id: uuid::Uuid::new_v4().simple().to_string(),
        selection,
        status: value
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 64)
            .map(str::to_owned),
        body,
        captured_at: chrono::Utc::now().to_rfc3339(),
        created: Instant::now(),
    };
    let first = page(&report, 0, p.body_limit)?;
    let mut cache = cache.lock();
    cache.reports.retain(|r| r.created.elapsed() < TTL);
    while cache.reports.len() >= MAX_REPORTS
        || cache.reports.iter().map(|r| r.body.len()).sum::<usize>() + report.body.len()
            > MAX_CACHE_BYTES
    {
        cache.reports.pop_front();
    }
    cache.reports.push_back(report);
    Ok(first)
}

fn page(report: &Snapshot, offset: usize, limit: Option<usize>) -> Result<String> {
    if offset > report.body.len() || !report.body.is_char_boundary(offset) {
        bail!("error.bad_input: invalid snapshot cursor byte boundary");
    }
    let mut budget = limit.unwrap_or(4096);
    loop {
        let mut end = offset.saturating_add(budget).min(report.body.len());
        while !report.body.is_char_boundary(end) {
            end -= 1;
        }
        let mut value = json!({
            "snapshot": {"id":report.id,"captured_at":report.captured_at,"ttl_seconds":TTL.as_secs(),"session_scoped":true,"immutable":true},
            "body": {"format":"json","text":&report.body[offset..end],"offset":offset,"total_bytes":report.body.len(),"next_cursor":(end < report.body.len()).then(|| format!("{}:{end}",report.id))},
            "continuation":"Concatenate body.text as JSON. Repeat the original selectors and body.next_cursor as cursor in this MCP session. Pages never repeat scans or probes. Retained up to ten minutes, four reports and 16 MiB per session; restart or eviction makes cursors unavailable.",
        });
        if let Some(status) = &report.status {
            value["status"] = json!(status);
        }
        let text = serde_json::to_string(&value)?;
        let envelope = json!({"content":[{"type":"text","text":&text}],"structuredContent":value,"isError":false});
        if serde_json::to_vec(&envelope)?.len()
            <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
            || budget <= 4
        {
            return Ok(text);
        }
        budget = (budget / 2).max(4);
    }
}

pub(crate) fn collect_status(state: &SharedState, p: &EmbedStatusParams) -> Result<Value> {
    let mut value: Value = serde_json::from_str(&super::status_json_for_state(
        state,
        p.include_coverage.unwrap_or(false),
        p.debug,
    )?)?;
    let diagnostics_requested =
        p.include_diagnostics.unwrap_or(false) || p.diagnostic_routes.is_some();
    if diagnostics_requested {
        let routes = p.diagnostic_routes.clone().unwrap_or_else(|| {
            crate::vectors::try_metrics()
                .map(|m| m.into_keys().take(64).collect())
                .unwrap_or_default()
        });
        let timeout =
            Duration::from_millis(p.diagnostic_deadline_ms.unwrap_or(2_000).clamp(1, 30_000));
        let report = match crate::vectors::try_diagnostics_bounded(&routes, timeout) {
            Some(report) => report.and_then(|report| Ok(serde_json::to_value(report)?)),
            None => Ok(
                json!({"partitions":{},"unavailable":[{"route":"<vector-store>","reason":"store_warming_up"}]}),
            ),
        };
        append_observation(&mut value, "vector_diagnostics", report);
    }
    if let Some(route) = p.recall_probe_route.as_deref() {
        let sample_every = p.probe_sample_every.unwrap_or(50).max(1);
        let k = p.probe_k.unwrap_or(10).max(1);
        let report = crate::vectors::self_recall_probe(route, sample_every, k).map(|self_recall| json!({"route":route,"sample_every":sample_every,"k":k,"self_recall":self_recall}));
        append_observation(&mut value, "recall_probe", report);
    }
    Ok(value)
}

// Later diagnostics must not discard completed status/coverage observations.
fn append_observation(value: &mut Value, key: &str, observation: Result<Value>) {
    value[key] = match observation {
        Ok(report) => report,
        Err(error) => {
            value["status"] = json!("error.embedding_observation_partial");
            json!({"state":"failed","error":error.to_string()})
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn parse(text: String) -> Value {
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn snapshot_recovers_unicode_without_repeating_producers_and_is_session_bound() {
        let cache = Mutex::new(StatusSnapshots::default());
        let other = Mutex::new(StatusSnapshots::default());
        let calls = Cell::new(0);
        let original =
            json!({"routes":{"synthetic":{"error":"界\n\"\\".repeat(6000)}},"coverage":42});
        let mut p = EmbedStatusParams {
            include_coverage: Some(true),
            debug: true,
            ..Default::default()
        };
        let mut page = parse(
            read_status(&cache, &p, || {
                calls.set(calls.get() + 1);
                Ok(original.clone())
            })
            .unwrap(),
        );
        let mut recovered = String::new();
        loop {
            let mirrored = json!({"content":[{"type":"text","text":page.to_string()}],"structuredContent":page});
            assert!(
                serde_json::to_vec(&mirrored).unwrap().len()
                    <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
            );
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            let Some(next) = page["body"]["next_cursor"].as_str() else {
                break;
            };
            p.cursor = Some(next.to_owned());
            assert!(
                read_status(&other, &p, || panic!(
                    "cross-session continuation collected"
                ))
                .unwrap_err()
                .to_string()
                .contains("snapshot_unavailable")
            );
            p.debug = false;
            assert!(read_status(&cache, &p, || panic!("changed selectors collected")).is_err());
            p.debug = true;
            page = parse(read_status(&cache, &p, || panic!("continuation collected")).unwrap());
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(serde_json::from_str::<Value>(&recovered).unwrap(), original);
    }

    #[test]
    fn snapshot_expiry_eviction_and_invalid_offsets_never_recollect() {
        let cache = Mutex::new(StatusSnapshots::default());
        let p = EmbedStatusParams {
            body_limit: Some(4),
            ..Default::default()
        };
        let first = parse(read_status(&cache, &p, || Ok(json!({"text":"界".repeat(50)}))).unwrap());
        let mut next = EmbedStatusParams {
            cursor: Some(first["body"]["next_cursor"].as_str().unwrap().into()),
            ..Default::default()
        };
        let id = first["snapshot"]["id"].as_str().unwrap();
        // JSON prefix has nine ASCII bytes; byte ten splits the first CJK codepoint.
        next.cursor = Some(format!("{id}:10"));
        assert!(read_status(&cache, &next, || panic!("invalid offset collected")).is_err());
        next.cursor = Some(format!("{id}:999999"));
        assert!(read_status(&cache, &next, || panic!("past end collected")).is_err());
        next.cursor = first["body"]["next_cursor"].as_str().map(str::to_owned);
        cache.lock().reports[0].created = Instant::now() - TTL;
        assert!(
            read_status(&cache, &next, || panic!("expiry collected"))
                .unwrap_err()
                .to_string()
                .contains("snapshot_unavailable")
        );
        let first = parse(read_status(&cache, &p, || Ok(json!({"value":1}))).unwrap());
        next.cursor = first["body"]["next_cursor"].as_str().map(str::to_owned);
        for n in 0..MAX_REPORTS {
            read_status(&cache, &p, || Ok(json!({"value":n}))).unwrap();
        }
        assert_eq!(cache.lock().reports.len(), MAX_REPORTS);
        assert!(read_status(&cache, &next, || panic!("eviction collected")).is_err());
    }

    #[test]
    fn later_probe_failure_preserves_completed_observations_and_paged_error_signal() {
        let cache = Mutex::new(StatusSnapshots::default());
        let mut value = json!({"routes":{"synthetic":{"source_count":31}},"vector_diagnostics":{"components":2}});
        append_observation(
            &mut value,
            "recall_probe",
            Err(anyhow::anyhow!("synthetic busy route")),
        );
        assert_eq!(value["routes"]["synthetic"]["source_count"], 31);
        assert_eq!(value["vector_diagnostics"]["components"], 2);
        assert_eq!(value["recall_probe"]["state"], "failed");
        let mut p = EmbedStatusParams {
            body_limit: Some(32),
            ..Default::default()
        };
        let mut page = parse(read_status(&cache, &p, || Ok(value.clone())).unwrap());
        let mut text = String::new();
        loop {
            let result = crate::server::BlackboxServer::ok_json(&page);
            assert_eq!(result.is_error, Some(true));
            text.push_str(page["body"]["text"].as_str().unwrap());
            p.cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if p.cursor.is_none() {
                break;
            }
            page = parse(read_status(&cache, &p, || panic!("failed probe repeated")).unwrap());
        }
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
    }

    #[test]
    fn snapshot_controls_validate_before_expensive_work() {
        let cache = Mutex::new(StatusSnapshots::default());
        for params in [
            json!({"body_limit":0}),
            json!({"body_limit":4097}),
            json!({"cursor":"x"}),
            json!({"diagnostic_routes":[]}),
            json!({"diagnostic_routes":[""]}),
            json!({"diagnostic_routes":vec!["synthetic";65]}),
            json!({"diagnostic_routes":["x".repeat(257)]}),
            json!({"diagnostic_routes":["synthetic"],"include_diagnostics":false}),
            json!({"diagnostic_deadline_ms":20}),
            json!({"probe_k":10}),
            json!({"probe_sample_every":2}),
            json!({"recall_probe_route":""}),
        ] {
            let p: EmbedStatusParams = serde_json::from_value(params).unwrap();
            assert!(read_status(&cache, &p, || panic!("invalid input collected")).is_err());
        }
        assert!(cache.lock().reports.is_empty());
    }

    #[test]
    fn snapshot_storage_is_bounded_without_evicting_on_failed_admission() {
        let cache = Mutex::new(StatusSnapshots::default());
        let p = EmbedStatusParams {
            body_limit: Some(4096),
            ..Default::default()
        };
        let first = parse(read_status(&cache, &p, || Ok(json!({"value":"original"}))).unwrap());
        let error = read_status(&cache, &p, || {
            Ok(json!({"body":"x".repeat(MAX_REPORT_BYTES)}))
        })
        .unwrap_err();
        assert!(error.to_string().contains("collection completed"));
        assert_eq!(
            cache.lock().reports[0].id,
            first["snapshot"]["id"].as_str().unwrap()
        );
        for _ in 0..3 {
            read_status(&cache, &p, || {
                Ok(json!({"body":"x".repeat(MAX_REPORT_BYTES-100)}))
            })
            .unwrap();
        }
        let cache = cache.lock();
        assert!(cache.reports.iter().map(|r| r.body.len()).sum::<usize>() <= MAX_CACHE_BYTES);
        assert_eq!(cache.reports.len(), 2);
    }

    #[test]
    fn small_status_keeps_existing_shape_and_session_clones_share_reports() {
        let cache = std::sync::Arc::new(Mutex::new(StatusSnapshots::default()));
        let value = json!({"queue_depth":0});
        assert_eq!(
            parse(
                read_status(&cache, &EmbedStatusParams::default(), || Ok(value.clone())).unwrap()
            ),
            value
        );
        assert!(cache.lock().reports.is_empty());
        let p = EmbedStatusParams {
            body_limit: Some(4),
            ..Default::default()
        };
        let first = parse(read_status(&cache, &p, || Ok(value)).unwrap());
        let p = EmbedStatusParams {
            cursor: first["body"]["next_cursor"].as_str().map(str::to_owned),
            ..Default::default()
        };
        assert!(read_status(&cache.clone(), &p, || panic!("clone collected")).is_ok());
    }
}
