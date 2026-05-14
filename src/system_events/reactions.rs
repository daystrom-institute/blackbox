use anyhow::{Result, bail};

use super::types::ReactionAction;

fn validate_reaction_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("reaction name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("reaction name cannot contain path separators");
    }
    if name == "." || name == ".." {
        bail!("reaction name cannot be '.' or '..'");
    }
    Ok(())
}

fn validate_secret_ref_in_value(val: &serde_json::Value) -> Result<()> {
    match val {
        serde_json::Value::String(s) => {
            let mut start = 0;
            while let Some(pos) = s[start..].find("secret:") {
                let abs = start + pos;
                let name_start = abs + "secret:".len();
                if name_start >= s.len() {
                    bail!("secret ref must have a name after 'secret:'");
                }
                let rest = &s[name_start..];
                let name_end = rest
                    .find(|c: char| c.is_whitespace() || c == ',' || c == '}' || c == '"')
                    .unwrap_or(rest.len());
                let name = &rest[..name_end];
                if name.is_empty() {
                    bail!("secret ref must have a name after 'secret:'");
                }
                if name.contains('/') || name.contains('\\') {
                    bail!("secret ref name cannot contain path separators: secret:{name}");
                }
                let re = regex::Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap();
                if !re.is_match(name) {
                    bail!("secret ref name must match [A-Za-z0-9_.-]+: secret:{name}");
                }
                start = name_start + name_end;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                validate_secret_ref_in_value(v)?;
            }
            Ok(())
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                validate_secret_ref_in_value(v)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn validate_reaction_spec(spec: &super::types::ReactionSpec) -> Result<()> {
    if spec.contract != "reaction/v1" {
        bail!(
            "unsupported contract: {} (expected reaction/v1)",
            spec.contract
        );
    }
    validate_reaction_name(&spec.name)?;
    if spec.event_kinds.is_empty() {
        bail!("event_kinds cannot be empty");
    }
    if spec.action.requires_idempotency_key() && spec.idempotency_key.is_none() {
        bail!(
            "action '{}' requires an idempotency_key",
            spec.action.op_name()
        );
    }
    if spec.retry.max_attempts == 0 {
        bail!("retry.max_attempts must be > 0");
    }
    if spec.retry.max_attempts > 20 {
        bail!(
            "retry.max_attempts ({}) exceeds maximum of 20",
            spec.retry.max_attempts
        );
    }
    match &spec.action {
        ReactionAction::HttpJson { args }
        | ReactionAction::McpCall { args }
        | ReactionAction::AtomInvoke { args }
        | ReactionAction::StartWorkflow { args }
        | ReactionAction::EmitEvent { args } => {
            validate_secret_ref_in_value(args)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_events::types::{FailurePolicy, ReactionSpec, RetryPolicy};
    use serde_json::json;

    fn valid_spec(action: ReactionAction, idem: Option<String>) -> ReactionSpec {
        ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "test-reaction".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: idem,
            action,
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        }
    }

    #[test]
    fn rejects_wrong_contract() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.contract = "reaction/v2".to_string();
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("unsupported contract")
        );
    }

    #[test]
    fn rejects_empty_name() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.name = String::new();
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("name cannot be empty")
        );
    }

    #[test]
    fn rejects_name_with_slash() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.name = "foo/bar".to_string();
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("path separators")
        );
    }

    #[test]
    fn rejects_empty_event_kinds() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.event_kinds = vec![];
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("event_kinds cannot be empty")
        );
    }

    #[test]
    fn rejects_missing_idempotency_key_for_http_json() {
        let spec = valid_spec(
            ReactionAction::HttpJson {
                args: json!({"url": "https://example.com"}),
            },
            None,
        );
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("idempotency_key")
        );
    }

    #[test]
    fn rejects_missing_idempotency_key_for_mcp_call() {
        let spec = valid_spec(
            ReactionAction::McpCall {
                args: json!({"tool": "test"}),
            },
            None,
        );
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("idempotency_key")
        );
    }

    #[test]
    fn rejects_missing_idempotency_key_for_atom_invoke() {
        let spec = valid_spec(
            ReactionAction::AtomInvoke {
                args: json!({"atom": "test"}),
            },
            None,
        );
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("idempotency_key")
        );
    }

    #[test]
    fn rejects_missing_idempotency_key_for_start_workflow() {
        let spec = valid_spec(
            ReactionAction::StartWorkflow {
                args: json!({"workflow": "test"}),
            },
            None,
        );
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("idempotency_key")
        );
    }

    #[test]
    fn accepts_emit_event_without_idempotency_key() {
        let spec = valid_spec(
            ReactionAction::EmitEvent {
                args: json!({"kind": "test.event"}),
            },
            None,
        );
        assert!(validate_reaction_spec(&spec).is_ok());
    }

    #[test]
    fn accepts_http_json_with_idempotency_key() {
        let spec = valid_spec(
            ReactionAction::HttpJson {
                args: json!({"url": "https://example.com"}),
            },
            Some("key:${event.id}".to_string()),
        );
        assert!(validate_reaction_spec(&spec).is_ok());
    }

    #[test]
    fn rejects_retry_max_zero() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.retry.max_attempts = 0;
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("max_attempts")
        );
    }

    #[test]
    fn rejects_retry_max_over_20() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.retry.max_attempts = 25;
        assert!(
            validate_reaction_spec(&spec)
                .unwrap_err()
                .to_string()
                .contains("max_attempts")
        );
    }

    #[test]
    fn accepts_retry_max_20() {
        let mut spec = valid_spec(ReactionAction::EmitEvent { args: json!({}) }, None);
        spec.retry.max_attempts = 20;
        assert!(validate_reaction_spec(&spec).is_ok());
    }

    #[test]
    fn rejects_invalid_secret_ref_in_action_args() {
        let spec = valid_spec(
            ReactionAction::HttpJson {
                args: json!({
                    "url": "https://example.com",
                    "headers": { "Authorization": "token secret:forgejo/foo" }
                }),
            },
            Some("key".to_string()),
        );
        let err = validate_reaction_spec(&spec).unwrap_err();
        assert!(err.to_string().contains("path separators"), "got: {err}");
    }

    #[test]
    fn accepts_valid_secret_ref_in_action_args() {
        let spec = valid_spec(
            ReactionAction::HttpJson {
                args: json!({
                    "url": "https://example.com",
                    "headers": { "Authorization": "token secret:forgejo-admin-token" }
                }),
            },
            Some("key".to_string()),
        );
        assert!(validate_reaction_spec(&spec).is_ok());
    }
}
