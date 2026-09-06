//! Vector maintenance operations independent of workflow execution.
//! Graph construction runs outside the partition lock; stale snapshots are
//! deferred. Publication still locks for WAL/derived-file persistence.
//! Diagnostic deadlines do not bound rebuild duration.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

const DEFAULT_DIAGNOSTIC_DEADLINE_MS: u64 = 2_000;

fn diagnostic_timeout(args: &Value) -> Duration {
    Duration::from_millis(
        args.get("diagnostic_deadline_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_DIAGNOSTIC_DEADLINE_MS)
            .clamp(1, 30_000),
    )
}

pub(crate) fn read_status(args: &Value) -> Result<Value> {
    let route_filter = args.get("route").and_then(|value| value.as_str());
    let mut partitions = crate::vectors::metrics_nonblocking()
        .ok_or_else(|| anyhow!("vector store is warming; retry maintenance later"))?;
    if let Some(route) = route_filter {
        partitions.retain(|name, _| name == route);
    }
    let diagnostic_routes = partitions.keys().cloned().collect::<Vec<_>>();
    let diagnostics =
        crate::vectors::diagnostics_bounded(&diagnostic_routes, diagnostic_timeout(args))?;
    let mut max_deleted_route = None::<String>;
    let mut max_deleted_ratio = 0.0f32;
    // Connectivity maxima honor the size floor: tiny-partition ratios are
    // noise and must not steer the compaction-policy gate (gap-1168b0bd).
    let mut max_connectivity_route = None::<String>;
    let mut max_connectivity_ratio = 0.0f32;
    for (route, metrics) in &partitions {
        if metrics.deleted_ratio >= max_deleted_ratio {
            max_deleted_ratio = metrics.deleted_ratio;
            max_deleted_route = Some(route.clone());
        }
        let Some(hnsw) = diagnostics
            .partitions
            .get(route)
            .and_then(|metrics| metrics.hnsw.as_ref())
        else {
            continue;
        };
        let connectivity = hnsw.connectivity_risk_ratio();
        if hnsw.connectivity_breach(0.0) && connectivity >= max_connectivity_ratio {
            max_connectivity_ratio = connectivity;
            max_connectivity_route = Some(route.clone());
        }
    }
    Ok(json!({
        "partitions": partitions,
        "max_deleted_route": max_deleted_route,
        "max_deleted_ratio": max_deleted_ratio,
        "max_connectivity_route": max_connectivity_route,
        "max_connectivity_ratio": max_connectivity_ratio,
        "connectivity_diagnostics_unavailable": diagnostics.unavailable,
    }))
}

pub(crate) fn rebuild(args: &Value) -> Result<()> {
    let route = args
        .get("route")
        .and_then(|value| value.as_str())
        .filter(|route| !route.trim().is_empty())
        .ok_or_else(|| anyhow!("RebuildHnsw requires args.route"))?;
    crate::vectors::rebuild(route)?;
    Ok(())
}

pub(crate) fn compact(args: &Value) -> Result<Value> {
    let max_partitions = args
        .get("max_partitions")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let deleted_ratio_threshold = args
        .get("deleted_ratio_threshold")
        .and_then(Value::as_f64)
        .unwrap_or(0.30) as f32;
    let connectivity_ratio_threshold = args
        .get("connectivity_ratio_threshold")
        .and_then(Value::as_f64)
        .map(|value| value as f32)
        .unwrap_or(bbox_vectors::COMPACT_CONNECTIVITY_RATIO);
    // A partition qualifies on EITHER axis: tombstone load (deleted_ratio)
    // or graph orphaning (connectivity_risk_ratio, gap-1168b0bd). Severity
    // for ordering is the worse of the two so a badly orphaned partition
    // is not starved behind moderately deleted ones.
    let cheap_metrics = crate::vectors::metrics_nonblocking()
        .ok_or_else(|| anyhow!("vector store is warming; retry maintenance later"))?;
    let diagnostic_routes = cheap_metrics.keys().cloned().collect::<Vec<_>>();
    let mut diagnostics =
        crate::vectors::diagnostics_bounded(&diagnostic_routes, diagnostic_timeout(args))?;
    let diagnostic_unavailable = diagnostics.unavailable.clone();
    let mut candidates = cheap_metrics
        .into_iter()
        .filter_map(|(route, metrics)| {
            let hnsw = diagnostics
                .partitions
                .remove(&route)
                .and_then(|metrics| metrics.hnsw);
            let connectivity_due = hnsw
                .as_ref()
                .is_some_and(|hnsw| hnsw.connectivity_breach(connectivity_ratio_threshold));
            let deleted_due = metrics.deleted_ratio >= deleted_ratio_threshold;
            if !deleted_due && !connectivity_due {
                return None;
            }
            let connectivity_ratio = hnsw
                .as_ref()
                .map(|hnsw| hnsw.connectivity_risk_ratio())
                .unwrap_or(0.0);
            let severity = metrics.deleted_ratio.max(connectivity_ratio);
            Some((route, severity, metrics, hnsw))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let limit = max_partitions.unwrap_or(usize::MAX);
    let mut stats = Vec::new();
    for (route, _, before, before_hnsw) in candidates.into_iter().take(limit) {
        let started = std::time::Instant::now();
        crate::vectors::rebuild(&route)?;
        let after = crate::vectors::metrics_nonblocking()
            .ok_or_else(|| anyhow!("vector store is warming; retry maintenance later"))?
            .remove(&route)
            .unwrap_or(before.clone());
        let after_diagnostics = crate::vectors::diagnostics_bounded(
            std::slice::from_ref(&route),
            diagnostic_timeout(args),
        )?;
        let after_hnsw = after_diagnostics
            .partitions
            .get(&route)
            .and_then(|metrics| metrics.hnsw.as_ref());
        stats.push(json!({
            "route": route,
            "before_wal_records": before.wal_records,
            "after_wal_records": after.wal_records,
            "before_slab_entries": before.active_count + before.deleted_count,
            "after_slab_entries": after.active_count + after.deleted_count,
            "before_connectivity_ratio": before_hnsw
                .as_ref()
                .map(|hnsw| hnsw.connectivity_risk_ratio()),
            "after_connectivity_ratio": after_hnsw
                .map(|hnsw| hnsw.connectivity_risk_ratio()),
            "after_connectivity_diagnostics_unavailable": after_diagnostics.unavailable,
            "elapsed_ms": started.elapsed().as_millis(),
        }));
    }
    let value = json!({
        "count": stats.len(),
        "routes": stats
            .iter()
            .filter_map(|stat| stat.get("route").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        "stats": stats,
        "connectivity_diagnostics_unavailable": diagnostic_unavailable,
    });
    Ok(value)
}
