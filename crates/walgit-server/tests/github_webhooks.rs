//! Outbound GitHub webhooks (`docs/GITHUB.md` §Webhooks): a `git push` to the
//! facade produces a signed `push` delivery out of the WAL within a second,
//! branch create/delete produce `create`/`delete`, and the PR handlers produce
//! `pull_request`.

mod harness;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness::{Server, git_in};
use serde_json::Value;

type TestResult = anyhow::Result<()>;

const SECRET: &str = "a-shared-secret";
const ZERO: &str = "0000000000000000000000000000000000000000";
const INSTALLATION: u64 = 55_555_555;

#[derive(Clone, Debug)]
struct Delivery {
    event: String,
    signature: String,
    delivery: String,
    content_type: String,
    body: Vec<u8>,
    payload: Value,
}

type Captured = Arc<Mutex<Vec<Delivery>>>;

/// `sha256=<hex>` the way `@octokit/webhooks-methods` computes it.
fn sign(secret: &[u8], body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret).expect("hmac key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The consumer: reads the raw body (there is no JSON body parser in front of
/// octokit's middleware either) and records what arrived.
async fn receiver() -> (String, Captured) {
    let captured: Captured = Captured::default();
    let app = axum::Router::new().route(
        "/github-enterprise/acme",
        axum::routing::post({
            let captured = captured.clone();
            move |headers: axum::http::HeaderMap, body: bytes::Bytes| {
                let captured = captured.clone();
                async move {
                    let header = |n: &str| {
                        headers
                            .get(n)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string()
                    };
                    captured.lock().expect("lock").push(Delivery {
                        event: header("x-github-event"),
                        signature: header("x-hub-signature-256"),
                        delivery: header("x-github-delivery"),
                        content_type: header("content-type"),
                        body: body.to_vec(),
                        payload: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    });
                    axum::http::StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}/github-enterprise/acme"), captured)
}

async fn server(url: &str) -> anyhow::Result<Server> {
    let url = url.to_string();
    Server::start_with_tweak(move |cfg| {
        cfg.github.enabled = true;
        cfg.github.webhook_url = Some(url);
        cfg.github.webhook_secret = Some(SECRET.into());
        cfg.github.installation_id = INSTALLATION;
        cfg.github.webhook_poll_interval = Duration::from_millis(200);
        cfg.server.auto_create_on_push = true;
    })
    .await
}

/// The first delivery matching `event` + `pred`, or a panic with everything
/// that did arrive. The Mintlify suite waits 10 s; 5 s here is the budget.
async fn wait_for(
    captured: &Captured,
    event: &str,
    pred: impl Fn(&Value) -> bool,
) -> (Delivery, Duration) {
    let t0 = Instant::now();
    loop {
        {
            let seen = captured.lock().expect("lock");
            if let Some(d) = seen.iter().find(|d| d.event == event && pred(&d.payload)) {
                return (d.clone(), t0.elapsed());
            }
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "no {event} delivery in 5s; got {:?}",
                seen.iter().map(|d| d.event.clone()).collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn commit_in(dir: &std::path::Path, path: &str, body: &str, message: &str) -> anyhow::Result<()> {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(full, body)?;
    git_in(dir, &["add", "."])?;
    git_in(dir, &["commit", "-q", "-m", message])?;
    Ok(())
}

fn fixture(s: &Server) -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "ada@acme.com"])?;
    git_in(&dir, &["config", "user.name", "Ada Lovelace"])?;
    git_in(
        &dir,
        &["remote", "add", "origin", &s.repo_url("acme", "docs")],
    )?;
    Ok((tmp, dir))
}

fn api(s: &Server, path: &str) -> String {
    format!("{}/api/v3{path}", s.base_url)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_delivers_a_signed_push_event_from_the_wal() -> TestResult {
    let (url, captured) = receiver().await;
    let s = server(&url).await?;
    let (_tmp, dir) = fixture(&s)?;

    // --- create the branch ----------------------------------------------------
    commit_in(
        &dir,
        "docs/quickstart.mdx",
        "hello\n",
        "docs: add quickstart",
    )?;
    let first = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git_in(&dir, &["push", "-q", "origin", "main"])?;

    let (d, latency) = wait_for(&captured, "push", |p| p["after"] == first).await;
    println!("push delivered in {latency:?}");
    assert!(
        latency < Duration::from_secs(2),
        "push took {latency:?}, the editor suite waits 10s"
    );
    assert_eq!(d.content_type, "application/json");
    assert_eq!(
        d.signature,
        sign(SECRET.as_bytes(), &d.body),
        "signature must verify over the raw body"
    );
    assert_eq!(uuid::Uuid::parse_str(&d.delivery)?.get_version_num(), 4);

    let p = &d.payload;
    assert_eq!(p["ref"], "refs/heads/main");
    assert_eq!(p["before"], ZERO);
    assert_eq!(p["after"], first);
    assert_eq!(p["created"], true);
    assert_eq!(p["deleted"], false);
    assert_eq!(p["forced"], false);
    assert_eq!(p["size"], 1);
    assert_eq!(p["installation"]["id"], INSTALLATION);
    assert_eq!(p["repository"]["name"], "docs");
    assert_eq!(p["repository"]["full_name"], "acme/docs");
    assert_eq!(p["repository"]["owner"]["login"], "acme");
    assert_eq!(p["repository"]["default_branch"], "main");
    assert_eq!(p["sender"]["login"], "mintlify-dev");
    assert_eq!(p["sender"]["type"], "User");
    // `pusher` is the WAL's principal (an unauthenticated dev push is `anon`);
    // `sender` is the facade's one user.
    let pusher = p["pusher"]["name"].as_str().unwrap_or_default();
    assert!(!pusher.is_empty(), "pusher.name empty: {p}");
    assert_eq!(p["pusher"]["email"], format!("{pusher}@walgit.localhost"));
    let head = &p["head_commit"];
    assert_eq!(head["id"], first);
    assert_eq!(head["message"], "docs: add quickstart");
    assert_eq!(head["added"], serde_json::json!(["docs/quickstart.mdx"]));
    assert_eq!(head["modified"], serde_json::json!([]));
    assert_eq!(head["removed"], serde_json::json!([]));
    assert_eq!(head["distinct"], true);
    // The commit's own identity (`.cargo/config.toml` pins it for tests);
    // `username` is the facade's one user, as GitHub reports it.
    assert!(!head["author"]["email"].as_str().unwrap_or("").is_empty());
    assert_eq!(head["author"]["name"], head["committer"]["name"]);
    assert_eq!(head["author"]["username"], "mintlify-dev");
    assert_eq!(head["committer"]["username"], "mintlify-dev");
    assert!(head["timestamp"].as_str().unwrap_or("").contains('T'));
    assert!(!head["tree_id"].as_str().unwrap_or("").is_empty());
    assert_eq!(p["commits"].as_array().map(Vec::len), Some(1));

    // A branch create is also a `create` event.
    let (c, _) = wait_for(&captured, "create", |p| p["ref"] == "main").await;
    assert_eq!(c.payload["ref_type"], "branch");
    assert_eq!(c.payload["master_branch"], "main");
    assert_eq!(c.payload["pusher_type"], "user");
    assert_eq!(c.payload["installation"]["id"], INSTALLATION);

    // --- a second commit: `before` is the first tip ---------------------------
    commit_in(
        &dir,
        "docs/quickstart.mdx",
        "hello again\n",
        "docs: edit quickstart",
    )?;
    std::fs::remove_file(dir.join("docs/quickstart.mdx")).ok();
    std::fs::write(dir.join("docs/second.mdx"), "two\n")?;
    git_in(&dir, &["add", "-A"])?;
    git_in(&dir, &["commit", "-q", "-m", "docs: rename to second"])?;
    let second = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git_in(&dir, &["push", "-q", "origin", "main"])?;

    let (d, _) = wait_for(&captured, "push", |p| p["after"] == second).await;
    let p = &d.payload;
    assert_eq!(p["before"], first);
    assert_eq!(p["created"], false);
    assert_eq!(p["forced"], false);
    assert_eq!(p["size"], 2);
    assert_eq!(p["commits"].as_array().map(Vec::len), Some(2));
    assert_eq!(p["head_commit"]["id"], second);
    assert_eq!(
        p["head_commit"]["added"],
        serde_json::json!(["docs/second.mdx"])
    );
    assert_eq!(
        p["head_commit"]["removed"],
        serde_json::json!(["docs/quickstart.mdx"])
    );

    // --- delete a branch ------------------------------------------------------
    git_in(&dir, &["push", "-q", "origin", "main:refs/heads/scratch"])?;
    wait_for(&captured, "create", |p| p["ref"] == "scratch").await;
    git_in(&dir, &["push", "-q", "origin", ":refs/heads/scratch"])?;
    let (d, _) = wait_for(&captured, "delete", |p| p["ref"] == "scratch").await;
    assert_eq!(d.payload["ref_type"], "branch");
    assert_eq!(d.payload["repository"]["full_name"], "acme/docs");
    let (d, _) = wait_for(&captured, "push", |p| {
        p["ref"] == "refs/heads/scratch" && p["deleted"] == true
    })
    .await;
    assert_eq!(d.payload["after"], ZERO);
    assert!(d.payload["head_commit"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_request_open_synchronize_and_merge_deliver() -> TestResult {
    let (url, captured) = receiver().await;
    let s = server(&url).await?;
    let (_tmp, dir) = fixture(&s)?;
    let client = reqwest::Client::new();

    commit_in(&dir, "docs.json", "{}\n", "initial commit")?;
    git_in(&dir, &["push", "-q", "origin", "main"])?;
    git_in(&dir, &["checkout", "-q", "-b", "editor/quickstart"])?;
    commit_in(
        &dir,
        "docs/quickstart.mdx",
        "hello\n",
        "docs: add quickstart",
    )?;
    git_in(&dir, &["push", "-q", "origin", "editor/quickstart"])?;
    wait_for(&captured, "push", |p| {
        p["ref"] == "refs/heads/editor/quickstart"
    })
    .await;

    // --- opened ---------------------------------------------------------------
    let resp = client
        .post(api(&s, "/repos/acme/docs/pulls"))
        .header("Authorization", "Bearer anything")
        .json(&serde_json::json!({
            "title": "Docs: add quickstart",
            "head": "editor/quickstart",
            "base": "main",
        }))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "create PR: {}", resp.status());
    let pr: Value = resp.json().await?;
    let number = pr["number"].as_u64().unwrap_or_default();
    anyhow::ensure!(number > 0, "no PR number: {pr}");

    let (d, _) = wait_for(&captured, "pull_request", |p| p["action"] == "opened").await;
    assert_eq!(d.payload["number"], number);
    assert_eq!(
        d.payload["pull_request"]["head"]["ref"],
        "editor/quickstart"
    );
    assert_eq!(d.payload["pull_request"]["base"]["ref"], "main");
    assert_eq!(d.payload["pull_request"]["state"], "open");
    assert_eq!(d.payload["installation"]["id"], INSTALLATION);
    assert_eq!(d.payload["repository"]["full_name"], "acme/docs");
    assert_eq!(d.payload["sender"]["login"], "mintlify-dev");

    // --- synchronize: a push to the open PR's head branch ---------------------
    commit_in(&dir, "docs/quickstart.mdx", "hello two\n", "docs: revise")?;
    let head2 = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git_in(&dir, &["push", "-q", "origin", "editor/quickstart"])?;
    let (d, _) = wait_for(&captured, "pull_request", |p| p["action"] == "synchronize").await;
    assert_eq!(d.payload["number"], number);
    assert_eq!(d.payload["after"], head2);
    assert_eq!(d.payload["pull_request"]["head"]["sha"], head2);

    // --- closed, merged -------------------------------------------------------
    let resp = client
        .put(api(&s, &format!("/repos/acme/docs/pulls/{number}/merge")))
        .header("Authorization", "Bearer anything")
        .json(&serde_json::json!({ "merge_method": "merge" }))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "merge: {}", resp.status());
    let merged: Value = resp.json().await?;
    let merge_sha = merged["sha"].as_str().unwrap_or_default().to_string();

    let (d, _) = wait_for(&captured, "pull_request", |p| p["action"] == "closed").await;
    assert_eq!(d.payload["pull_request"]["merged"], true);
    assert_eq!(d.payload["pull_request"]["state"], "closed");
    assert_eq!(d.payload["pull_request"]["merge_commit_sha"], merge_sha);

    // The merge is a real publish, so the base branch gets a `push` too.
    let (d, latency) = wait_for(&captured, "push", |p| {
        p["ref"] == "refs/heads/main" && p["after"] == merge_sha
    })
    .await;
    println!("merge push delivered in {latency:?}");
    assert_eq!(d.payload["created"], false);
    assert!(d.payload["size"].as_u64().unwrap_or(0) >= 1);
    Ok(())
}
