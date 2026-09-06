use crate::workflow;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ArcSignalParams {
    /// Signal name to deliver (e.g. `pr-merged`, `pr-feedback`).
    pub(crate) signal: String,
    /// Correlation tuple to match against pending waits. A wait
    /// matches when every key/value here is present in the wait's
    /// registered correlation. Empty correlation = broadcast.
    #[serde(default)]
    pub(crate) correlate: Option<serde_json::Map<String, Value>>,
    /// Optional payload delivered to the resumed wait as
    /// `${last_signal.payload}`. When omitted, the correlation
    /// tuple is used (legacy default — kept so callers that don't
    /// have a payload don't need to manufacture one).
    #[serde(default)]
    pub(crate) payload: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArcStatusParams {
    /// Optional arcId or arc_thread_id. These are distinct accepted identities.
    /// Omit to list active/recent arcs in deterministic arc-id order.
    #[serde(default)]
    pub(crate) arc_id: Option<String>,
    /// Summary arc list size: default 10, maximum 20. Not used with arc_id.
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Summary list offset; continue with next_offset. Not used with arc_id.
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    /// summary (default) returns current position and bounded wait previews;
    /// full returns exact selected snapshots and waits as JSON body pages.
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// Opaque body.next_cursor from detail=full, with the same arc selector.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Exact body bytes per page, 4..4096; JSON escaping may reduce the page.
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ArcCancelParams {
    /// Arc id (from `WorkflowRunResult.arc_id` / arc_thread_id) to cancel.
    pub(crate) arc_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArcResultParams {
    /// Arc id (`arcId` from bro_orchestrate_run) or the workflow task id.
    pub(crate) arc_id: String,
    /// Optional var names to return. When omitted, all final vars are
    /// selected. `_structured_exit` is surfaced separately and omitted from
    /// default vars to avoid duplication; an explicit key request retains it.
    /// Large selections require detail=full to retrieve exact JSON pages.
    #[serde(default)]
    pub(crate) keys: Option<Vec<String>>,
    /// Include per-node prose outputs (can be large). Default false.
    #[serde(default)]
    pub(crate) include_node_outputs: Option<bool>,
    /// summary (default) returns small selected results inline; large results
    /// explicitly preview available fields. full returns exact selected JSON
    /// in body.text pages; concatenate pages before parsing.
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// Opaque body.next_cursor; keep arc_id, keys and include_node_outputs.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Exact body bytes per page, 4..4096; JSON escaping may reduce the page.
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WebhookReplayParams {
    /// Installed webhook name to replay against.
    pub(crate) name: String,
    /// Webhook body payload (the JSON Forgejo / GitHub / etc. would
    /// post). Top-level fields are extractor inputs.
    pub(crate) body: Value,
    /// Optional inbound headers; keys are lowercased before extractor
    /// projection. Use this to provide event-type / delivery-id
    /// headers (e.g. `{"x-gitea-event": "pull_request"}`) when the
    /// extractor reads from `$._headers.<name>`.
    #[serde(default)]
    pub(crate) headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WebhookDeliveriesParams {
    /// Filter to a specific webhook name.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Filter to deliveries at or after this ISO 8601 timestamp.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Filter by routing verdict classification.
    #[serde(default)]
    pub(crate) verdict_classification: Option<String>,
    /// Max deliveries returned, newest-first. Default 50, max = buffer cap.
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SignalsParams {
    /// Filter to a specific signal name (e.g. `pr-ready`). Exact match.
    #[serde(default)]
    pub(crate) signal: Option<String>,
    /// Filter to events at or after this ISO 8601 timestamp.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Filter by outcome: `matched` or `no_matching_wait`.
    #[serde(default)]
    pub(crate) outcome: Option<String>,
    /// Max events returned, newest-first. Default 50, max = buffer cap.
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WebhookInstallParams {
    /// Full WebhookSpec JSON (name, signature, extractor, routing_packet).
    #[schemars(with = "crate::webhooks::WebhookSpec")]
    pub(crate) spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct PollerInstallParams {
    /// Full PollerSpec JSON (name, every_seconds, source, optional
    /// iterate, extractor, optional dedup_id_path, routing_packet,
    /// optional default_project_dir).
    #[schemars(with = "crate::pollers::PollerSpec")]
    pub(crate) spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WebhookRemoveParams {
    /// Webhook name (as passed to bro_webhook_install).
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct PollerRemoveParams {
    /// Poller name (as passed to bro_poller_install).
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CronInstallParams {
    /// Full CronSpec JSON (name, schedule, optional payload, optional
    /// concurrency cap, routing_packet, optional default_project_dir and tz).
    #[schemars(with = "crate::crons::CronSpec")]
    pub(crate) spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CronUpcomingParams {
    /// Cron schedule expression (6-field `sec min hour dom mon dow`).
    pub(crate) schedule: String,
    /// How many upcoming times to return. Default 5, max 100.
    #[serde(default)]
    pub(crate) count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CronRemoveParams {
    /// Cron name (as passed to bro_cron_install).
    pub(crate) name: String,
}

// ── Whiteboard params ──────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardOpenParams {
    /// Stable id for the board. Often `<workflow-name>:<arc_id>` or a
    /// deliberation-specific slug ("adr-2026-04-27").
    pub(crate) board_id: String,
    /// Free-form topic — surfaces in inbox + agent prompts.
    pub(crate) topic: String,
    /// Project root associated with this board. Used for inbox
    /// scoping; doesn't affect storage path.
    #[serde(default)]
    pub(crate) project: Option<String>,
    /// Optional arc thread id binding the board to a specific arc.
    /// Set when the engine opens the board on behalf of an arc; absent
    /// when external clients open ad-hoc boards.
    #[serde(default)]
    pub(crate) arc_thread_id: Option<String>,
    /// Agent name credited with opening (auto-registered as facilitator).
    pub(crate) opened_by: String,
    /// Domain hint for the opener's role (default: "facilitation").
    #[serde(default)]
    pub(crate) domain: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardRegisterParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// Role on the board: `specialist`, `facilitator`, or `operator`.
    pub(crate) role: String,
    pub(crate) domain: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardPostParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// Post type: `proposal`, `claim`, `concern`, `informational`.
    #[serde(rename = "type")]
    pub(crate) post_type: String,
    pub(crate) title: String,
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) target_file: Option<String>,
    #[serde(default)]
    pub(crate) target_location: Option<String>,
    /// Severity: `critical`, `high`, `medium`, `low`. Optional.
    #[serde(default)]
    pub(crate) severity: Option<String>,
    #[serde(default)]
    pub(crate) finding_refs: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) cascade_targets: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WhiteboardStateParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// summary (default) returns a bounded preview; full returns exact visible
    /// JSON in body.text pages. Concatenate pages, then parse JSON.
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// Opaque body.next_cursor; keep board_id, agent_name, and selectors.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Exact body bytes per page, 4..4096; escaping may reduce the page.
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
    /// Optional visible post id; restrict posts, annotations and votes to it.
    /// Unknown and invisible post ids both return not found.
    #[serde(default)]
    pub(crate) post_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardAnnotateParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    pub(crate) post_id: String,
    /// Annotation type: `challenge`, `corroborate`, `resolve`, `validation`.
    #[serde(rename = "type")]
    pub(crate) annotation_type: String,
    pub(crate) body: String,
    /// Required for `validation`: `confirmed` / `refuted` / `inconclusive`.
    #[serde(default)]
    pub(crate) result: Option<String>,
    /// Required for `resolve`: id of the challenge annotation being resolved.
    #[serde(default)]
    pub(crate) resolves: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardTransitionParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// Target phase: `read`, `validate`, `debate`, `resolve`, `archived`.
    pub(crate) target_phase: String,
    #[serde(default)]
    pub(crate) summary: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardVoteParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    pub(crate) post_id: String,
    /// Vote: `accept`, `reject`, `defer`.
    pub(crate) vote: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WhiteboardConflictsParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// summary (default) returns a bounded preview; full returns exact visible
    /// JSON in body.text pages. Concatenate pages, then parse JSON.
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// Opaque body.next_cursor; keep board_id, agent_name, and selectors.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Exact body bytes per page, 4..4096; escaping may reduce the page.
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WhiteboardSummarizeParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// summary (default) returns a bounded preview; full returns exact visible
    /// JSON in body.text pages. Concatenate pages, then parse JSON.
    #[serde(default)]
    pub(crate) detail: Option<String>,
    /// Opaque body.next_cursor; keep board_id, agent_name, and selectors.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Exact body bytes per page, 4..4096; escaping may reduce the page.
    #[serde(default)]
    pub(crate) body_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhiteboardArchiveParams {
    pub(crate) board_id: String,
    pub(crate) agent_name: String,
    /// Archive from ANY phase (abandon path for boards stranded
    /// mid-phase by a failed arc). Facilitator or operator role
    /// required. Default false = resolve phase only.
    #[serde(default)]
    pub(crate) force: bool,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WorkflowInstallParams {
    /// Optional id for registry lookup. Defaults to `spec.name`.
    #[serde(default)]
    pub(crate) id: Option<String>,
    /// Full Workflow spec JSON.
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub(crate) spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WorkflowRemoveParams {
    /// Registry id (as passed to, or defaulted by, bro_workflow_install).
    pub(crate) id: String,
    /// Remove even if non-terminal arcs for this workflow are still
    /// running. Default false (fail closed).
    #[serde(default)]
    pub(crate) force: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct OrchestrateRunParams {
    /// Full workflow spec (Workflow struct serialized as JSON). Must
    /// contain `name`, `version`, `actors`, `nodes`, and `graph` (an
    /// per-node `next` transitions). Optional `policy_packet` for
    /// arc-level advisor-as-packet evaluation.
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub(crate) workflow: Value,
    /// Working directory passed to every dispatched bro.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Cap on activity-node steps (default: 50).
    #[serde(default)]
    pub(crate) max_steps: Option<usize>,
    /// When true: parse + cross-validate + summarize; do NOT dispatch.
    #[serde(default)]
    pub(crate) dry_run: Option<bool>,
    /// Optional initial vars seeded into the arc's ArcContext at
    /// start. Schema-validated against `Workflow.vars_schema` before
    /// the run begins.
    #[serde(default)]
    pub(crate) initial_vars: Option<serde_json::Map<String, Value>>,
    /// When true, block until the workflow task reaches a terminal
    /// state and return the same shape as `bro_wait`. Default false:
    /// return immediately with taskId + arcId, then poll via
    /// `bro_status` / await via `bro_wait`.
    #[serde(default)]
    pub(crate) await_completion: Option<bool>,
    /// Optional timeout used only when `await_completion=true`.
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct OrchestrateAuthorParams {
    /// Prose charter — what the arc should accomplish. Describe the
    /// actors, the phases, any gate/retry/halt conditions the author
    /// should encode. The compiler turns this into a validated
    /// workflow spec.
    pub(crate) charter: String,
    /// Brofile to dispatch as the authoring LLM. Should be a capable
    /// instruction-following model; Claude Haiku is usually sufficient.
    pub(crate) brofile: String,
    /// Optional shape hint — e.g. "crucible", "blind-convergence",
    /// "linear", "optimistic-review". If given, the authoring prompt
    /// suggests matching the named pattern.
    #[serde(default)]
    pub(crate) hint: Option<String>,
    /// Optional caller-supplied few-shot exemplars: complete workflow
    /// JSON specs (or spec fragments) the authoring LLM should imitate.
    /// These are appended after the built-in reference example, so a
    /// caller with an established house grammar (recipe workflows,
    /// domain-specific gate/packet idioms) gets specs in that grammar
    /// instead of generic shapes. At most 16 exemplars; the exemplars
    /// and preamble RENDER into a combined 64KB budget (framing labels
    /// included) and oversize input is rejected, not truncated.
    #[serde(default)]
    pub(crate) exemplars: Option<Vec<String>>,
    /// Optional domain preamble injected verbatim ahead of the charter:
    /// vocabulary, conventions, catalog context the compiler should
    /// treat as ground truth while authoring.
    #[serde(default)]
    pub(crate) preamble: Option<String>,
    /// Working directory for the authoring dispatch.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
}

// ---------------------------------------------------------------------------
// Bro roster endpoint — resolves selectors to concrete per-bro lane info
// (provider, session_id, transcript file path). Consumed by `bro tail`
// to know WHICH JSONL files to open and follow.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct RosterQuery {
    /// Comma-separated bro names (union of matches across all teams)
    #[serde(default)]
    pub(crate) bros: Option<String>,
    /// Comma-separated team names (each contributes all members). Accepts
    /// legacy `team=` singular form as an alias.
    #[serde(default, alias = "team")]
    pub(crate) teams: Option<String>,
    /// Comma-separated session IDs — synthetic adhoc lanes bypassing team membership.
    #[serde(default, alias = "session")]
    pub(crate) sessions: Option<String>,
    /// Comma-separated provider names (claude/codex/gemini/copilot/vibe) — final filter.
    #[serde(default, alias = "provider")]
    pub(crate) providers: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct BroRosterEntry {
    pub(crate) bro: String,
    pub(crate) bro_selector: String,
    pub(crate) team: String,
    pub(crate) provider: String,
    pub(crate) account: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) jsonl_path: Option<String>,
    pub(crate) brofile: String,
    pub(crate) model: Option<String>,
}

/// Request body for POST `/orchestrate`. `workflow` is the parsed
/// workflow spec; `project_dir` is the working directory to pass to all
/// dispatched bros; `max_steps` caps the loop (defaults to 50 server-side).
#[derive(Debug, Deserialize)]
pub(crate) struct OrchestrateRequest {
    pub(crate) workflow: workflow::Workflow,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    #[serde(default)]
    pub(crate) max_steps: Option<usize>,
    /// When true, parse + cross-validate the workflow and return a
    /// textual plan — do not dispatch any bros. Intended as a pre-flight
    /// check before the real run.
    #[serde(default)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrchestrateStatusQuery {
    pub(crate) thread_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OrchestrateStatusResponse {
    pub(crate) thread_id: String,
    pub(crate) notes: Vec<Value>,
    /// Most recent `ANCHOR [...]` body — the rolling compaction summary
    /// emitted at each step boundary.
    pub(crate) latest_anchor: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct OrchestrateListEntry {
    pub(crate) thread_id: String,
    pub(crate) name: Option<String>,
    pub(crate) topic: String,
    pub(crate) status: String,
    pub(crate) created_at: String,
    pub(crate) last_activity: String,
    pub(crate) project: Option<String>,
    pub(crate) latest_anchor: Option<String>,
    pub(crate) final_status: Option<String>,
    pub(crate) note_count: usize,
}

#[cfg(test)]
mod trigger_schema_tests {
    use super::*;
    use serde_json::json;

    fn required(schema: &Value, names: &[&str]) {
        let actual = schema["required"].as_array().expect("required fields");
        for name in names {
            assert!(
                actual.iter().any(|value| value.as_str() == Some(*name)),
                "missing required {name}: {schema}"
            );
        }
    }

    #[test]
    fn trigger_install_schemas_expose_required_specs_and_nested_selector_contracts() {
        let cron = serde_json::to_value(rmcp::schemars::schema_for!(CronInstallParams)).unwrap();
        let poller =
            serde_json::to_value(rmcp::schemars::schema_for!(PollerInstallParams)).unwrap();
        let webhook =
            serde_json::to_value(rmcp::schemars::schema_for!(WebhookInstallParams)).unwrap();
        for (root, definition, fields) in [
            (
                &cron,
                "CronSpec",
                vec!["name", "schedule", "routing_packet"],
            ),
            (
                &poller,
                "PollerSpec",
                vec![
                    "name",
                    "every_seconds",
                    "source",
                    "extractor",
                    "routing_packet",
                ],
            ),
            (
                &webhook,
                "WebhookSpec",
                vec!["name", "signature", "extractor", "routing_packet"],
            ),
        ] {
            required(root, &["spec"]);
            assert!(
                root["properties"]["spec"]
                    .to_string()
                    .contains(&format!("#/definitions/{definition}"))
            );
            required(&root["definitions"][definition], &fields);
        }
        let definitions = &poller["definitions"];
        required(&definitions["HttpFetchSpec"], &["url"]);
        assert_eq!(
            definitions["HttpFetchSpec"]["properties"]["method"]["default"],
            "GET"
        );
        assert_eq!(
            definitions["RetrySpec"]["properties"]["attempts"]["default"],
            3
        );
        assert_eq!(
            definitions["ResponseKind"]["enum"],
            json!(["json", "text", "auto"])
        );
        required(&definitions["Extractor"], &["outputs"]);
        let selectors = definitions["Selector"]["oneOf"].as_array().unwrap();
        for kind in ["json_path", "const", "default", "concat", "coalesce"] {
            assert!(
                selectors
                    .iter()
                    .any(|variant| variant["properties"]["kind"]["enum"] == json!([kind]))
            );
        }
        let path = selectors
            .iter()
            .find(|variant| variant["properties"]["kind"]["enum"] == json!(["json_path"]))
            .unwrap();
        required(path, &["kind", "path"]);
        let signatures = webhook["definitions"]["SignatureScheme"]["oneOf"]
            .as_array()
            .unwrap();
        let hmac = signatures
            .iter()
            .find(|variant| variant["properties"]["kind"]["enum"] == json!(["hmac_sha256"]))
            .unwrap();
        required(hmac, &["kind", "secret_env", "header"]);
        let timezone = &cron["definitions"]["CronSpec"]["properties"]["tz"];
        assert!(timezone.to_string().contains("[Uu][Tt][Cc]"));
    }

    #[test]
    fn trigger_schema_annotations_preserve_value_and_stringified_spec_parsing() {
        let spec = json!({"name":"example", "schedule":"0 0 9 * * *", "routing_packet":"example-routing", "tz":"UTC"});
        for value in [spec.clone(), Value::String(spec.to_string())] {
            let params: CronInstallParams = serde_json::from_value(json!({"spec":value})).unwrap();
            let parsed = crate::server::BlackboxServer::parse_spec::<crate::crons::CronSpec>(
                params.spec,
                "cron",
            )
            .unwrap();
            assert!(parsed.validate().is_ok());
        }
    }
}
