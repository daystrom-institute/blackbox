use anyhow::Result;
use serde_json::json;

use super::{OpEffect, call_blackbox_tool};
use crate::workflow::context::ArcContext;

/// Observable marker for the index-drop node. Logs intent and returns None;
/// the actual document deletion is performed by the following
/// `SchemaMigrationRebuild` op via a full `bbox_reindex`.
pub(super) fn exec_schema_migration_drop(ctx: &ArcContext) -> OpEffect {
    tracing::info!(
        arc_id = %ctx.meta.arc_id,
        project = ?ctx.meta.project_dir,
        "schema_migration_drop: marking index for full rebuild"
    );
    OpEffect::None
}

/// Full tantivy rebuild via `bbox_reindex(full=true)`. Captures a JSON
/// summary into `vars[into_var]` when set.
pub(super) async fn exec_schema_migration_rebuild(
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let result = call_blackbox_tool("bbox_reindex", json!({"full": true}), ctx).await?;
    tracing::info!(
        arc_id = %ctx.meta.arc_id,
        "schema_migration_rebuild: full reindex complete"
    );
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result,
        }),
        None => Ok(OpEffect::None),
    }
}
