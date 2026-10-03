use super::*;
use std::time::Duration;

#[tokio::test]
async fn inference_steers_without_code_mode_preserve_fifo_and_clear_the_signal() {
    let (mut session, shared) = mk_session(vec![
        MockTurn::AwaitPreemption,
        MockTurn::Text("done".into()),
    ]);
    assert!(session.code_mode_session.is_none());
    let inputs = MidTurnInputs::new();
    let turn_inputs = inputs.clone();
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let turn = tokio::spawn(async move {
        session
            .user_turn("initial request", cancel_rx, turn_inputs)
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while shared.started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        inputs.push_back("first steer".into());
        inputs.push_back("second steer".into());
        turn.await.unwrap();
    })
    .await
    .expect("queued input must wake inference");
    let seen = shared.seen_users.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(!seen[0].iter().any(|text| text.contains("steer")));
    let steers = seen[1]
        .iter()
        .filter(|text| text.contains("steer"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(steers, vec!["first steer", "second steer"]);
    assert!(shared.inference_preemption.lock().unwrap().is_none());
}

#[tokio::test]
async fn inference_failure_clears_the_step_preemption_signal() {
    let (mut session, shared) = mk_session(vec![MockTurn::Failure]);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    assert!(
        session
            .user_turn("request", cancel_rx, MidTurnInputs::new())
            .await
            .is_err()
    );
    assert!(shared.inference_preemption.lock().unwrap().is_none());
}
