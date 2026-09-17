//! Events (docs/EVENTS.md): the bridge publishes exactly what the WAL
//! committed, from a durable cursor; the GCS-notification wake-up; the sweep;
//! a sink failure keeps the cursor.
mod harness;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

type TestResult = anyhow::Result<()>;
use harness::{Server, TestRepo, git_in};
use std::time::Duration;

type Captured = std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

/// The webhook sink's target: records every event it receives (the bus as
/// the test sees it).
async fn webhook() -> (String, Captured) {
    let captured: Captured = Default::default();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post({
            let captured = captured.clone();
            move |axum::Json(batch): axum::Json<Vec<serde_json::Value>>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().extend(batch);
                    axum::http::StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}/events"), captured)
}

fn bridge_cfg(url: &str, sweep: Duration) -> impl FnOnce(&mut walgit_config::Config) + '_ {
    move |c| {
        c.events.webhook_url = Some(url.to_string());
        c.events.sweep_interval = sweep;
    }
}

async fn cursor_seq(server: &Server, owner: &str, name: &str) -> Option<u64> {
    use walgit_store::ObjectStoreExt;
    let id = walgit_git::RepoId::new(owner, name).unwrap();
    let h = server.state.registry.open(&id).await.unwrap();
    let (_, bytes) = h.store().get_bytes("events/cursor.json").await.unwrap()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["published_seq"].as_u64()
}

async fn wait_for(captured: &Captured, n: usize) -> Vec<serde_json::Value> {
    let t0 = std::time::Instant::now();
    loop {
        let got = captured.lock().unwrap().clone();
        if got.len() >= n {
            return got;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "timed out waiting for {n} events; got {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn gcs_notification(object: &str, event_type: &str) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "attributes": { "objectId": object, "eventType": event_type,
                            "bucketId": "walgit-store", "objectGeneration": "7" },
            "data": "", "messageId": "1", "publishTime": "2026-08-20T00:00:00Z"
        },
        "subscription": "projects/p/subscriptions/walgit-store-changes"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_publishes_from_cursor_exactly_once() -> TestResult {
    let (url, captured) = webhook().await;
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::ZERO)).await?;
    let bridge = server.state.bridge.clone().expect("bridge enabled");
    server.put_repo("t", "r").await?;
    let id = walgit_git::RepoId::new("t", "r")?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "b"])?;
    git_in(&src, &["push"])?;
    assert!(
        captured.lock().unwrap().is_empty(),
        "nothing reaches the bus until the bridge runs"
    );

    // First catch-up: cold cursor → everything readable (seq 1..=2).
    let r = bridge.catch_up(&id).await?;
    assert_eq!((r.from_seq, r.head_seq, r.emitted), (0, 2, 2));
    let got = wait_for(&captured, 2).await;
    assert_eq!(got[0]["action"], "create");
    assert_eq!(
        got[0]["old"], ZERO_OID,
        "create carries the zero OID, never empty"
    );
    assert_eq!(got[0]["_walgit"]["seq"], "1");
    assert_eq!(got[1]["action"], "update");
    assert_eq!(got[1]["_walgit"]["seq"], "2");
    assert_eq!(got[1]["_walgit"]["entry_kind"], "push");
    assert_eq!(got[1]["repo"], "t/r");
    assert_eq!(got[1]["ref_name"], "refs/heads/main");
    assert_eq!(got[1]["pusher"], "anon");
    assert!(
        !got[1]["correlation_id"].as_str().unwrap().is_empty(),
        "the request id the middleware minted travels WAL meta → event"
    );
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(2));

    // Again: nothing new, nothing published, cursor untouched.
    let r = bridge.catch_up(&id).await?;
    assert_eq!((r.from_seq, r.head_seq, r.emitted), (2, 2, 0));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(captured.lock().unwrap().len(), 2);

    // The GCS notification of the manifest CAS is the wake-up.
    git_in(&src, &["push", "origin", ":refs/heads/main"])?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/_events/notify", server.base_url))
        .json(&gcs_notification(
            "repos/t/r/manifest.pb",
            "OBJECT_FINALIZE",
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let report: serde_json::Value = resp.json().await?;
    assert_eq!(report[0]["emitted"], 1);
    let got = wait_for(&captured, 3).await;
    assert_eq!(got[2]["action"], "delete");
    assert_eq!(
        got[2]["new"], ZERO_OID,
        "delete carries the zero OID, never empty"
    );
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(3));

    // An S3-shaped notification (MinIO/rustfs/Ceph emit the same) and a plain `{"repo": …}` wake
    // the same catch-up; with nothing new they are acked with an empty report list.
    for body in [
        serde_json::json!({"Records": [{"eventName": "ObjectCreated:Put", "s3": {"object": {"key": "repos/t/r/manifest.pb"}}}]}),
        serde_json::json!({"repo": "t/r"}),
        serde_json::json!({"key": "repos/t/r/manifest.pb"}),
    ] {
        let resp = client
            .post(format!("{}/_events/notify", server.base_url))
            .json(&body)
            .send()
            .await?;
        assert_eq!(resp.status(), 200, "{body}");
        let report: serde_json::Value = resp.json().await?;
        assert_eq!(report[0]["emitted"], 0, "{body}: {report}");
    }

    // Other objects and other event types are acked and ignored.
    for (obj, ty) in [
        ("repos/t/r/wal/abc.pack", "OBJECT_FINALIZE"),
        ("repos/t/r/manifest.pb", "OBJECT_DELETE"),
        ("repos/t/r/events/cursor.json", "OBJECT_FINALIZE"),
    ] {
        let resp = client
            .post(format!("{}/_events/notify", server.base_url))
            .json(&gcs_notification(obj, ty))
            .send()
            .await?;
        assert_eq!(resp.status(), 200, "{obj} {ty}");
    }
    // A late notification for a repo deleted since: 200, nothing to do
    // (a 503 would have Pub/Sub retry it for days).
    let del = client
        .delete(format!("{}/t/r", server.base_url))
        .send()
        .await?;
    assert_eq!(del.status(), 204);
    let resp = client
        .post(format!("{}/_events/notify", server.base_url))
        .json(&gcs_notification(
            "repos/t/r/manifest.pb",
            "OBJECT_FINALIZE",
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(captured.lock().unwrap().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_sweep_timer_publishes_without_notifications() -> TestResult {
    let (url, captured) = webhook().await;
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::from_millis(200))).await?;
    server.put_repo("t", "r").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;
    let got = wait_for(&captured, 1).await;
    assert_eq!(got[0]["action"], "create");
    assert_eq!(got[0]["_walgit"]["seq"], "1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_sink_failure_keeps_the_cursor() -> TestResult {
    // Nothing listens here: every delivery fails.
    let server =
        Server::start_with_tweak(bridge_cfg("http://127.0.0.1:1/events", Duration::ZERO)).await?;
    let bridge = server.state.bridge.clone().expect("bridge enabled");
    server.put_repo("t", "r").await?;
    let id = walgit_git::RepoId::new("t", "r")?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;

    let err = bridge.catch_up(&id).await.expect_err("sink down");
    assert!(err.to_string().contains("webhook sink"), "{err:#}");
    assert_eq!(
        cursor_seq(&server, "t", "r").await,
        Some(0),
        "initial boundary is durable but must not advance"
    );

    let resp = reqwest::Client::new()
        .post(format!("{}/_events/notify", server.base_url))
        .json(&gcs_notification(
            "repos/t/r/manifest.pb",
            "OBJECT_FINALIZE",
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 503, "non-2xx so Pub/Sub redelivers");
    Ok(())
}

/// A webhook is still in flight while two checkpoints remove its next batch
/// from the manifest. Kill the bridge before ACK, then recover from the bucket
/// on a fresh instance: both the in-flight event and the unseen ones must arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_replays_checkpointed_events_after_interrupted_first_delivery() -> TestResult {
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let captured: Captured = Default::default();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post({
            let entered = entered.clone();
            let permits = permits.clone();
            let captured = captured.clone();
            move |axum::Json(batch): axum::Json<Vec<serde_json::Value>>| {
                let (entered, permits, captured) =
                    (entered.clone(), permits.clone(), captured.clone());
                async move {
                    captured.lock().unwrap().extend(batch);
                    entered.notify_one();
                    permits.acquire().await.unwrap().forget();
                    axum::http::StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/events", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::ZERO)).await?;
    server.put_repo("t", "r").await?;
    let id: walgit_git::RepoId = "t/r".parse()?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;
    let bridge = server.state.bridge.clone().unwrap();
    let task = tokio::spawn({
        let id = id.clone();
        async move { bridge.catch_up(&id).await }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified()).await?;
    let handle = server.state.registry.open(&id).await?;
    for message in ["second", "third"] {
        git_in(&src, &["commit", "--allow-empty", "-m", message])?;
        git_in(&src, &["push"])?;
        handle.write_checkpoint().await?;
    }
    assert_eq!(handle.manifest().min_seq, 4);
    assert!(handle.manifest().log_segments.is_empty());
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(0));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let store = server.store.clone();
    drop(server);
    // Ignore the unacknowledged first attempt; the retried batch must contain
    // all three committed ref updates, in order, from a cold cache.
    captured.lock().unwrap().clear();
    permits.add_permits(10);
    let restarted =
        Server::start_with_store_and_tweak(store, bridge_cfg(&url, Duration::ZERO)).await?;
    let report = restarted
        .state
        .bridge
        .as_ref()
        .unwrap()
        .catch_up(&id)
        .await?;
    assert_eq!(
        (report.from_seq, report.head_seq, report.emitted),
        (0, 3, 3)
    );
    let got = captured.lock().unwrap().clone();
    assert_eq!(
        got.iter()
            .map(|ev| ev["_walgit"]["seq"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["1", "2", "3"]
    );
    assert_eq!(cursor_seq(&restarted, "t", "r").await, Some(3));
    assert_eq!(
        restarted
            .state
            .bridge
            .as_ref()
            .unwrap()
            .catch_up(&id)
            .await?
            .emitted,
        0
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_missing_history_does_not_advance_cursor() -> TestResult {
    use walgit_store::ObjectStore;
    let (url, captured) = webhook().await;
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::ZERO)).await?;
    server.put_repo("t", "r").await?;
    let id = "t/r".parse()?;
    let bridge = server.state.bridge.as_ref().unwrap();
    bridge.catch_up(&id).await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(&src, &["push", &server.repo_url("t", "r"), "main"])?;
    let handle = server.state.registry.open(&id).await?;
    let checkpoint = handle.write_checkpoint().await?;
    handle.store().delete(&checkpoint.key, None).await?;
    assert!(bridge.catch_up(&id).await.is_err());
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(0));
    assert!(captured.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_first_pass_after_checkpoint_replays_retained_history() -> TestResult {
    let (url, captured) = webhook().await;
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::ZERO)).await?;
    server.put_repo("t", "r").await?;
    let id = "t/r".parse()?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(&src, &["push", &server.repo_url("t", "r"), "main"])?;
    server
        .state
        .registry
        .open(&id)
        .await?
        .write_checkpoint()
        .await?;
    assert_eq!(cursor_seq(&server, "t", "r").await, None);
    let report = server.state.bridge.as_ref().unwrap().catch_up(&id).await?;
    assert_eq!((report.from_seq, report.emitted), (0, 1));
    assert_eq!(captured.lock().unwrap()[0]["_walgit"]["seq"], "1");
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(1));
    Ok(())
}
