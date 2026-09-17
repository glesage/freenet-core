//! Contract operations through the in-process client, against a local node.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use freenet_mobile::{FreenetNode, MobileError};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_then_get_round_trips_state() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), Some(reserve_port()))).await?;
    let (wasm, params) = test_contract();
    let state = empty_todo_list();

    let key = node.put(wasm, params, state.clone(), false).await?;
    assert!(!key.is_empty(), "put must return the instance id");

    let got = node.get(key.clone(), false).await?;
    assert_eq!(got.key, key);
    assert_eq!(got.state, state);
    node.stop().await
}

/// A well-formed but unknown local GET is reported as a generic request error.
/// Keep this narrow: accepting `NotFound` too would hide a regression in the
/// local request loop's error mapping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_of_unknown_contract_is_a_client_error() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), Some(reserve_port()))).await?;
    let err = node
        .get("6Sf2buCM1LzU5EhscNvjeNqPYbQbtvKSkzC6EFUy8Jjh".into(), false)
        .await
        .expect_err("unknown contract must not resolve");
    assert!(matches!(err, MobileError::Request(_)), "{err}");
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_key_is_rejected_before_hitting_the_node() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = FreenetNode::new_plain(local_profile(root.path(), None))?;
    let err = node
        .get("not a key".into(), false)
        .await
        .expect_err("garbage key must fail");
    assert!(matches!(err, MobileError::Request(_)), "{err}");
    Ok(())
}

/// `do_update` (client.rs) falls back to a GET when the contract key is not
/// in its `known` cache, because UPDATE needs the full key (code hash
/// included). Every other test in this file publishes and updates within the
/// same client session, so that cache always hits and the fallback never
/// runs. A restart gives a fresh client with an empty cache — the realistic
/// way this path gets exercised, since an app relaunch is exactly that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_delta_learns_the_key_after_a_restart() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), Some(reserve_port()))).await?;
    let (wasm, params) = test_contract();
    let key = node.put(wasm, params, empty_todo_list(), false).await?;
    node.stop().await?;

    // Restart: a new client actor, its `known` map starts empty.
    node.start().await?;
    node.update_delta(key.clone(), add_task_delta(1, "first"))
        .await?;

    let got = node.get(key.clone(), false).await?;
    let text = String::from_utf8_lossy(&got.state);
    assert!(
        text.contains("first"),
        "update after restart must have learned the key and applied: {text}"
    );
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_receives_update_notifications() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), Some(reserve_port()))).await?;
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
    let node = start_node(local_profile(root.path(), Some(reserve_port()))).await?;
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

/// Concurrent public calls are serialized by `FreenetNode::with_client`'s
/// lifecycle mutex before reaching the actor queue. This checks that callers
/// still receive the response for their own key; it does not exercise matching
/// two simultaneously pending actor requests or late-response recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_are_routed_to_their_callers() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = Arc::new(start_node(local_profile(root.path(), Some(reserve_port()))).await?);

    // Same code, different params: distinct contract keys (the test contract
    // ignores params in validate_state, so both PUTs succeed identically).
    let (wasm, _) = test_contract();
    let state_a = empty_todo_list();
    let state_b = empty_todo_list();
    let key_a = node
        .put(wasm.clone(), vec![], state_a.clone(), false)
        .await?;
    let key_b = node.put(wasm, vec![1], state_b.clone(), false).await?;
    assert_ne!(
        key_a, key_b,
        "different params must produce different contract keys"
    );

    let (na, ka) = (node.clone(), key_a.clone());
    let get_a = tokio::spawn(async move { na.get(ka, false).await });
    let (nb, kb) = (node.clone(), key_b.clone());
    let get_b = tokio::spawn(async move { nb.get(kb, false).await });
    let nc = node.clone();
    let peers = tokio::spawn(async move { nc.connected_peers().await });

    let (ra, rb, rc) = tokio::join!(get_a, get_b, peers);
    let ra = ra.expect("task a join")?;
    let rb = rb.expect("task b join")?;
    assert_eq!(
        ra.key, key_a,
        "task a must get contract a's response, not b's"
    );
    assert_eq!(ra.state, state_a);
    assert_eq!(
        rb.key, key_b,
        "task b must get contract b's response, not a's"
    );
    assert_eq!(rb.state, state_b);
    assert!(
        rc.expect("task c join").is_err(),
        "local mode rejects peer queries regardless of ordering"
    );

    node.stop().await
}
