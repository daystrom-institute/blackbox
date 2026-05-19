use super::OpEffect;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};

pub(super) fn exec_read_vector_status(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.unwrap_or("vector_status");
    let route_filter = args.get("route").and_then(|value| value.as_str());
    let mut partitions = crate::vectors::metrics();
    if let Some(route) = route_filter {
        partitions.retain(|name, _| name == route);
    }
    let mut max_deleted_route = None::<String>;
    let mut max_deleted_ratio = 0.0f32;
    for (route, metrics) in &partitions {
        if metrics.deleted_ratio >= max_deleted_ratio {
            max_deleted_ratio = metrics.deleted_ratio;
            max_deleted_route = Some(route.clone());
        }
    }
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: json!({
            "partitions": partitions,
            "max_deleted_route": max_deleted_route,
            "max_deleted_ratio": max_deleted_ratio,
        }),
    })
}

pub(super) fn exec_rebuild_hnsw(args: &Value) -> Result<OpEffect> {
    let route = args
        .get("route")
        .and_then(|value| value.as_str())
        .filter(|route| !route.trim().is_empty())
        .ok_or_else(|| anyhow!("RebuildHnsw requires args.route"))?;
    crate::vectors::rebuild(route)?;
    Ok(OpEffect::None)
}

pub(super) fn exec_compact_vector_partitions(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    let max_partitions = args
        .get("max_partitions")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let deleted_ratio_threshold = args
        .get("deleted_ratio_threshold")
        .and_then(Value::as_f64)
        .unwrap_or(0.30) as f32;
    let mut candidates = crate::vectors::metrics()
        .into_iter()
        .filter(|(_, metrics)| metrics.deleted_ratio >= deleted_ratio_threshold)
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.1.deleted_ratio
            .total_cmp(&a.1.deleted_ratio)
            .then_with(|| a.0.cmp(&b.0))
    });
    let limit = max_partitions.unwrap_or(usize::MAX);
    let mut stats = Vec::new();
    for (route, before) in candidates.into_iter().take(limit) {
        let started = std::time::Instant::now();
        crate::vectors::rebuild(&route)?;
        let after = crate::vectors::metrics()
            .remove(&route)
            .unwrap_or(before.clone());
        stats.push(json!({
            "route": route,
            "before_wal_records": before.wal_records,
            "after_wal_records": after.wal_records,
            "before_slab_entries": before.active_count + before.deleted_count,
            "after_slab_entries": after.active_count + after.deleted_count,
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
    });
    if let Some(key) = into_var {
        return Ok(OpEffect::SetVar {
            key: key.to_string(),
            value,
        });
    }
    Ok(OpEffect::None)
}
