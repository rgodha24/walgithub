//! `POST /api/graphql` (`docs/GITHUB.md` §GraphQL): every document the
//! Mintlify server sends, verbatim from `docs/GITHUB_SHAPES.md`, against a
//! repository pushed with real `git` — including the write path, which is
//! verified by fetching the commit back with `git`.

mod harness;

use harness::{Server, git_in};
use serde_json::{Value, json};

type TestResult = anyhow::Result<()>;

async fn server() -> anyhow::Result<Server> {
    Server::start_with_tweak(|cfg| {
        cfg.github.enabled = true;
        cfg.server.auto_create_on_push = true;
    })
    .await
}

/// One GraphQL POST. Every answer is a 200; a failure is in the body.
async fn gql(s: &Server, query: &str, variables: Value) -> anyhow::Result<Value> {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/graphql", s.base_url))
        .header("Authorization", "Bearer anything-at-all")
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    let status = resp.status();
    anyhow::ensure!(status == reqwest::StatusCode::OK, "graphql -> {status}");
    // The rate-limit budget a client reads off every response; zero would make
    // it sleep for a minute before retrying (`docs/GITHUB_SHAPES.md`).
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0")
        .to_string();
    anyhow::ensure!(remaining != "0", "x-ratelimit-remaining was {remaining}");
    Ok(resp.json().await?)
}

/// `data` or a failure naming the error the server sent instead.
fn data(body: &Value) -> anyhow::Result<&Value> {
    anyhow::ensure!(
        body.get("errors").is_none_or(|e| e.as_array().is_none_or(Vec::is_empty)),
        "graphql errors: {body}"
    );
    body.get("data")
        .ok_or_else(|| anyhow::anyhow!("no data in {body}"))
}

fn first_error(body: &Value) -> (&str, &str) {
    let e = &body["errors"][0];
    (
        e["type"].as_str().unwrap_or_default(),
        e["message"].as_str().unwrap_or_default(),
    )
}

/// A two-commit repository pushed over smart HTTP — the only way a repository
/// gets into the facade.
fn fixture(s: &Server, owner: &str, name: &str) -> anyhow::Result<(tempfile::TempDir, String)> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "dev@walgit.test"])?;
    git_in(&dir, &["config", "user.name", "Dev"])?;
    std::fs::write(dir.join("README.md"), "# docs\n")?;
    std::fs::create_dir_all(dir.join("pages"))?;
    std::fs::write(dir.join("pages/index.mdx"), "hello\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "initial commit\n\nwith a body"])?;
    std::fs::write(dir.join("pages/index.mdx"), "hello again\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "edit the page"])?;
    git_in(&dir, &["remote", "add", "origin", &s.repo_url(owner, name)])?;
    git_in(&dir, &["push", "-q", "origin", "main"])?;
    let head = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok((tmp, head))
}

// The documents, copied out of `docs/GITHUB_SHAPES.md` without an edit.

const GET_LATEST_COMMIT: &str = "query getLatestCommit($owner: String!, $name: String!, $branch: String!) {
  repository(name: $name, owner: $owner) {
    ref(qualifiedName: $branch) { target { ... on Commit { history(first: 1) { nodes { oid } } } } }
  }
}";

const GET_FILE_SHA: &str = "query($owner: String!, $repo: String!, $expression: String!) {
  repository(owner: $owner, name: $repo) { object(expression: $expression) { ... on Blob { oid } } }
}";

const GET_BRANCHES: &str = "query($owner: String!, $repo: String!, $cursor: String, $queryStr: String) {
  repository(owner: $owner, name: $repo) {
    refs(refPrefix: \"refs/heads/\", first: 100, after: $cursor, query: $queryStr) {
      pageInfo { hasNextPage endCursor }
      nodes { name }
    }
  }
}";

const GET_REPOS: &str = "query($owner: String!, $cursor: String) {
  repositoryOwner(login: $owner) {
    repositories(first: 100, after: $cursor, orderBy: { field: PUSHED_AT, direction: DESC }) {
      pageInfo { hasNextPage endCursor }
      nodes { name }
    }
  }
}";

const SEARCH_REPOS: &str = "query($q: String!, $first: Int!) {
  search(query: $q, type: REPOSITORY, first: $first) { nodes { ... on Repository { nameWithOwner } } }
}";

const CREATE_COMMIT: &str = "mutation CreateCommit($input: CreateCommitOnBranchInput!) {
  createCommitOnBranch(input: $input) { commit { url oid } }
}";

const BLAME_AUTHORS: &str = "query BlameAuthors($owner: String!, $name: String!, $ref: String!, $path: String!) {
  repository(owner: $owner, name: $name) {
    ref(qualifiedName: $ref) {
      target {
        ... on Commit {
          blame(path: $path) {
            ranges { startingLine endingLine commit { committedDate author { email user { email } } } }
          }
        }
      }
    }
  }
}";

const MARK_READY: &str = "mutation ($pullRequestId: ID!) {
  markPullRequestReadyForReview(input: { pullRequestId: $pullRequestId }) { pullRequest { id } }
}";

const TO_DRAFT: &str = "mutation ($pullRequestId: ID!) {
  convertPullRequestToDraft(input: { pullRequestId: $pullRequestId }) { pullRequest { id } }
}";

const ADD_THREAD: &str = "mutation ($pullRequestReviewId: ID!, $path: String!, $body: String!) {
  addPullRequestReviewThread(
    input: { pullRequestReviewId: $pullRequestReviewId, path: $path, body: $body, subjectType: FILE }
  ) { thread { id } }
}";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_answer_the_documents_the_client_sends() -> TestResult {
    let s = server().await?;
    let (tmp, head) = fixture(&s, "acme", "docs")?;

    let body = gql(
        &s,
        GET_LATEST_COMMIT,
        json!({"owner": "acme", "name": "docs", "branch": "main"}),
    )
    .await?;
    let d = data(&body)?;
    assert_eq!(
        d["repository"]["ref"]["target"]["history"]["nodes"][0]["oid"],
        Value::from(head.clone())
    );

    // A branch that is not there is a null ref, not an error: the client falls
    // back to the REST latest-commit read.
    let body = gql(
        &s,
        GET_LATEST_COMMIT,
        json!({"owner": "acme", "name": "docs", "branch": "no-such-branch"}),
    )
    .await?;
    assert!(data(&body)?["repository"]["ref"].is_null(), "{body}");

    // A repository that is not there is GitHub's NOT_FOUND, whose message the
    // client greps for "Could not resolve to a Repository".
    let body = gql(
        &s,
        GET_LATEST_COMMIT,
        json!({"owner": "acme", "name": "nope", "branch": "main"}),
    )
    .await?;
    let (kind, message) = first_error(&body);
    assert_eq!(kind, "NOT_FOUND", "{body}");
    assert!(
        message.contains("Could not resolve to a Repository"),
        "{message}"
    );

    // The blob sha by path, hit and miss.
    let sha = git_in(tmp.path(), &["rev-parse", "HEAD:pages/index.mdx"])?
        .trim()
        .to_string();
    let body = gql(
        &s,
        GET_FILE_SHA,
        json!({"owner": "acme", "repo": "docs", "expression": "main:pages/index.mdx"}),
    )
    .await?;
    assert_eq!(data(&body)?["repository"]["object"]["oid"], Value::from(sha));
    let body = gql(
        &s,
        GET_FILE_SHA,
        json!({"owner": "acme", "repo": "docs", "expression": "main:pages/missing.mdx"}),
    )
    .await?;
    assert!(data(&body)?["repository"]["object"].is_null(), "{body}");

    // Branches.
    let body = gql(
        &s,
        GET_BRANCHES,
        json!({"owner": "acme", "repo": "docs", "cursor": null, "queryStr": null}),
    )
    .await?;
    let refs = &data(&body)?["repository"]["refs"];
    assert_eq!(refs["nodes"][0]["name"], "main");
    assert_eq!(refs["pageInfo"]["hasNextPage"], false);

    // Blame: the date variant rethrows in the client, so the date must parse.
    let body = gql(
        &s,
        BLAME_AUTHORS,
        json!({
            "owner": "acme",
            "name": "docs",
            "ref": "refs/heads/main",
            "path": "pages/index.mdx",
        }),
    )
    .await?;
    let ranges = &data(&body)?["repository"]["ref"]["target"]["blame"]["ranges"];
    assert_eq!(ranges[0]["startingLine"], 1, "{body}");
    // `.cargo/config.toml` pins the identity every test commit is made with.
    assert_eq!(ranges[0]["commit"]["author"]["email"], "tests@walgit.test");
    let date = ranges[0]["commit"]["committedDate"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&date).is_ok(),
        "committedDate {date}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repositories_are_listed_by_owner_and_by_search() -> TestResult {
    let s = server().await?;
    let (_a, _) = fixture(&s, "acme", "docs")?;
    let (_b, _) = fixture(&s, "acme", "handbook")?;
    let (_c, _) = fixture(&s, "other", "docs")?;

    let body = gql(&s, GET_REPOS, json!({"owner": "acme", "cursor": null})).await?;
    let repos = &data(&body)?["repositoryOwner"]["repositories"];
    let mut names: Vec<&str> = repos["nodes"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n["name"].as_str()).collect())
        .unwrap_or_default();
    names.sort_unstable();
    assert_eq!(names, vec!["docs", "handbook"], "{body}");
    assert_eq!(repos["pageInfo"]["hasNextPage"], false);

    // An owner with nothing in the bucket ends the client's pagination.
    let body = gql(&s, GET_REPOS, json!({"owner": "nobody", "cursor": null})).await?;
    assert!(data(&body)?["repositoryOwner"].is_null(), "{body}");

    let body = gql(
        &s,
        SEARCH_REPOS,
        json!({"q": "hand in:name user:acme fork:true", "first": 50}),
    )
    .await?;
    let nodes = &data(&body)?["search"]["nodes"];
    assert_eq!(nodes.as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(nodes[0]["nameWithOwner"], "acme/handbook");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_commit_on_branch_writes_a_commit_git_can_fetch() -> TestResult {
    let s = server().await?;
    let (tmp, head) = fixture(&s, "acme", "docs")?;

    let contents = |s: &str| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(s)
    };
    let input = json!({
        "branch": { "repositoryNameWithOwner": "acme/docs", "branchName": "main" },
        "expectedHeadOid": head,
        "message": { "headline": "editor: add a page", "body": "written by the facade" },
        "fileChanges": {
            "additions": [
                { "path": "pages/new.mdx", "contents": contents("brand new\n") },
                // An addition wins over a deletion of the same path.
                { "path": "pages/index.mdx", "contents": contents("rewritten\n") },
            ],
            "deletions": [
                { "path": "pages/index.mdx" },
                { "path": "README.md" },
            ],
        },
    });
    let body = gql(&s, CREATE_COMMIT, json!({ "input": input })).await?;
    let commit = &data(&body)?["createCommitOnBranch"]["commit"];
    let oid = commit["oid"].as_str().unwrap_or_default().to_string();
    assert_eq!(oid.len(), 40, "{body}");
    assert!(
        commit["url"]
            .as_str()
            .is_some_and(|u| u.contains("/acme/docs/commit/")),
        "commit url: {commit}"
    );

    // Fetch it with real git and read the tree the facade built.
    git_in(tmp.path(), &["fetch", "-q", "origin", "main"])?;
    let tree = git_in(tmp.path(), &["ls-tree", "-r", "--name-only", &oid])?;
    let mut paths: Vec<&str> = tree.lines().collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["pages/index.mdx", "pages/new.mdx"], "{tree}");
    assert_eq!(
        git_in(tmp.path(), &["show", &format!("{oid}:pages/index.mdx")])?,
        "rewritten\n"
    );
    assert_eq!(
        git_in(tmp.path(), &["show", &format!("{oid}:pages/new.mdx")])?,
        "brand new\n"
    );
    assert_eq!(
        git_in(tmp.path(), &["show", "-s", "--format=%an <%ae>", &oid])?.trim(),
        "mintlify-dev <mintlify-dev@localhost>"
    );
    assert_eq!(
        git_in(tmp.path(), &["show", "-s", "--format=%B", &oid])?.trim(),
        "editor: add a page\n\nwritten by the facade"
    );
    assert_eq!(
        git_in(tmp.path(), &["rev-parse", &format!("{oid}^")])?.trim(),
        head
    );

    // The client batches a large payload and chains the next batch on the oid
    // the previous one returned.
    let second = json!({
        "branch": { "repositoryNameWithOwner": "acme/docs", "branchName": "main" },
        "expectedHeadOid": oid,
        "message": { "headline": "editor: second batch" },
        "fileChanges": { "additions": [
            { "path": "pages/second.mdx", "contents": contents("two\n") }
        ] },
    });
    let body = gql(&s, CREATE_COMMIT, json!({ "input": second })).await?;
    let next = data(&body)?["createCommitOnBranch"]["commit"]["oid"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    git_in(tmp.path(), &["fetch", "-q", "origin", "main"])?;
    assert_eq!(
        git_in(tmp.path(), &["rev-parse", &format!("{next}^")])?.trim(),
        oid
    );

    // A stale expectedHeadOid is UNPROCESSABLE with GitHub's wording, not a
    // 409: the client only ever sees a 200 with errors[].
    let stale = json!({
        "branch": { "repositoryNameWithOwner": "acme/docs", "branchName": "main" },
        "expectedHeadOid": head,
        "message": { "headline": "editor: stale" },
        "fileChanges": { "additions": [
            { "path": "pages/third.mdx", "contents": contents("three\n") }
        ] },
    });
    let body = gql(&s, CREATE_COMMIT, json!({ "input": stale })).await?;
    let (kind, message) = first_error(&body);
    assert_eq!(kind, "UNPROCESSABLE", "{body}");
    assert!(
        message.starts_with(&format!("Expected branch to point to \"{head}\" but it did not.")),
        "{message}"
    );

    // A branch that is not there resolves to nothing.
    let missing = json!({
        "branch": { "repositoryNameWithOwner": "acme/docs", "branchName": "nope" },
        "message": { "headline": "editor: missing branch" },
        "fileChanges": { "additions": [] },
    });
    let body = gql(&s, CREATE_COMMIT, json!({ "input": missing })).await?;
    assert_eq!(first_error(&body).0, "NOT_FOUND", "{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_request_mutations_move_state_in_the_bucket() -> TestResult {
    use walgit_server::github::graphql::prs::{NodeId, PullRequest, RefSide};

    let s = server().await?;
    let (_tmp, head) = fixture(&s, "acme", "docs")?;
    let id = walgit_git::RepoId::new("acme", "docs")?;
    let node = NodeId::pull_request(&id, 412);
    let pr = PullRequest {
        number: 412,
        node_id: node.clone(),
        title: "Docs: add quickstart".to_string(),
        body: String::new(),
        state: "open".to_string(),
        draft: true,
        base: RefSide {
            ref_name: "main".to_string(),
            sha: head.clone(),
        },
        head: RefSide {
            ref_name: "editor/quickstart".to_string(),
            sha: head,
        },
        user: "mintlify-dev".to_string(),
        created_at: "2026-08-30T09:00:00Z".to_string(),
        updated_at: "2026-08-30T09:00:00Z".to_string(),
        merged: false,
        merged_at: None,
        merge_commit_sha: None,
        html_url: format!("{}/acme/docs/pull/412", s.base_url),
        review_threads: Vec::new(),
        extra: serde_json::Map::new(),
    };
    walgit_server::github::graphql::prs::create(&s.state, &id, &pr)
        .await
        .map_err(|e| anyhow::anyhow!("seed pull request: {}", e.message))?;

    let body = gql(&s, MARK_READY, json!({ "pullRequestId": node })).await?;
    assert_eq!(
        data(&body)?["markPullRequestReadyForReview"]["pullRequest"]["id"],
        Value::from(node.clone())
    );
    let stored = walgit_server::github::graphql::prs::get(&s.state, &node)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
    assert!(!stored.draft);

    let body = gql(&s, TO_DRAFT, json!({ "pullRequestId": node })).await?;
    data(&body)?;
    let stored = walgit_server::github::graphql::prs::get(&s.state, &node)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
    assert!(stored.draft);
    assert_eq!(stored.title, "Docs: add quickstart");

    // A review thread is addressed by the pending review's node id and lands
    // in the same JSON.
    let review = NodeId::review(&id, 412, "7");
    let body = gql(
        &s,
        ADD_THREAD,
        json!({
            "pullRequestReviewId": review,
            "path": "pages/index.mdx",
            "body": "this paragraph is wrong",
        }),
    )
    .await?;
    let thread = &data(&body)?["addPullRequestReviewThread"]["thread"];
    assert!(thread["id"].as_str().is_some_and(|i| !i.is_empty()), "{body}");
    let stored = walgit_server::github::graphql::prs::get(&s.state, &node)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
    assert_eq!(stored.review_threads.len(), 1);
    assert_eq!(stored.review_threads[0].path, "pages/index.mdx");
    assert_eq!(stored.review_threads[0].review_id.as_deref(), Some("7"));

    // A pull request that was never written is NOT_FOUND.
    let absent = NodeId::pull_request(&id, 999);
    let body = gql(&s, MARK_READY, json!({ "pullRequestId": absent })).await?;
    assert_eq!(first_error(&body).0, "NOT_FOUND", "{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unserved_field_names_itself_at_200() -> TestResult {
    let s = server().await?;
    for (path, query, want) in [
        (
            "/api/graphql",
            "query Whoami { viewer { login } }",
            "not implemented: Whoami.viewer",
        ),
        (
            "/api/v3/graphql",
            "query Sponsors($o:String!){ repository(owner:$o, name:\"docs\"){ discussions { id } } }",
            "not implemented: Sponsors.repository.discussions",
        ),
        (
            "/api/graphql",
            "mutation { closePullRequest(input:{}) { clientMutationId } }",
            "not implemented: mutation.closePullRequest",
        ),
    ] {
        let body: Value = reqwest::Client::new()
            .post(format!("{}{path}", s.base_url))
            .json(&json!({ "query": query, "variables": {"o": "acme"} }))
            .send()
            .await?
            .json()
            .await?;
        assert!(body["data"].is_null(), "{body}");
        assert_eq!(body["errors"][0]["type"], "NOT_IMPLEMENTED", "{body}");
        assert_eq!(body["errors"][0]["message"], want, "{path}");
    }

    // A document that is not GraphQL is still a 200 with an error body.
    let body = gql(&s, "not graphql at all {{{", json!({})).await?;
    assert_eq!(body["errors"][0]["type"], "BAD_REQUEST", "{body}");
    Ok(())
}
