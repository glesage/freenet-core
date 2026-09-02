//! Contract operations through the in-process client, against a local node.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use freenet_mobile::MobileError;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_then_get_round_trips_state() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    let (wasm, params) = test_contract();
    let state = empty_todo_list();

    let key = node.put(wasm, params, state.clone(), false).await?;
    assert!(!key.is_empty(), "put must return the instance id");

    let got = node.get(key.clone(), false).await?;
    assert_eq!(got.key, key);
    assert_eq!(got.state, state);
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_of_unknown_contract_is_not_found() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    let err = node
        .get("6Sf2buCM1LzU5EhscNvjeNqPYbQbtvKSkzC6EFUy8Jjh".into(), false)
        .await
        .expect_err("unknown contract must not resolve");
    assert!(
        matches!(err, MobileError::NotFound(_) | MobileError::Request(_)),
        "unexpected error kind: {err}"
    );
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_key_is_rejected_before_hitting_the_node() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    let err = node
        .get("not a key".into(), false)
        .await
        .expect_err("garbage key must fail");
    assert!(matches!(err, MobileError::Request(_)), "{err}");
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_receives_update_notifications() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    let listener = Arc::new(RecordingListener::default());
    node.set_update_listener(listener.clone());

    let (wasm, params) = test_contract();
    let key = node.put(wasm, params, empty_todo_list(), true).await?;
    node.update_delta(key.clone(), add_task_delta(1, "first"))
        .await?;

    let updates = listener.wait_for_updates(1, Duration::from_secs(20)).await;
    assert_eq!(updates[0].key, key);
    assert!(
        updates[0].state.is_some() || updates[0].delta.is_some(),
        "a notification must carry a state or a delta: {:?}",
        updates[0]
    );

    // The new state is readable and reflects the update.
    let got = node.get(key.clone(), false).await?;
    let text = String::from_utf8_lossy(&got.state);
    assert!(
        text.contains("first"),
        "updated state must contain the task: {text}"
    );
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_subscribe_then_update_notifies() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    let listener = Arc::new(RecordingListener::default());
    node.set_update_listener(listener.clone());

    let (wasm, params) = test_contract();
    let key = node.put(wasm, params, empty_todo_list(), false).await?;
    node.subscribe(key.clone()).await?;
    node.update_delta(key.clone(), add_task_delta(7, "seventh"))
        .await?;

    let updates = listener.wait_for_updates(1, Duration::from_secs(20)).await;
    assert_eq!(updates[0].key, key);
    node.stop().await
}
