//! The GitHub facade's PR flow (`docs/GITHUB.md` §prs): open/list/get/patch,
//! files with a rename, real merges (merge, squash, rebase) verified by a real
//! `git fetch`, conflicts and stale heads, comments and reviews,
//! `commits/{sha}/pulls`, `POST /merges`, template generation and the
//! accept-and-forget stubs.

mod harness;

use harness::{Server, git_in};
use reqwest::StatusCode;
use serde_json::{Value, json};

type TestResult = anyhow::Result<()>;

async fn server() -> anyhow::Result<Server> {
    Server::start_with_tweak(|cfg| {
        cfg.github.enabled = true;
        cfg.server.auto_create_on_push = true;
    })
    .await
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn get(s: &Server, path: &str) -> anyhow::Result<(StatusCode, Value)> {
    let resp = client()
        .get(format!("{}{path}", s.base_url))
        .header("Authorization", "Bearer anything-at-all")
        .send()
        .await?;
    read(resp).await
}

async fn send(
    s: &Server,
    method: reqwest::Method,
    path: &str,
    body: &Value,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = client()
        .request(method, format!("{}{path}", s.base_url))
        .header("Authorization", "Bearer anything-at-all")
        .json(body)
        .send()
        .await?;
    read(resp).await
}

async fn read(resp: reqwest::Response) -> anyhow::Result<(StatusCode, Value)> {
    let status = resp.status();
    let text = resp.text().await?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

async fn post(s: &Server, path: &str, body: &Value) -> anyhow::Result<(StatusCode, Value)> {
    send(s, reqwest::Method::POST, path, body).await
}

async fn put(s: &Server, path: &str, body: &Value) -> anyhow::Result<(StatusCode, Value)> {
    send(s, reqwest::Method::PUT, path, body).await
}

async fn ok(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (status, v) = get(s, path).await?;
    anyhow::ensure!(status.is_success(), "GET {path} -> {status}: {v}");
    Ok(v)
}

/// A repository with `main` and a `feature` branch that renames a file and
/// edits another, pushed over smart HTTP the way every repository arrives.
struct Fixture {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    main: String,
    feature: String,
}

fn fixture(s: &Server) -> anyhow::Result<Fixture> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "dev@walgit.test"])?;
    git_in(&dir, &["config", "user.name", "Dev"])?;
    std::fs::create_dir_all(dir.join("pages"))?;
    std::fs::write(dir.join("README.md"), "# docs\n")?;
    std::fs::write(dir.join("pages/index.mdx"), "one\ntwo\nthree\n")?;
    std::fs::write(dir.join("pages/old.mdx"), "keep me around\nsecond line\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "initial commit"])?;
    let main = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();

    git_in(&dir, &["checkout", "-q", "-b", "feature"])?;
    std::fs::write(dir.join("pages/index.mdx"), "one\ntwo edited\nthree\n")?;
    git_in(&dir, &["mv", "pages/old.mdx", "pages/new.mdx"])?;
    git_in(&dir, &["add", "-A"])?;
    git_in(&dir, &["commit", "-q", "-m", "edit and rename"])?;
    let feature = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();

    git_in(&dir, &["checkout", "-q", "main"])?;
    git_in(&dir, &["remote", "add", "origin", &s.repo_url("acme", "docs")])?;
    git_in(&dir, &["push", "-q", "origin", "main", "feature"])?;
    Ok(Fixture {
        _tmp: tmp,
        dir,
        main,
        feature,
    })
}

/// Push one more branch off `main` from the fixture's working copy.
fn branch_with(f: &Fixture, s: &Server, name: &str, path: &str, body: &str) -> anyhow::Result<()> {
    git_in(&f.dir, &["checkout", "-q", "-B", name, "main"])?;
    std::fs::write(f.dir.join(path), body)?;
    git_in(&f.dir, &["add", "-A"])?;
    git_in(&f.dir, &["commit", "-q", "-m", &format!("work on {name}")])?;
    git_in(&f.dir, &["push", "-q", "origin", name])?;
    git_in(&f.dir, &["checkout", "-q", "main"])?;
    let _ = s;
    Ok(())
}

/// The sha a real `git fetch` sees for a branch — proof the write reached the
/// bucket rather than a process-local cache.
fn fetched(s: &Server, branch: &str) -> anyhow::Result<(String, String)> {
    let dst = tempfile::tempdir()?;
    harness::git(&["init", "-q", "-b", "main"], dst.path())?;
    harness::git(
        &["remote", "add", "origin", &s.repo_url("acme", "docs")],
        dst.path(),
    )?;
    harness::git(&["fetch", "-q", "origin", branch], dst.path())?;
    let sha = git_in(dst.path(), &["rev-parse", "FETCH_HEAD"])?
        .trim()
        .to_string();
    let parents = git_in(dst.path(), &["rev-list", "--parents", "-n", "1", "FETCH_HEAD"])?
        .trim()
        .to_string();
    Ok((sha, parents))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opens_lists_and_reads_a_pull_request() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let (status, pr) = post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "Edit the quickstart", "head": "acme:feature", "base": "main", "body": "why"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{pr}");
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["state"], "open");
    assert_eq!(pr["head"]["ref"], "feature");
    assert_eq!(pr["head"]["sha"], f.feature);
    assert_eq!(pr["base"]["ref"], "main");
    assert!(pr["base"]["repo"]["id"].is_number());
    assert!(pr["head"]["repo"]["id"].is_number());
    assert_eq!(pr["user"]["login"], "mintlify-dev");
    assert!(pr["html_url"].as_str().is_some_and(|u| u.contains("/pull/1")));
    let decoded = {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(pr["node_id"].as_str().unwrap_or_default())?;
        String::from_utf8(raw)?
    };
    assert_eq!(decoded, "PR_acme/docs#1");

    // A second PR for the same head/base pair is GitHub's 422, and the message
    // the client string-matches is inside errors[].
    let (status, dup) = post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "again", "head": "feature", "base": "main"}),
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(dup["message"], "Validation Failed");
    assert!(
        dup["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("A pull request already exists")),
        "{dup}"
    );

    // No commits between: main into main.
    let (status, empty) = post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "nothing", "head": "main", "base": "main"}),
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        empty["errors"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("No commits between ")),
        "{empty}"
    );

    // The full read carries the diff numbers and mergeability.
    let one = ok(&s, "/api/v3/repos/acme/docs/pulls/1").await?;
    assert_eq!(one["commits"], 1);
    assert_eq!(one["changed_files"], 2);
    assert_eq!(one["mergeable"], true);
    assert_eq!(one["mergeable_state"], "clean");
    assert_eq!(one["merged"], false);
    assert!(one["merged_at"].is_null());
    assert!(one["merge_commit_sha"].is_null());

    // Listing filters on head, and `state=all` is honoured.
    let listed = ok(&s, "/api/v3/repos/acme/docs/pulls?state=open&head=acme:feature").await?;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    assert_eq!(listed[0]["number"], 1);
    let none = ok(&s, "/api/v3/repos/acme/docs/pulls?head=acme:nope").await?;
    assert_eq!(none.as_array().map(Vec::len), Some(0));

    // PATCH: title and body, then close and reopen.
    let (status, _) = send(
        &s,
        reqwest::Method::PATCH,
        "/api/v3/repos/acme/docs/pulls/1",
        &json!({"title": "Renamed", "body": "new body"}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let (_, closed) = send(
        &s,
        reqwest::Method::PATCH,
        "/api/v3/repos/acme/docs/pulls/1",
        &json!({"state": "closed"}),
    )
    .await?;
    assert_eq!(closed["state"], "closed");
    assert_eq!(closed["title"], "Renamed");
    assert!(!closed["closed_at"].is_null());
    let only_closed = ok(&s, "/api/v3/repos/acme/docs/pulls?state=closed").await?;
    assert_eq!(only_closed.as_array().map(Vec::len), Some(1));
    assert_eq!(
        ok(&s, "/api/v3/repos/acme/docs/pulls?state=open")
            .await?
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let (_, reopened) = send(
        &s,
        reqwest::Method::PATCH,
        "/api/v3/repos/acme/docs/pulls/1",
        &json!({"state": "open"}),
    )
    .await?;
    assert_eq!(reopened["state"], "open");

    let missing = get(&s, "/api/v3/repos/acme/docs/pulls/99").await?;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_and_commits_describe_the_diff() -> TestResult {
    let s = server().await?;
    let _f = fixture(&s)?;
    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "t", "head": "feature", "base": "main"}),
    )
    .await?;

    let files = ok(&s, "/api/v3/repos/acme/docs/pulls/1/files?per_page=100").await?;
    let files = files.as_array().cloned().unwrap_or_default();
    assert_eq!(files.len(), 2, "{files:?}");
    let edited = files
        .iter()
        .find(|f| f["filename"] == "pages/index.mdx")
        .ok_or_else(|| anyhow::anyhow!("no edited file in {files:?}"))?;
    assert_eq!(edited["status"], "modified");
    assert_eq!(edited["additions"], 1);
    assert_eq!(edited["deletions"], 1);
    assert_eq!(edited["changes"], 2);
    assert!(
        edited["patch"]
            .as_str()
            .is_some_and(|p| p.starts_with("@@") && p.contains("+two edited")),
        "{edited}"
    );
    assert!(edited["blob_url"].is_string());
    assert!(edited["raw_url"].is_string());
    assert!(edited["contents_url"].is_string());
    assert!(edited["sha"].as_str().is_some_and(|s| s.len() == 40));

    let renamed = files
        .iter()
        .find(|f| f["filename"] == "pages/new.mdx")
        .ok_or_else(|| anyhow::anyhow!("no renamed file in {files:?}"))?;
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["previous_filename"], "pages/old.mdx");

    let commits = ok(&s, "/api/v3/repos/acme/docs/pulls/1/commits").await?;
    assert_eq!(commits.as_array().map(Vec::len), Some(1));
    assert_eq!(commits[0]["commit"]["message"], "edit and rename");
    assert!(commits[0]["sha"].as_str().is_some_and(|s| s.len() == 40));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_advances_the_base_with_two_parents() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;
    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "Edit", "head": "feature", "base": "main"}),
    )
    .await?;

    // A stale `sha` is GitHub's 409 and merges nothing.
    let (status, conflict) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/merge",
        &json!({"merge_method": "merge", "sha": "0".repeat(40)}),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        conflict["message"],
        "Head branch was modified. Review and try the merge again."
    );

    let (status, merged) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/merge",
        &json!({"merge_method": "merge", "sha": f.feature}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["merged"], true);
    assert_eq!(merged["message"], "Pull Request successfully merged");
    let sha = merged["sha"].as_str().unwrap_or_default().to_string();
    assert_eq!(sha.len(), 40);

    // Real git sees the merge commit with both parents.
    let (fetched_sha, parents) = fetched(&s, "main")?;
    assert_eq!(fetched_sha, sha);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "expected two parents: {parents}");
    assert!(parts.contains(&f.main.as_str()), "{parents}");
    assert!(parts.contains(&f.feature.as_str()), "{parents}");

    let pr = ok(&s, "/api/v3/repos/acme/docs/pulls/1").await?;
    assert_eq!(pr["merged"], true);
    assert_eq!(pr["state"], "closed");
    assert_eq!(pr["merge_commit_sha"], sha);
    assert!(!pr["merged_at"].is_null());
    assert_eq!(pr["head"]["sha"], f.feature, "a merged head is frozen");

    // Merging again is refused with the 405 the client branches on.
    let (status, again) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/merge",
        &json!({"merge_method": "merge"}),
    )
    .await?;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        again["message"]
            .as_str()
            .is_some_and(|m| m.contains("not mergeable") || m.contains("already merged")),
        "{again}"
    );

    // `commits/{sha}/pulls` finds it by merge commit, head sha and head ref.
    for probe in [sha.as_str(), f.feature.as_str(), "feature"] {
        let found = ok(&s, &format!("/api/v3/repos/acme/docs/commits/{probe}/pulls")).await?;
        assert_eq!(found.as_array().map(Vec::len), Some(1), "probe {probe}");
        assert_eq!(found[0]["number"], 1);
        assert_eq!(found[0]["base"]["ref"], "main");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn squash_and_rebase_land_a_single_parent_history() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;
    branch_with(&f, &s, "squashed", "pages/squash.mdx", "squash me\n")?;
    branch_with(&f, &s, "rebased", "pages/rebase.mdx", "rebase me\n")?;

    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "squash", "head": "squashed", "base": "main"}),
    )
    .await?;
    let (status, merged) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/merge",
        &json!({"merge_method": "squash", "commit_title": "Squashed (#1)"}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let (_, parents) = fetched(&s, "main")?;
    assert_eq!(
        parents.split_whitespace().count(),
        2,
        "a squash has one parent: {parents}"
    );
    let head = ok(&s, "/api/v3/repos/acme/docs/commits/main").await?;
    assert_eq!(head["commit"]["message"], "Squashed (#1)");

    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "rebase", "head": "rebased", "base": "main"}),
    )
    .await?;
    let (status, merged) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/2/merge",
        &json!({"merge_method": "rebase"}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{merged}");
    let (_, parents) = fetched(&s, "main")?;
    assert_eq!(
        parents.split_whitespace().count(),
        2,
        "a rebase is linear: {parents}"
    );
    let head = ok(&s, "/api/v3/repos/acme/docs/commits/main").await?;
    assert_eq!(head["commit"]["message"], "work on rebased");
    let listing = ok(&s, "/api/v3/repos/acme/docs/commits?sha=main&per_page=10").await?;
    let messages: Vec<&str> = listing
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["commit"]["message"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(messages.contains(&"Squashed (#1)"), "{messages:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflicting_merge_is_405_and_changes_nothing() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;
    // Two branches editing the same line of the same file in different ways.
    branch_with(&f, &s, "left", "pages/index.mdx", "one\nLEFT\nthree\n")?;
    branch_with(&f, &s, "right", "pages/index.mdx", "one\nRIGHT\nthree\n")?;

    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "left", "head": "left", "base": "main"}),
    )
    .await?;
    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "right", "head": "right", "base": "main"}),
    )
    .await?;

    let (status, _) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/merge",
        &json!({"merge_method": "merge"}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let after_first = ok(&s, "/api/v3/repos/acme/docs/git/ref/heads/main").await?;
    let main_now = after_first["object"]["sha"].as_str().unwrap_or_default().to_string();

    let (status, body) = put(
        &s,
        "/api/v3/repos/acme/docs/pulls/2/merge",
        &json!({"merge_method": "merge"}),
    )
    .await?;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    assert_eq!(body["message"], "Pull Request is not mergeable");

    let unchanged = ok(&s, "/api/v3/repos/acme/docs/git/ref/heads/main").await?;
    assert_eq!(unchanged["object"]["sha"], main_now);
    let pr = ok(&s, "/api/v3/repos/acme/docs/pulls/2").await?;
    assert_eq!(pr["merged"], false);
    assert_eq!(pr["mergeable"], false);
    assert_eq!(pr["mergeable_state"], "dirty");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merges_endpoint_merges_a_branch_directly() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let (status, commit) = post(
        &s,
        "/api/v3/repos/acme/docs/merges",
        &json!({"base": "main", "head": "feature", "commit_message": "Merge feature into main"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{commit}");
    assert!(commit["html_url"].is_string());
    assert_eq!(commit["commit"]["message"], "Merge feature into main");
    let (fetched_sha, parents) = fetched(&s, "main")?;
    assert_eq!(commit["sha"], fetched_sha);
    assert!(parents.contains(&f.feature), "{parents}");

    // Nothing left to merge is a 204 with no body.
    let resp = client()
        .post(format!("{}/api/v3/repos/acme/docs/merges", s.base_url))
        .json(&json!({"base": "main", "head": "feature"}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn comments_reviews_and_reactions_round_trip() -> TestResult {
    let s = server().await?;
    let _f = fixture(&s)?;
    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "t", "head": "feature", "base": "main"}),
    )
    .await?;

    let (status, comment) = post(
        &s,
        "/api/v3/repos/acme/docs/issues/1/comments",
        &json!({"body": "<!-- mintlify -->\npreview ready"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    let comment_id = comment["id"].as_u64().unwrap_or_default();
    assert!(comment_id > 0);

    let listed = ok(&s, "/api/v3/repos/acme/docs/issues/1/comments?per_page=100").await?;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    assert_eq!(listed[0]["id"], comment_id);
    assert_eq!(listed[0]["user"]["login"], "mintlify-dev");
    assert!(listed[0]["body"].as_str().is_some_and(|b| b.contains("mintlify")));

    let (status, _) = send(
        &s,
        reqwest::Method::PATCH,
        &format!("/api/v3/repos/acme/docs/issues/comments/{comment_id}"),
        &json!({"body": "edited"}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let after = ok(&s, "/api/v3/repos/acme/docs/issues/1/comments").await?;
    assert_eq!(after[0]["body"], "edited");

    // A reaction's id is what the comment bot later deletes by.
    let (status, reaction) = post(
        &s,
        &format!("/api/v3/repos/acme/docs/issues/comments/{comment_id}/reactions"),
        &json!({"content": "eyes"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED);
    let reaction_id = reaction["id"].as_u64().unwrap_or_default();
    assert!(reaction_id > 0);
    let resp = client()
        .delete(format!(
            "{}/api/v3/repos/acme/docs/issues/comments/{comment_id}/reactions/{reaction_id}",
            s.base_url
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // A pending review has a usable node id; submitting it flips the state.
    let (status, pending) = post(&s, "/api/v3/repos/acme/docs/pulls/1/reviews", &json!({})).await?;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["state"], "PENDING");
    assert!(pending["node_id"].as_str().is_some_and(|n| !n.is_empty()));
    let review_id = pending["id"].as_u64().unwrap_or_default();
    let (status, submitted) = post(
        &s,
        &format!("/api/v3/repos/acme/docs/pulls/1/reviews/{review_id}/events"),
        &json!({"event": "REQUEST_CHANGES", "body": "needs work"}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{submitted}");

    let (_, approved) = post(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/reviews",
        &json!({"event": "APPROVE", "body": "LGTM"}),
    )
    .await?;
    assert_eq!(approved["state"], "APPROVED");

    let reviews = ok(&s, "/api/v3/repos/acme/docs/pulls/1/reviews?per_page=100").await?;
    let reviews = reviews.as_array().cloned().unwrap_or_default();
    assert_eq!(reviews.len(), 2);
    let states: Vec<&str> = reviews.iter().filter_map(|r| r["state"].as_str()).collect();
    assert!(states.contains(&"CHANGES_REQUESTED"), "{states:?}");
    assert!(states.contains(&"APPROVED"), "{states:?}");
    assert!(reviews[1]["submitted_at"].is_string());
    assert_eq!(reviews[1]["user"]["login"], "mintlify-dev");

    // A review comment on a file.
    let (status, rc) = post(
        &s,
        "/api/v3/repos/acme/docs/pulls/1/comments",
        &json!({"body": "typo", "path": "pages/index.mdx", "line": 2}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{rc}");
    let review_comments = ok(&s, "/api/v3/repos/acme/docs/pulls/1/comments").await?;
    assert_eq!(review_comments.as_array().map(Vec::len), Some(1));
    assert_eq!(review_comments[0]["path"], "pages/index.mdx");
    assert_eq!(review_comments[0]["line"], 2);
    assert_eq!(review_comments[0]["original_line"], 2);

    // The PR as an issue, and search over it.
    let issue = ok(&s, "/api/v3/repos/acme/docs/issues/1").await?;
    assert_eq!(issue["number"], 1);
    assert!(issue["pull_request"]["merged_at"].is_null());
    let found = ok(
        &s,
        "/api/v3/search/issues?q=repo%3Aacme%2Fdocs+is%3Apr+head%3Afeature+state%3Aopen",
    )
    .await?;
    assert_eq!(found["total_count"], 1);
    assert_eq!(found["items"][0]["number"], 1);
    assert_eq!(found["items"][0]["state"], "open");
    assert!(found["items"][0]["html_url"].is_string());
    assert_eq!(found["incomplete_results"], false);

    let empty = ok(
        &s,
        "/api/v3/search/issues?q=repo%3Aacme%2Fdocs+is%3Apr+head%3Aabsent",
    )
    .await?;
    assert_eq!(empty["total_count"], 0);

    let (status, _) = send(
        &s,
        reqwest::Method::DELETE,
        &format!("/api/v3/repos/acme/docs/issues/comments/{comment_id}"),
        &json!({}),
    )
    .await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let gone = ok(&s, "/api/v3/repos/acme/docs/issues/1/comments").await?;
    assert_eq!(gone.as_array().map(Vec::len), Some(0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_template_generates_a_new_repository() -> TestResult {
    let s = server().await?;
    let _f = fixture(&s)?;

    let (status, repo) = post(
        &s,
        "/api/v3/repos/acme/docs/generate",
        &json!({"owner": "acme", "name": "copy", "private": true, "include_all_branches": false}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{repo}");
    assert_eq!(repo["name"], "copy");
    assert_eq!(repo["default_branch"], "main");
    assert_eq!(repo["full_name"], "acme/copy");

    // The seeded repository serves the template's default branch through the
    // ordinary read path and to a real `git fetch`.
    let head = ok(&s, "/api/v3/repos/acme/copy/commits/main").await?;
    assert_eq!(head["commit"]["message"], "initial commit");
    let dst = tempfile::tempdir()?;
    harness::git(&["init", "-q", "-b", "main"], dst.path())?;
    harness::git(
        &["remote", "add", "origin", &s.repo_url("acme", "copy")],
        dst.path(),
    )?;
    harness::git(&["fetch", "-q", "origin", "main"], dst.path())?;
    let listing = git_in(dst.path(), &["ls-tree", "-r", "--name-only", "FETCH_HEAD"])?;
    let mut names: Vec<&str> = listing.lines().collect();
    names.sort_unstable();
    assert_eq!(names, ["README.md", "pages/index.mdx", "pages/old.mdx"]);

    // The name is taken now.
    let (status, taken) = post(
        &s,
        "/api/v3/repos/acme/docs/generate",
        &json!({"owner": "acme", "name": "copy", "private": true}),
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(taken["message"], "Validation Failed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_runs_deployments_and_statuses_are_accepted() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let (status, run) = post(
        &s,
        "/api/v3/repos/acme/docs/check-runs",
        &json!({"name": "Mintlify Deployment", "head_sha": f.main, "status": "in_progress"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{run}");
    let id = run["id"].as_u64().unwrap_or_default();
    assert!(id > 0);
    assert_eq!(run["name"], "Mintlify Deployment");
    assert_eq!(run["head_sha"], f.main);
    assert_eq!(run["status"], "in_progress");

    // Concurrent PATCHes on one run must all succeed.
    let patches = (0..8).map(|i| {
        let s = &s;
        async move {
            client()
                .patch(format!("{}/api/v3/repos/acme/docs/check-runs/{id}", s.base_url))
                .json(&json!({
                    "status": "completed",
                    "conclusion": if i % 2 == 0 { "success" } else { "neutral" },
                    "output": {"summary": format!("pass {i}"), "text": Value::Null},
                }))
                .send()
                .await
                .map(|r| r.status())
        }
    });
    for result in futures::future::join_all(patches).await {
        assert_eq!(result?, StatusCode::OK);
    }

    let listed = ok(
        &s,
        &format!("/api/v3/repos/acme/docs/commits/{}/check-runs?filter=latest&per_page=100", f.main),
    )
    .await?;
    assert_eq!(listed["total_count"], 1);
    assert_eq!(listed["check_runs"][0]["id"], id);
    assert_eq!(listed["check_runs"][0]["status"], "completed");
    assert!(listed["check_runs"][0]["output"]["summary"].is_string());

    let (status, deployment) = post(
        &s,
        "/api/v3/repos/acme/docs/deployments",
        &json!({
            "ref": "feature", "environment": "staging", "description": "preview",
            "transient_environment": true, "required_contexts": [], "auto_merge": false,
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{deployment}");
    let deployment_id = deployment["id"].as_u64().unwrap_or_default();
    assert!(deployment_id > 0);
    assert_eq!(deployment["environment"], "staging");

    let (status, _) = post(
        &s,
        &format!("/api/v3/repos/acme/docs/deployments/{deployment_id}/statuses"),
        &json!({"state": "success", "environment_url": "http://preview.test"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED);
    let deployments = ok(&s, "/api/v3/repos/acme/docs/deployments?environment=staging").await?;
    assert_eq!(deployments.as_array().map(Vec::len), Some(1));

    let (status, _) = post(
        &s,
        &format!("/api/v3/repos/acme/docs/statuses/{}", f.main),
        &json!({"state": "success", "context": "walgit"}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED);
    let combined = ok(
        &s,
        &format!("/api/v3/repos/acme/docs/commits/{}/status", f.main),
    )
    .await?;
    assert_eq!(combined["state"], "success");
    assert_eq!(combined["sha"], f.main);

    let rules = ok(&s, "/api/v3/repos/acme/docs/rules/branches/main").await?;
    assert_eq!(rules.as_array().map(Vec::len), Some(0));

    // The existing wildcard commit route still answers next to these.
    let commit = ok(&s, &format!("/api/v3/repos/acme/docs/commits/{}", f.main)).await?;
    assert_eq!(commit["sha"], f.main);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr_state_survives_a_restart_because_it_lives_in_the_bucket() -> TestResult {
    let s = server().await?;
    let _f = fixture(&s)?;
    post(
        &s,
        "/api/v3/repos/acme/docs/pulls",
        &json!({"title": "persisted", "head": "feature", "base": "main"}),
    )
    .await?;
    post(
        &s,
        "/api/v3/repos/acme/docs/issues/1/comments",
        &json!({"body": "still here"}),
    )
    .await?;

    // A second instance over the same bucket, with its own cache dir.
    let other = s
        .start_sibling_with(|cfg| {
            cfg.github.enabled = true;
            cfg.server.auto_create_on_push = true;
        })
        .await?;
    let pr = ok(&other, "/api/v3/repos/acme/docs/pulls/1").await?;
    assert_eq!(pr["title"], "persisted");
    assert_eq!(pr["state"], "open");
    let comments = ok(&other, "/api/v3/repos/acme/docs/issues/1/comments").await?;
    assert_eq!(comments.as_array().map(Vec::len), Some(1));
    assert_eq!(comments[0]["body"], "still here");
    Ok(())
}
