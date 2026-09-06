use super::OpEffect;
use anyhow::Result;
use serde_json::Value;

pub(super) fn exec_read_vector_status(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("vector_status").to_string(),
        value: crate::vector_maintenance::read_status(args)?,
    })
}

pub(super) fn exec_rebuild_hnsw(args: &Value) -> Result<OpEffect> {
    crate::vector_maintenance::rebuild(args)?;
    Ok(OpEffect::None)
}

pub(super) fn exec_compact_vector_partitions(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    let value = crate::vector_maintenance::compact(args)?;
    Ok(match into_var {
        Some(key) => OpEffect::SetVar {
            key: key.to_string(),
            value,
        },
        None => OpEffect::None,
    })
}
