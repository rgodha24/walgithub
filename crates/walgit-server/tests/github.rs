//! The GitHub Enterprise Server facade (`docs/GITHUB.md`): auth stubs, the
//! repository/commit/branch/ref reads, and a write through `github::write`
//! that a real `git fetch` can see.

mod harness;

use harness::{Server, git_in};
use serde_json::Value;

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

async fn get(s: &Server, path: &str) -> anyhow::Result<(reqwest::StatusCode, Value)> {
    let resp = client()
        .get(format!("{}{path}", s.base_url))
        .header("Authorization", "Bearer anything-at-all")
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, value))
}

async fn ok(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (status, v) = get(s, path).await?;
    anyhow::ensure!(status.is_success(), "GET {path} -> {status}: {v}");
    Ok(v)
}

/// Build a two-commit repository with a tag and push it over smart HTTP —
/// the only way repositories get into the facade.
fn fixture(s: &Server) -> anyhow::Result<(tempfile::TempDir, String)> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "dev@walgit.test"])?;
    git_in(&dir, &["config", "user.name", "Dev"])?;
    std::fs::write(dir.join("README.md"), "# docs\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "initial commit\n\nwith a body"])?;
    std::fs::create_dir_all(dir.join("pages"))?;
    std::fs::write(dir.join("pages/index.mdx"), "hello\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "add a page"])?;
    git_in(&dir, &["tag", "-a", "v1", "-m", "release one"])?;
    git_in(&dir, &["remote", "add", "origin", &s.repo_url("acme", "docs")])?;
    git_in(&dir, &["push", "-q", "origin", "main", "--tags"])?;
    let head = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok((tmp, head))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_stubs_accept_anything_and_report_admin() -> TestResult {
    let s = server().await?;

    let user = ok(&s, "/api/v3/user").await?;
    assert_eq!(user["login"], "mintlify-dev");
    assert_eq!(user["id"], 1);

    let app = ok(&s, "/api/v3/app").await?;
    assert!(
        app["html_url"].as_str().is_some_and(|u| u.contains("/apps/")),
        "app.html_url: {app}"
    );

    let installs = ok(&s, "/api/v3/app/installations").await?;
    assert_eq!(installs[0]["repository_selection"], "all");
    assert_eq!(installs[0]["account"]["login"], "mintlify-dev");

    let one = ok(&s, "/api/v3/app/installations/42").await?;
    assert_eq!(one["id"], 42);
    assert!(one["suspended_by"].is_null());
    assert_eq!(one["permissions"]["contents"], "write");

    let resp = client()
        .post(format!("{}/api/v3/app/installations/42/access_tokens", s.base_url))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let token: Value = resp.json().await?;
    assert_eq!(token["token"], "ghs_dev");
    assert_eq!(token["repository_selection"], "all");
    assert!(token["expires_at"].as_str().is_some_and(|e| e.ends_with('Z')));

    let named = ok(&s, "/api/v3/users/someone").await?;
    assert_eq!(named["login"], "someone");
    assert!(named["email"].as_str().is_some_and(|e| e.contains('@')));

    let limit = ok(&s, "/api/v3/rate_limit").await?;
    assert_eq!(limit["resources"]["core"]["remaining"], 1_000_000);

    let unknown = get(&s, "/api/v3/no/such/thing").await?;
    assert_eq!(unknown.0, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(unknown.1["message"], "Not Found");
    assert!(unknown.1["documentation_url"].is_string());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_web_flow_agrees_immediately() -> TestResult {
    let s = server().await?;
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = no_redirect
        .get(format!(
            "{}/login/oauth/authorize?client_id=x&state=st%2Fate&redirect_uri=http://app.test/cb",
            s.base_url
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(loc.starts_with("http://app.test/cb?code=dev"), "location {loc}");
    assert!(loc.contains("state=st%2Fate"), "location {loc}");

    let json: Value = client()
        .post(format!("{}/login/oauth/access_token", s.base_url))
        .header("Accept", "application/json")
        .body("code=dev")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(json["access_token"], "gho_dev");
    assert_eq!(json["token_type"], "bearer");

    let form = client()
        .post(format!("{}/login/oauth/access_token", s.base_url))
        .body("code=dev")
        .send()
        .await?
        .text()
        .await?;
    assert!(form.contains("access_token=gho_dev"), "form body {form}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_a_pushed_repository_in_github_shapes() -> TestResult {
    let s = server().await?;
    let (_src, head) = fixture(&s)?;

    let repo = ok(&s, "/api/v3/repos/acme/docs").await?;
    assert_eq!(repo["full_name"], "acme/docs");
    assert_eq!(repo["name"], "docs");
    assert_eq!(repo["owner"]["login"], "acme");
    assert_eq!(repo["default_branch"], "main");
    assert_eq!(repo["permissions"]["admin"], true);
    assert_eq!(repo["permissions"]["push"], true);
    assert_eq!(repo["archived"], false);
    assert_eq!(repo["fork"], false);
    assert!(repo["id"].as_u64().is_some_and(|i| i > 0));
    assert!(
        repo["clone_url"].as_str().is_some_and(|u| u.ends_with("/acme/docs.git")),
        "clone_url {repo}"
    );

    let all = ok(&s, "/api/v3/installation/repositories").await?;
    assert_eq!(all["total_count"], 1);
    assert_eq!(all["repositories"][0]["full_name"], "acme/docs");

    for r in ["main", head.as_str(), "v1"] {
        let c = ok(&s, &format!("/api/v3/repos/acme/docs/commits/{r}")).await?;
        assert_eq!(c["sha"], head, "commits/{r}");
        assert_eq!(c["commit"]["message"], "add a page");
        assert!(c["commit"]["tree"]["sha"].is_string());
        // `.cargo/config.toml` pins GIT_AUTHOR_* for everything cargo spawns,
        // so the fixture's identity is the workspace's, not the repo config's.
        assert!(
            c["commit"]["author"]["email"]
                .as_str()
                .is_some_and(|e| e.contains('@')),
            "author email: {c}"
        );
        assert!(c["commit"]["committer"]["date"].is_string());
        assert_eq!(c["parents"].as_array().map(Vec::len), Some(1));
        assert!(c["html_url"].is_string());
        assert!(c["node_id"].is_string());
    }

    let missing = get(&s, "/api/v3/repos/acme/docs/commits/nope").await?;
    assert_eq!(missing.0, reqwest::StatusCode::NOT_FOUND);

    let git_commit = ok(&s, &format!("/api/v3/repos/acme/docs/git/commits/{head}")).await?;
    assert_eq!(git_commit["sha"], head);
    assert_eq!(git_commit["message"], "add a page");
    assert!(git_commit["tree"]["sha"].is_string());

    let list = ok(&s, "/api/v3/repos/acme/docs/commits").await?;
    assert_eq!(list.as_array().map(Vec::len), Some(2));
    assert_eq!(list[0]["sha"], head);

    let paged = client()
        .get(format!("{}/api/v3/repos/acme/docs/commits?per_page=1", s.base_url))
        .send()
        .await?;
    let link = paged
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(link.contains("rel=\"next\""), "link header {link:?}");
    let page1: Value = paged.json().await?;
    assert_eq!(page1.as_array().map(Vec::len), Some(1));

    let filtered = ok(
        &s,
        "/api/v3/repos/acme/docs/commits?path=pages/index.mdx",
    )
    .await?;
    assert_eq!(filtered.as_array().map(Vec::len), Some(1));

    let branch = ok(&s, "/api/v3/repos/acme/docs/branches/main").await?;
    assert_eq!(branch["name"], "main");
    assert_eq!(branch["commit"]["sha"], head);
    assert_eq!(branch["protected"], false);

    let branches = ok(&s, "/api/v3/repos/acme/docs/branches").await?;
    assert_eq!(branches.as_array().map(Vec::len), Some(1));
    assert_eq!(branches[0]["name"], "main");

    let r = ok(&s, "/api/v3/repos/acme/docs/git/ref/heads/main").await?;
    assert_eq!(r["ref"], "refs/heads/main");
    assert_eq!(r["object"]["sha"], head);
    assert_eq!(r["object"]["type"], "commit");

    let tag = ok(&s, "/api/v3/repos/acme/docs/git/ref/tags/v1").await?;
    assert_eq!(tag["object"]["type"], "tag");

    let matching = ok(&s, "/api/v3/repos/acme/docs/git/matching-refs/heads/").await?;
    assert_eq!(matching.as_array().map(Vec::len), Some(1));

    let perm = ok(
        &s,
        "/api/v3/repos/acme/docs/collaborators/mintlify-dev/permission",
    )
    .await?;
    assert_eq!(perm["permission"], "admin");
    assert_eq!(perm["role_name"], "admin");
    assert_eq!(perm["user"]["login"], "mintlify-dev");

    let absent = get(&s, "/api/v3/repos/acme/nothere").await?;
    assert_eq!(absent.0, reqwest::StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creates_and_deletes_refs() -> TestResult {
    let s = server().await?;
    let (_src, head) = fixture(&s)?;

    let created = client()
        .post(format!("{}/api/v3/repos/acme/docs/git/refs", s.base_url))
        .json(&serde_json::json!({ "ref": "refs/heads/topic", "sha": head }))
        .send()
        .await?;
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let body: Value = created.json().await?;
    assert_eq!(body["ref"], "refs/heads/topic");
    assert_eq!(body["object"]["sha"], head);

    let again = client()
        .post(format!("{}/api/v3/repos/acme/docs/git/refs", s.base_url))
        .json(&serde_json::json!({ "ref": "refs/heads/topic", "sha": head }))
        .send()
        .await?;
    assert_eq!(again.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let err: Value = again.json().await?;
    assert_eq!(err["message"], "Reference already exists");
    assert_eq!(err["errors"][0]["field"], "ref");

    let bad_sha = client()
        .patch(format!(
            "{}/api/v3/repos/acme/docs/git/refs/heads/topic",
            s.base_url
        ))
        .json(&serde_json::json!({ "sha": "0".repeat(40) }))
        .send()
        .await?;
    assert_eq!(bad_sha.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    let r = ok(&s, "/api/v3/repos/acme/docs/git/ref/heads/topic").await?;
    assert_eq!(r["object"]["sha"], head);

    let gone = client()
        .delete(format!(
            "{}/api/v3/repos/acme/docs/git/refs/heads/topic",
            s.base_url
        ))
        .send()
        .await?;
    assert_eq!(gone.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        get(&s, "/api/v3/repos/acme/docs/git/ref/heads/topic")
            .await?
            .0,
        reqwest::StatusCode::NOT_FOUND
    );

    let missing = client()
        .delete(format!(
            "{}/api/v3/repos/acme/docs/git/refs/heads/never",
            s.base_url
        ))
        .send()
        .await?;
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_lands_in_the_bucket_and_a_real_fetch_sees_it() -> TestResult {
    use walgit_server::github::write::{Change, CommitOnRef, Signature};

    let s = server().await?;
    let (src, head) = fixture(&s)?;
    let id = walgit_git::RepoId::new("acme", "docs")?;

    let written = walgit_server::github::write::commit_on_ref(
        &s.state,
        &id,
        CommitOnRef {
            ref_name: "refs/heads/main".into(),
            base: Some(head.clone()),
            expected_head: Some(head.clone()),
            changes: vec![
                Change::put("pages/index.mdx", "hello again\n"),
                Change::put("pages/new.mdx", "brand new\n"),
                Change::delete("README.md"),
            ],
            message: "facade write".into(),
            author: Signature::new("Facade", "facade@walgit.test"),
            committer: Signature::new("Facade", "facade@walgit.test"),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("write failed: {}", e.message()))?;

    assert_ne!(written.oid, head);
    assert!(written.seq > 0);

    // Read it back through the facade.
    let c = ok(&s, "/api/v3/repos/acme/docs/commits/main").await?;
    assert_eq!(c["sha"], written.oid);
    assert_eq!(c["commit"]["message"], "facade write");
    assert_eq!(c["commit"]["author"]["name"], "Facade");
    assert_eq!(c["parents"][0]["sha"], head);

    // And through real git, which proves the objects reached the bucket and
    // came back out of the WAL rather than out of a process-local cache.
    let dst = tempfile::tempdir()?;
    harness::git(&["init", "-q", "-b", "main"], dst.path())?;
    harness::git(
        &["remote", "add", "origin", &s.repo_url("acme", "docs")],
        dst.path(),
    )?;
    harness::git(&["fetch", "-q", "origin", "main"], dst.path())?;
    let fetched = git_in(dst.path(), &["rev-parse", "FETCH_HEAD"])?
        .trim()
        .to_string();
    assert_eq!(fetched, written.oid);
    let listing = git_in(dst.path(), &["ls-tree", "-r", "--name-only", "FETCH_HEAD"])?;
    let mut names: Vec<&str> = listing.lines().collect();
    names.sort_unstable();
    assert_eq!(names, ["pages/index.mdx", "pages/new.mdx"]);
    let blob = git_in(dst.path(), &["show", "FETCH_HEAD:pages/new.mdx"])?;
    assert_eq!(blob, "brand new\n");

    // A stale expected head is a 409, and the ref did not move.
    let stale = walgit_server::github::write::commit_on_ref(
        &s.state,
        &id,
        CommitOnRef {
            ref_name: "refs/heads/main".into(),
            base: Some(head.clone()),
            expected_head: Some(head.clone()),
            changes: vec![Change::put("pages/late.mdx", "late\n")],
            message: "should not land".into(),
            author: Signature::new("Facade", "facade@walgit.test"),
            committer: Signature::new("Facade", "facade@walgit.test"),
        },
    )
    .await;
    let status = stale.err().map(|e| e.status());
    assert_eq!(status, Some(axum::http::StatusCode::CONFLICT));
    let after = ok(&s, "/api/v3/repos/acme/docs/git/ref/heads/main").await?;
    assert_eq!(after["object"]["sha"], written.oid);

    drop(src);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graphql_names_the_field_it_cannot_answer() -> TestResult {
    let s = server().await?;
    for path in ["/api/graphql", "/api/v3/graphql"] {
        let body: Value = client()
            .post(format!("{}{path}", s.base_url))
            .json(&serde_json::json!({
                "query": "query getLatestCommit($o:String!){ repository(owner:$o){ ref { name } } }"
            }))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(
            body["errors"][0]["message"],
            "not implemented: getLatestCommit.repository",
            "{path}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_facade_is_absent_unless_it_is_enabled() -> TestResult {
    let s = Server::start().await?;
    assert_eq!(
        s.get_status("/api/v3/user").await?,
        axum::http::StatusCode::NOT_FOUND
    );
    Ok(())
}

#[test]
fn validate_refuses_the_facade_on_a_public_bind() {
    let mut cfg = walgit_config::Config::default();
    cfg.store.bucket = "test".into();
    cfg.github.enabled = true;
    cfg.server.auth.mode = walgit_config::AuthMode::Token;
    cfg.server.auth.tokens = vec![walgit_config::StaticToken {
        principal: "ci".into(),
        token: "t".into(),
        token_env: None,
        write: true,
        admin: false,
    }];
    cfg.server.listen = "0.0.0.0:8080".parse().expect("addr");
    let err = cfg.validate().expect_err("must fail closed");
    assert!(
        err.to_string().contains("github.enabled"),
        "unexpected error: {err}"
    );
}
