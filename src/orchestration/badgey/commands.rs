#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperCommand {
    ApplyProposal(String),
    RejectProposal(String),
    Dismiss,
    ExpandPath(String),
    RetryApply(String),
    RevertBrofileTo(String),
    TrustSubBro(String),
    BudgetExtend,
}

pub fn parse_command(prompt: &str) -> Option<WrapperCommand> {
    let normalized = prompt.trim();
    let lower = normalized.to_ascii_lowercase();
    match lower.as_str() {
        "dismiss" | "dismiss badgey" => return Some(WrapperCommand::Dismiss),
        "extend budget" | "budget extend" => return Some(WrapperCommand::BudgetExtend),
        _ => {}
    }

    if let Some(id) = parse_prefixed_proposal(&lower, "apply ") {
        return Some(WrapperCommand::ApplyProposal(id));
    }
    if let Some(id) = parse_prefixed_proposal(&lower, "reject ") {
        return Some(WrapperCommand::RejectProposal(id));
    }
    if let Some(id) = parse_prefixed_proposal(&lower, "retry apply ") {
        return Some(WrapperCommand::RetryApply(id));
    }
    if let Some(id) = parse_prefixed_path(&lower, "expand ") {
        return Some(WrapperCommand::ExpandPath(id));
    }
    if lower.starts_with("revert brofile to ") {
        let version = normalized["revert brofile to ".len()..].trim();
        if is_simple_token(version) {
            return Some(WrapperCommand::RevertBrofileTo(version.to_string()));
        }
    }
    if lower.starts_with("trust sub-bro ") {
        let label = normalized["trust sub-bro ".len()..].trim();
        if is_simple_token(label) {
            return Some(WrapperCommand::TrustSubBro(label.to_string()));
        }
    }
    None
}

fn parse_prefixed_proposal(input: &str, prefix: &str) -> Option<String> {
    let id = input.strip_prefix(prefix)?;
    parse_proposal_id(id)
}

fn parse_proposal_id(input: &str) -> Option<String> {
    let n = input.strip_prefix("p-")?;
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("P-{n}"))
}

fn parse_prefixed_path(input: &str, prefix: &str) -> Option<String> {
    let id = input.strip_prefix(prefix)?;
    let n = id.strip_prefix("bg-path-")?;
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("bg-path-{n}"))
}

fn is_simple_token(input: &str) -> bool {
    !input.is_empty()
        && input
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrapper_commands() {
        assert_eq!(
            parse_command("apply P-3"),
            Some(WrapperCommand::ApplyProposal("P-3".to_string()))
        );
        assert_eq!(
            parse_command("reject P-4"),
            Some(WrapperCommand::RejectProposal("P-4".to_string()))
        );
        assert_eq!(parse_command("dismiss"), Some(WrapperCommand::Dismiss));
        assert_eq!(
            parse_command("expand bg-path-9"),
            Some(WrapperCommand::ExpandPath("bg-path-9".to_string()))
        );
        assert_eq!(
            parse_command("retry apply P-2"),
            Some(WrapperCommand::RetryApply("P-2".to_string()))
        );
        assert_eq!(
            parse_command("revert brofile to v1.2.3"),
            Some(WrapperCommand::RevertBrofileTo("v1.2.3".to_string()))
        );
        assert_eq!(
            parse_command("trust sub-bro A"),
            Some(WrapperCommand::TrustSubBro("A".to_string()))
        );
        assert_eq!(parse_command("extend budget"), Some(WrapperCommand::BudgetExtend));
    }

    #[test]
    fn near_misses_fall_through_to_badgey() {
        for prompt in [
            "please apply P-3",
            "apply proposal P-3",
            "apply P-3 now",
            "apply p-three",
            "expand path 1",
            "revert brofile",
            "trust sub-bro A because it seems right",
            "dismiss this concern",
            "dismiss P-3",
        ] {
            assert_eq!(parse_command(prompt), None, "{prompt}");
        }
    }
}
