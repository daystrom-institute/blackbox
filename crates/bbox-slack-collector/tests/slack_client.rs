//! The allowlist Slack client against a fixture Slack API.
//!
//! These exercise the REAL client rather than a double, because everything
//! asserted here is transport behavior: cursor pagination, the `ok:false` error
//! channel that arrives with HTTP 200, `Retry-After` on a 429, and the fact that
//! no request path outside the read allowlist can be composed at all.

mod support;

use std::time::Instant;

use bbox_slack_collector::slack::{ChannelListRequest, HistoryRequest, RepliesRequest, SlackRead};
use bbox_slack_collector::{SlackClient, SlackReadMethod};
use support::{
    FakeChannel, FakeMessage, FakeSlack, FakeSlackState, fast_rate_policy, one_app_scopes,
    ts_before,
};

const CHANNEL: &str = "C0FIXTURE01";

fn client(base_url: &str) -> SlackClient {
    SlackClient::new(base_url, "xoxb-fixture-token", fast_rate_policy()).unwrap()
}

fn workspace(page_size: usize, messages: Vec<FakeMessage>) -> FakeSlackState {
    FakeSlackState {
        granted_scopes: one_app_scopes(),
        channels: vec![FakeChannel::public(CHANNEL, "engineering")],
        history: [(CHANNEL.to_string(), messages)].into_iter().collect(),
        page_size,
        ..FakeSlackState::default()
    }
}

fn history_request(max_pages: u32) -> HistoryRequest {
    HistoryRequest {
        channel_id: CHANNEL.to_string(),
        oldest: None,
        latest: None,
        page_limit: 2,
        max_pages,
    }
}

#[tokio::test]
async fn auth_test_captures_every_granted_scope_including_the_writes_it_cannot_refuse() {
    let slack = FakeSlack::start(workspace(10, Vec::new())).await;
    let identity = client(&slack.base_url).auth_test().await.unwrap();

    assert_eq!(identity.workspace_id, support::WORKSPACE_ID);
    assert_eq!(
        identity.workspace_domain.as_deref(),
        Some("fixture.slack.com")
    );
    assert_eq!(identity.bot_user_id.as_deref(), Some("U0BOT"));
    // The one-app posture in one assertion: the write scopes are visible and
    // recorded, because this collector reads with the interactive bot's token
    // and cannot demand a credential without them.
    assert!(identity.has_scope("channels:history"));
    assert_eq!(
        identity.write_scopes(),
        vec!["chat:write".to_string(), "reactions:write".to_string()]
    );
}

#[tokio::test]
async fn a_history_sweep_pages_a_window_to_completion() {
    let messages: Vec<FakeMessage> = (0..5)
        .map(|index| {
            FakeMessage::new(
                &ts_before(500 - index * 10, index as u32 + 1),
                "U0HUMAN",
                &format!("message {index}"),
            )
        })
        .collect();
    let slack = FakeSlack::start(workspace(2, messages)).await;

    let sweep = client(&slack.base_url)
        .history(&history_request(10))
        .await
        .unwrap();

    assert!(sweep.complete);
    assert_eq!(sweep.messages.len(), 5);
    assert_eq!(
        sweep.pages, 3,
        "five messages at two per page is three pages"
    );
    assert_eq!(slack.request_count("conversations.history"), 3);
}

#[tokio::test]
async fn a_sweep_that_runs_out_of_page_budget_reports_itself_incomplete() {
    // The property the whole window discipline rests on. History pages
    // newest-first, so a budget-truncated sweep holds the NEWEST messages, not a
    // contiguous run from the watermark. The client must say so; the cycle
    // refuses to land it.
    let messages: Vec<FakeMessage> = (0..6)
        .map(|index| {
            FakeMessage::new(
                &ts_before(600 - index * 10, index as u32 + 1),
                "U0HUMAN",
                "message",
            )
        })
        .collect();
    let slack = FakeSlack::start(workspace(2, messages)).await;

    let sweep = client(&slack.base_url)
        .history(&history_request(2))
        .await
        .unwrap();

    assert!(
        !sweep.complete,
        "a truncated sweep must not claim completion"
    );
    assert_eq!(sweep.pages, 2);
    assert_eq!(sweep.messages.len(), 4);
}

#[tokio::test]
async fn a_429_is_honored_for_its_full_retry_after_and_retried_once() {
    let mut state = workspace(
        10,
        vec![FakeMessage::new(&ts_before(60, 1), "U0HUMAN", "hi")],
    );
    state
        .throttle_once
        .insert("conversations.history".to_string(), 1);
    let slack = FakeSlack::start(state).await;
    let client = client(&slack.base_url);

    let started = Instant::now();
    let sweep = client.history(&history_request(10)).await.unwrap();
    let elapsed = started.elapsed();

    assert!(sweep.complete);
    assert_eq!(sweep.messages.len(), 1);
    // Honored: the wait was at least the second the vendor asked for. This is
    // the "no tight retry" clause, and it is asserted on the CLOCK rather than
    // on a counter because a counter cannot tell a backoff from a hot loop.
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "retried after only {elapsed:?}"
    );
    // Retried ONCE, not repeatedly: two calls total for one throttle.
    assert_eq!(slack.request_count("conversations.history"), 2);
    let stats = client.stats();
    assert_eq!(stats.throttled, 1);
    assert_eq!(stats.retries, 1);
    assert_eq!(stats.last_retry_after_secs, Some(1));
}

#[tokio::test]
async fn repeated_throttling_gives_up_rather_than_retrying_forever() {
    let mut state = workspace(10, Vec::new());
    // The fixture throttles once per method, so a policy of one attempt turns
    // the first 429 into a refusal. The point is that the budget is finite and
    // exhausting it ends the cycle instead of hammering a shared credential.
    state
        .throttle_once
        .insert("conversations.history".to_string(), 1);
    let slack = FakeSlack::start(state).await;
    let policy = bbox_slack_collector::RatePolicy {
        max_attempts: 1,
        ..fast_rate_policy()
    };
    let client = SlackClient::new(slack.base_url.as_str(), "xoxb-fixture-token", policy).unwrap();

    let error = client
        .history(&history_request(10))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("throttled"), "{error}");
    assert_eq!(slack.request_count("conversations.history"), 1);
}

#[tokio::test]
async fn an_ok_false_body_is_an_error_rather_than_an_empty_page() {
    // Slack reports errors with HTTP 200 and `ok: false`. Reading that as an
    // empty page would advance a watermark over messages never seen, which is
    // the failure mode that makes an ingestion lane look correct and not be.
    let mut state = workspace(
        10,
        vec![FakeMessage::new(&ts_before(60, 1), "U0HUMAN", "hi")],
    );
    state.fail_once.insert(
        "conversations.history".to_string(),
        "channel_not_found".to_string(),
    );
    let slack = FakeSlack::start(state).await;

    let error = client(&slack.base_url)
        .history(&history_request(10))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("channel_not_found"), "{error}");
}

#[tokio::test]
async fn a_replies_sweep_returns_the_parent_alongside_its_replies() {
    let parent_ts = ts_before(300, 1);
    let reply_one = ts_before(200, 2);
    let reply_two = ts_before(100, 3);
    let mut state = workspace(
        10,
        vec![FakeMessage::new(&parent_ts, "U0HUMAN", "thread starter").parent(2, &reply_two)],
    );
    state.replies.insert(
        (CHANNEL.to_string(), parent_ts.clone()),
        vec![
            FakeMessage::new(&reply_one, "U0HUMAN", "first reply").reply_to(&parent_ts),
            FakeMessage::new(&reply_two, "U0OTHER", "second reply").reply_to(&parent_ts),
        ],
    );
    let slack = FakeSlack::start(state).await;

    let sweep = client(&slack.base_url)
        .replies(&RepliesRequest {
            channel_id: CHANNEL.to_string(),
            parent_ts: parent_ts.clone(),
            oldest: None,
            page_limit: 10,
            max_pages: 5,
        })
        .await
        .unwrap();

    assert!(sweep.complete);
    // Parent plus two replies. The caller filters the parent by ts; a client
    // that silently dropped it would hide a real message the first time a
    // parent was observed only through a thread.
    assert_eq!(sweep.messages.len(), 3);
    assert_eq!(sweep.messages[0].ts.as_deref(), Some(parent_ts.as_str()));
}

#[tokio::test]
async fn the_client_never_composes_a_path_outside_the_read_allowlist() {
    let slack = FakeSlack::start(workspace(
        10,
        vec![FakeMessage::new(&ts_before(60, 1), "U0HUMAN", "hi")],
    ))
    .await;
    let client = client(&slack.base_url);

    client.auth_test().await.unwrap();
    client
        .list_channels(&ChannelListRequest {
            memberships_only: false,
            include_private: false,
            exclude_archived: true,
            page_limit: 100,
            max_pages: 5,
        })
        .await
        .unwrap();
    client.history(&history_request(10)).await.unwrap();

    let allowed: Vec<String> = SlackReadMethod::ALL
        .iter()
        .map(|method| format!("/api/{}", method.api_name()))
        .collect();
    let requests = slack.requests();
    assert!(!requests.is_empty());
    for path in &requests {
        assert!(
            allowed.contains(path),
            "{path} is outside the read allowlist"
        );
    }
    // And the fixture's catch-all, which would record a write attempt, saw
    // nothing at all.
    assert!(!requests.iter().any(|path| path.contains("chat.")));
}
