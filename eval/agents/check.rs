use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDiscoveryCase {
    pub query: String,
    #[serde(default)]
    pub expected_agents: Vec<String>,
    #[serde(default)]
    pub forbidden_agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCuingCase {
    pub agent: String,
    pub prompt: String,
    pub expect_dispatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSearchObservation {
    pub ranked_agents: Vec<String>,
    #[serde(default)]
    pub search_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCuingObservation {
    pub dispatched_agent: Option<String>,
}

pub fn check_discovery(
    case: &AgentDiscoveryCase,
    observed: &AgentSearchObservation,
) -> Vec<String> {
    let ranked: BTreeSet<&str> = observed.ranked_agents.iter().map(String::as_str).collect();
    let top3: BTreeSet<&str> = observed
        .ranked_agents
        .iter()
        .take(3)
        .map(String::as_str)
        .collect();
    let mut failures = Vec::new();
    for expected in &case.expected_agents {
        if !top3.contains(expected.as_str()) {
            failures.push(format!("missing_expected_agent:{expected}"));
        }
    }
    for forbidden in &case.forbidden_agents {
        if ranked.contains(forbidden.as_str()) {
            failures.push(format!("forbidden_agent_returned:{forbidden}"));
        }
    }
    if observed.search_mode.as_deref() == Some("keyword") {
        failures.push("search_mode=keyword".into());
    }
    failures
}

pub fn check_cuing(case: &AgentCuingCase, observed: &AgentCuingObservation) -> Vec<String> {
    match (case.expect_dispatch, observed.dispatched_agent.as_deref()) {
        (true, Some(agent)) if agent == case.agent => Vec::new(),
        (true, Some(agent)) => vec![format!("wrong_agent_dispatched:{agent}")],
        (true, None) => vec!["expected_dispatch_missing".into()],
        (false, Some(agent)) => vec![format!("unexpected_dispatch:{agent}")],
        (false, None) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_requires_expected_agent_in_top_three() {
        let failures = check_discovery(
            &AgentDiscoveryCase {
                query: "review".into(),
                expected_agents: vec!["code-reviewer".into()],
                forbidden_agents: vec![],
            },
            &AgentSearchObservation {
                ranked_agents: vec!["diff-narrator".into(), "code-reviewer".into()],
                search_mode: Some("hybrid".into()),
            },
        );
        assert!(failures.is_empty());
    }

    #[test]
    fn cuing_rejects_unexpected_dispatch() {
        let failures = check_cuing(
            &AgentCuingCase {
                agent: "code-reviewer".into(),
                prompt: "typo".into(),
                expect_dispatch: false,
            },
            &AgentCuingObservation {
                dispatched_agent: Some("code-reviewer".into()),
            },
        );
        assert_eq!(failures, vec!["unexpected_dispatch:code-reviewer"]);
    }
}
