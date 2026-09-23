use std::collections::BTreeMap;

use bbox_config::config::ProducerScopeClaimPolicy;
use sha2::{Digest, Sha256};

use super::SharedState;

pub(crate) const ONBOARDING_SKILL_NAME: &str = "onboard-project";
pub(crate) const ONBOARDING_SKILL_DESCRIPTION: &str =
    "Register, onboard, or add a project to Blackbox from a local or remote checkout host.";
pub(crate) const ONBOARDING_SKILL_URI: &str = "blackbox://skills/onboard-project/SKILL.md";

pub(crate) fn render(state: &SharedState) -> String {
    let config = state.config.read();
    let policies = config
        .code_collection
        .producers
        .iter()
        .map(|producer| (producer.producer_id.as_str(), producer))
        .collect::<BTreeMap<_, _>>();
    let presences = state.producer_commands.fresh_presences();

    let mut body = format!(
        "---\nname: {ONBOARDING_SKILL_NAME}\ndescription: {ONBOARDING_SKILL_DESCRIPTION}\n---\n\n\
         # Onboard a project\n\n\
         1. Call `bbox_project_register(path=<absolute path on the checkout host>)`. Pass `producer` only when the call returns `error.project_onboarding_ambiguous`.\n\
         2. If the receipt has `identity_committed: false`, commit exactly the returned `commit_paths` on `published_ref` in that checkout. Do not push unless the user asks. The collector reads the local ref.\n\
         3. Verify the project with `bbox_project_catalog_list` and `bbox_project_publisher_status`. Publication starts on the collector's next pass after the identity commit.\n\n\
         ## Checkout hosts\n\n"
    );

    if presences.is_empty() {
        body.push_str("No checkout-host collector has fresh presence.\n\n");
    } else {
        body.push_str(
            "| Producer | Host | Enroll roots | Config path | Service | Claims unclaimed scopes | Auto-publishes |\n\
             | --- | --- | --- | --- | --- | --- | --- |\n",
        );
        for producer in &presences {
            let policy = policies.get(producer.producer_id.as_str()).copied();
            let claims = policy.is_some_and(|producer| {
                producer.claim_scopes == ProducerScopeClaimPolicy::Unclaimed
            });
            let auto_publish = policy.is_some_and(|producer| producer.auto_publish);
            let roots = producer
                .presence
                .enroll_roots
                .iter()
                .map(|root| format!("`{}`", markdown_cell(root)))
                .collect::<Vec<_>>()
                .join("<br>");
            body.push_str(&format!(
                "| `{}` | {} | {} | `{}` | {} | {} | {} |\n",
                markdown_cell(&producer.producer_id),
                markdown_cell(&producer.presence.host_label),
                roots,
                markdown_cell(&producer.presence.config_path),
                producer
                    .presence
                    .service_label
                    .as_deref()
                    .map(markdown_cell)
                    .unwrap_or_else(|| "not configured".to_string()),
                yes_no(claims),
                yes_no(auto_publish),
            ));
        }
        body.push('\n');
        body.push_str("Enrollment is live-reloaded, so no service restart is needed.\n\n");

        for producer in &presences {
            body.push_str(&format!(
                "- `{}` fallback: `bbox-code-collector --config {} add <path>`.\n",
                producer.producer_id,
                shell_quote(&producer.presence.config_path),
            ));
            if let Some(service) = &producer.presence.service_label {
                body.push_str(&format!(
                    "  If the collector service needs recovery, restart `{}`.\n",
                    service
                ));
            }
        }
        body.push('\n');
    }

    body.push_str("## Producer policy\n\n");
    let mut configured = config.code_collection.producers.iter().collect::<Vec<_>>();
    configured.sort_by(|left, right| left.producer_id.cmp(&right.producer_id));
    if configured.is_empty() {
        body.push_str("No code-collection producers are configured.\n\n");
    } else {
        for producer in &configured {
            body.push_str(&format!(
                "- `{}`: claims unclaimed scopes: {}; auto-publishes: {}.\n",
                producer.producer_id,
                yes_no(producer.claim_scopes == ProducerScopeClaimPolicy::Unclaimed),
                yes_no(producer.auto_publish),
            ));
        }
        body.push('\n');
    }
    if !configured
        .iter()
        .any(|producer| producer.claim_scopes == ProducerScopeClaimPolicy::Unclaimed)
    {
        body.push_str(
            "No producer claims unclaimed scopes. The operator must pin a new scope in the daemon's producer config. Ask the operator to do that instead of editing daemon config.\n\n",
        );
    }

    body.push_str("## Host without a collector\n\n");
    body.push_str(
        "Ask the operator to install the collector, mint its token, and supply the daemon URL when it is not advertised. Write the minimal collector config:\n\n```toml\n",
    );
    match config.daemon.advertise_url.as_deref() {
        Some(url) => body.push_str(&format!("server_url = {:?}\n", url)),
        None => body.push_str("server_url = \"<operator-supplied daemon URL>\"\n"),
    }
    body.push_str(
        "token_file = \"/path/to/operator-minted-token\"\nenroll_roots = [\"/absolute/checkout/root\"]\n```\n\n\
         Token minting is an operator action.\n",
    );
    body
}

pub(crate) fn digest(text: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(text.as_bytes())))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use bbox_code_source::{
        PRODUCER_COMMAND_SCHEMA_VERSION, ProducerCommandPollRequestV1, ProducerPresenceV1,
    };
    use bbox_config::config::CodeCollectionProducerConfig;

    use crate::server::SharedState;

    fn test_state() -> (tempfile::TempDir, Arc<SharedState>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        (dir, state)
    }

    fn install_producer(
        state: &SharedState,
        producer_id: &str,
        claim_scopes: ProducerScopeClaimPolicy,
        auto_publish: bool,
    ) {
        state
            .config
            .write()
            .code_collection
            .producers
            .push(CodeCollectionProducerConfig {
                producer_id: producer_id.to_string(),
                token_file: PathBuf::from("/tokens/collector"),
                token_files: Vec::new(),
                scopes: Vec::new(),
                claim_scopes,
                auto_publish,
            });
    }

    fn poll_presence(
        state: &SharedState,
        producer_id: &str,
        host_label: &str,
        service_label: Option<&str>,
    ) {
        state.producer_commands.poll(
            producer_id,
            ProducerCommandPollRequestV1 {
                schema_version: PRODUCER_COMMAND_SCHEMA_VERSION,
                presence: ProducerPresenceV1 {
                    enroll_roots: vec!["/srv/checkouts".to_string()],
                    host_label: host_label.to_string(),
                    config_path: format!("/etc/blackbox/{producer_id}.toml"),
                    service_label: service_label.map(str::to_string),
                    collector_version: "1.0.0".to_string(),
                },
            },
        );
    }

    #[test]
    fn digest_is_stable_sha256() {
        assert_eq!(
            digest("fixture"),
            "sha256:f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d"
        );
    }

    #[test]
    fn render_reflects_fresh_presence_policy_and_advertise_url() {
        let (_dir, state) = test_state();
        install_producer(
            &state,
            "producer-b",
            ProducerScopeClaimPolicy::Unclaimed,
            true,
        );
        install_producer(&state, "producer-a", ProducerScopeClaimPolicy::None, false);
        poll_presence(&state, "producer-b", "host-b", Some("collector-b.service"));
        poll_presence(&state, "producer-a", "host-a", None);
        state.config.write().daemon.advertise_url =
            Some("https://blackbox.example.test".to_string());

        let text = render(&state);
        assert!(text.starts_with("---\nname: onboard-project\ndescription:"));
        assert!(text.contains("https://blackbox.example.test"));
        assert!(text.contains("collector-b.service"));
        assert!(text.contains("`producer-b`: claims unclaimed scopes: yes; auto-publishes: yes."));
        assert!(text.find("producer-a").unwrap() < text.find("producer-b").unwrap());
        assert!(!text.contains("No producer claims unclaimed scopes."));
        assert_eq!(text, render(&state));
    }

    #[test]
    fn render_omits_stale_presence_and_explains_missing_claim_policy_and_url() {
        let (_dir, state) = test_state();
        install_producer(&state, "producer-a", ProducerScopeClaimPolicy::None, false);
        poll_presence(&state, "producer-a", "stale-host", None);
        state.producer_commands.age_presence_for_test(
            "producer-a",
            super::super::producer_commands::PRODUCER_PRESENCE_FRESH_SECS + 1,
        );

        let text = render(&state);
        assert!(!text.contains("stale-host"));
        assert!(text.contains("No checkout-host collector has fresh presence."));
        assert!(text.contains("No producer claims unclaimed scopes."));
        assert!(text.contains("server_url = \"<operator-supplied daemon URL>\""));
        assert!(text.contains("Token minting is an operator action."));
    }
}
