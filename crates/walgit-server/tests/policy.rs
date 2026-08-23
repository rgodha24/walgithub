//! Per-repo push policy: HTTP get/put/delete and receive-pack enforcement.
mod harness;

type TestResult = anyhow::Result<()>;
use anyhow::Context;
use harness::{Server, TestRepo, git_in};
use std::process::Command;

const PROTECT_MAIN: &str = r#"{
  "version": 1,
  "rules": [
    {
      "name": "lock-main",
      "match": { "refs": ["refs/heads/main"] },
      "effect": {
        "protect": { "restricts": ["delete", "force-push"] }
      }
    }
  ]
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_http_roundtrip() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let client = reqwest::Client::new();
    let url = format!("{}/t/r/policy", server.base_url);

    let empty = client.get(&url).send().await?;
    assert_eq!(empty.status(), 200);
    let body: serde_json::Value = empty.json().await?;
    assert_eq!(body["rules"].as_array().unwrap().len(), 0);

    let put = client
        .put(&url)
        .header("content-type", "application/json")
        .body(PROTECT_MAIN)
        .send()
        .await?;
    assert_eq!(
        put.status(),
        204,
        "{}",
        put.text().await.unwrap_or_default()
    );

    let got = client.get(&url).send().await?;
    let body: serde_json::Value = got.json().await?;
    assert_eq!(body["rules"][0]["name"], "lock-main");
    assert_eq!(body["rules"][0]["match"]["refs"][0], "refs/heads/main");

    let del = client.delete(&url).send().await?;
    assert_eq!(del.status(), 204);
    let empty = client.get(&url).send().await?;
    let body: serde_json::Value = empty.json().await?;
    assert_eq!(body["rules"].as_array().unwrap().len(), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_missing_repo_is_404() -> TestResult {
    let server = Server::start().await?;
    let status = reqwest::Client::new()
        .get(format!("{}/no/such/policy", server.base_url))
        .send()
        .await?
        .status();
    assert_eq!(status, 404);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_main_rejects_force_and_delete() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;
    let put = reqwest::Client::new()
        .put(format!("{}/t/r/policy", server.base_url))
        .header("content-type", "application/json")
        .body(PROTECT_MAIN)
        .send()
        .await?;
    assert_eq!(put.status(), 204);

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    // Unrelated history + --force: policy, not CAS, must reject.
    git_in(&src, &["checkout", "--orphan", "other"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "other"])?;
    let force = Command::new("git")
        .current_dir(&*src)
        .args(["push", "--force", "origin", "other:main"])
        .output()?;
    let stderr = String::from_utf8_lossy(&force.stderr);
    assert!(
        !force.status.success(),
        "force-push of protected main succeeded: {stderr}"
    );
    assert!(
        stderr.contains("lock-main") || stderr.contains("rejected by rule"),
        "stderr should name the rule: {stderr}"
    );

    let del = Command::new("git")
        .current_dir(&*src)
        .args(["push", "origin", ":refs/heads/main"])
        .output()?;
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(!del.status.success(), "delete of protected main succeeded");
    assert!(
        stderr.contains("lock-main") || stderr.contains("rejected by rule"),
        "stderr should name the rule: {stderr}"
    );

    // Unprotected branch may be force-pushed (orphan onto a new name).
    git_in(&src, &["checkout", "--orphan", "topic"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "topic"])?;
    git_in(&src, &["push", "origin", "topic"])?;
    git_in(&src, &["checkout", "--orphan", "topic2"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "topic-other"])?;
    git_in(&src, &["push", "--force", "origin", "topic2:topic"])
        .context("force-push of unprotected topic")?;

    // After clearing policy, force-push of main is allowed.
    let cleared = reqwest::Client::new()
        .delete(format!("{}/t/r/policy", server.base_url))
        .send()
        .await?;
    assert_eq!(cleared.status(), 204);
    git_in(&src, &["push", "--force", "origin", "other:main"])?;
    Ok(())
}
