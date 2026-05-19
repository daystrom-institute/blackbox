use super::OpEffect;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};

pub(super) fn exec_system_event_compact(
    args: &Value,
    into_var: Option<&str>,
    hub: &crate::system_events::SharedEventHub,
) -> Result<OpEffect> {
    let now = args
        .get("now")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(crate::util::now_iso);
    let report = hub.compact_with_now(&now)?;
    Ok(OpEffect::SetVar {
        key: into_var
            .unwrap_or("system_event_compaction_result")
            .to_string(),
        value: serde_json::to_value(report)?,
    })
}

pub(super) async fn exec_require_identity(
    args: &Value,
    into_var: Option<&str>,
    hub: &crate::system_events::SharedEventHub,
) -> Result<OpEffect> {
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.scope"))?;
    let instance = args
        .get("instance")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.instance"))?;
    let bro = args
        .get("bro")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.bro"))?;
    let provider = args
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.provider"))?;
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.model"))?;
    let effort = args
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let project = args
        .get("project")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let owner = args
        .get("owner")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let req = crate::system_events::identity::IdentityRequest {
        scope: scope.to_string(),
        instance: instance.to_string(),
        bro: bro.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        effort,
        project,
        owner,
        repo,
    };

    let into = into_var.unwrap_or("identity_result");
    match hub.require_identity(&req).await? {
        Some(identity) => Ok(OpEffect::SetVar {
            key: into.to_string(),
            value: json!({
                "status": "ready",
                "identity": serde_json::to_value(&identity)?
            }),
        }),
        None => Ok(OpEffect::SetVar {
            key: into.to_string(),
            value: json!({ "status": "pending", "identity": null }),
        }),
    }
}
