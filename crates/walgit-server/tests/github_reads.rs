//! The read surface of the GitHub facade (`docs/GITHUB.md` §"reads"): git
//! trees and blobs, `contents/{path}` in all four representations, three-dot
//! compare, the README, source archives and the branch-protection toggle.
//!
//! The harness helpers are copied from `github.rs` rather than shared: the two
//! files are written in parallel with the PR and GraphQL phases, and a shared
//! helper module would be a merge conflict in every one of them.

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

/// A GET with an explicit `Accept`, returning the content type and the body
/// bytes — the raw media paths are byte streams, not JSON.
async fn get_accept(
    s: &Server,
    path: &str,
    accept: &str,
) -> anyhow::Result<(reqwest::StatusCode, String, Vec<u8>)> {
    let resp = client()
        .get(format!("{}{path}", s.base_url))
        .header("Accept", accept)
        .send()
        .await?;
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    Ok((status, ct, resp.bytes().await?.to_vec()))
}

fn decode(content: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    let joined: String = content.chars().filter(|c| *c != '\n').collect();
    Ok(base64::engine::general_purpose::STANDARD.decode(joined)?)
}

struct Fixture {
    _src: tempfile::TempDir,
    /// The first commit — the merge base of `main` and `topic`.
    base: String,
    /// `main`'s tip: a rename, a binary add and a modification over `base`.
    main: String,
    /// `topic`'s tip: one commit off `base`, so the two branches diverge.
    topic: String,
}

/// A repository with everything the read surface has to render: a nested
/// directory, a rename, a binary file, a symlink and two divergent branches.
fn fixture(s: &Server) -> anyhow::Result<Fixture> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "dev@walgit.test"])?;
    git_in(&dir, &["config", "user.name", "Dev"])?;
    std::fs::write(dir.join("README.md"), "# docs\n\nread me\n")?;
    std::fs::write(dir.join("docs.json"), "{\"name\":\"docs\"}\n")?;
    std::fs::create_dir_all(dir.join("pages"))?;
    std::fs::write(dir.join("pages/index.mdx"), "a\nb\nc\n")?;
    std::fs::write(dir.join("pages/old.mdx"), "old page, unchanged body\n")?;
    std::os::unix::fs::symlink("pages/index.mdx", dir.join("link.mdx"))?;
    git_in(&dir, &["add", "-A"])?;
    git_in(&dir, &["commit", "-q", "-m", "initial commit"])?;
    let base = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();

    git_in(&dir, &["checkout", "-q", "-b", "topic"])?;
    std::fs::write(dir.join("pages/topic.mdx"), "topic only\n")?;
    git_in(&dir, &["add", "-A"])?;
    git_in(&dir, &["commit", "-q", "-m", "a topic page"])?;
    let topic = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();

    git_in(&dir, &["checkout", "-q", "main"])?;
    std::fs::write(dir.join("pages/index.mdx"), "a\nB\nc\nd\n")?;
    git_in(&dir, &["mv", "pages/old.mdx", "pages/new.mdx"])?;
    std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3, 0xff, 0xfe])?;
    git_in(&dir, &["add", "-A"])?;
    git_in(&dir, &["commit", "-q", "-m", "rename, edit and a binary"])?;
    let main = git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string();

    git_in(&dir, &["remote", "add", "origin", &s.repo_url("acme", "docs")])?;
    git_in(&dir, &["push", "-q", "origin", "main", "topic"])?;
    Ok(Fixture {
        _src: tmp,
        base,
        main,
        topic,
    })
}

fn entry<'a>(tree: &'a Value, path: &str) -> Option<&'a Value> {
    tree.as_array()?.iter().find(|e| e["path"] == path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_trees_in_both_modes() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let flat = ok(&s, &format!("/api/v3/repos/acme/docs/git/trees/{}", f.main)).await?;
    assert_eq!(flat["truncated"], false);
    assert!(flat["sha"].as_str().is_some_and(|s| s.len() == 40));
    assert!(flat["url"].is_string());
    // Non-recursive: bare names, one level only.
    let names: Vec<&str> = flat["tree"]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["path"].as_str()).collect())
        .unwrap_or_default();
    assert!(names.contains(&"pages"), "{names:?}");
    assert!(names.contains(&"README.md"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains('/')), "{names:?}");

    let pages = entry(&flat["tree"], "pages").expect("pages entry");
    assert_eq!(pages["type"], "tree");
    assert_eq!(pages["mode"], "040000");
    assert!(pages.get("size").is_none(), "trees carry no size: {pages}");

    let readme = entry(&flat["tree"], "README.md").expect("README entry");
    assert_eq!(readme["type"], "blob");
    assert_eq!(readme["mode"], "100644");
    assert_eq!(readme["size"], 16);
    assert!(readme["sha"].as_str().is_some_and(|s| s.len() == 40));

    let link = entry(&flat["tree"], "link.mdx").expect("symlink entry");
    assert_eq!(link["mode"], "120000");

    // Recursive: repo-relative paths, tree entries kept.
    let deep = ok(
        &s,
        &format!("/api/v3/repos/acme/docs/git/trees/{}?recursive=1", f.main),
    )
    .await?;
    assert_eq!(deep["truncated"], false);
    assert!(entry(&deep["tree"], "pages/index.mdx").is_some(), "{deep}");
    assert_eq!(
        entry(&deep["tree"], "pages").map(|e| e["type"].clone()),
        Some(Value::from("tree"))
    );
    // Every entry carries the four fields `toTreeNode` refuses to run without.
    for e in deep["tree"].as_array().into_iter().flatten() {
        for field in ["sha", "path", "mode", "type"] {
            assert!(e[field].is_string(), "{field} missing on {e}");
        }
    }

    // A branch name and a bare tree sha both resolve.
    let by_ref = ok(&s, "/api/v3/repos/acme/docs/git/trees/main?recursive=true").await?;
    assert_eq!(by_ref["sha"], deep["sha"]);
    let tree_sha = deep["sha"].as_str().unwrap_or_default().to_string();
    let by_tree = ok(&s, &format!("/api/v3/repos/acme/docs/git/trees/{tree_sha}")).await?;
    assert_eq!(by_tree["sha"], tree_sha);

    assert_eq!(
        get(&s, "/api/v3/repos/acme/docs/git/trees/nope").await?.0,
        reqwest::StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_blobs_as_json_and_as_raw_bytes() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let tree = ok(&s, &format!("/api/v3/repos/acme/docs/git/trees/{}", f.main)).await?;
    let sha = entry(&tree["tree"], "README.md").expect("README")["sha"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let blob = ok(&s, &format!("/api/v3/repos/acme/docs/git/blobs/{sha}")).await?;
    assert_eq!(blob["sha"], sha);
    assert_eq!(blob["size"], 16);
    assert_eq!(blob["encoding"], "base64");
    assert!(blob["node_id"].is_string());
    assert!(blob["url"].is_string());
    let content = blob["content"].as_str().unwrap_or_default();
    assert!(content.ends_with('\n'), "github wraps base64: {content:?}");
    assert_eq!(decode(content)?, b"# docs\n\nread me\n");

    let (status, ct, bytes) = get_accept(
        &s,
        &format!("/api/v3/repos/acme/docs/git/blobs/{sha}"),
        "application/vnd.github.raw+json",
    )
    .await?;
    assert!(status.is_success());
    assert!(ct.starts_with("application/vnd.github.raw"), "content-type {ct}");
    assert_eq!(bytes, b"# docs\n\nread me\n");

    // A tree sha is not a blob.
    let tree_sha = tree["sha"].as_str().unwrap_or_default();
    assert_eq!(
        get(&s, &format!("/api/v3/repos/acme/docs/git/blobs/{tree_sha}"))
            .await?
            .0,
        reqwest::StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contents_serves_directories_files_and_raw() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    // The root, which octokit emits as `contents/` with an empty path.
    let root = ok(&s, "/api/v3/repos/acme/docs/contents").await?;
    let arr = root.as_array().expect("directories are arrays");
    assert!(arr.iter().any(|e| e["path"] == "pages" && e["type"] == "dir"));
    assert!(arr.iter().any(|e| e["path"] == "link.mdx" && e["type"] == "symlink"));
    let readme = arr
        .iter()
        .find(|e| e["path"] == "README.md")
        .expect("README entry");
    assert_eq!(readme["name"], "README.md");
    assert_eq!(readme["type"], "file");
    assert_eq!(readme["size"], 16);
    assert!(readme["sha"].as_str().is_some_and(|s| s.len() == 40));
    assert!(readme["html_url"].is_string());
    assert!(readme["download_url"].is_string());

    // A subdirectory: entries are repo-relative, which is what
    // `getContentDirectorySha` matches `contentDirectory` against.
    let pages = ok(&s, "/api/v3/repos/acme/docs/contents/pages?ref=main").await?;
    let names: Vec<&str> = pages
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["path"].as_str()).collect())
        .unwrap_or_default();
    assert!(names.contains(&"pages/index.mdx"), "{names:?}");
    assert!(names.contains(&"pages/new.mdx"), "{names:?}");

    // A file.
    let file = ok(&s, "/api/v3/repos/acme/docs/contents/pages/index.mdx?ref=main").await?;
    assert_eq!(file["type"], "file");
    assert_eq!(file["encoding"], "base64");
    assert_eq!(file["size"], 8);
    assert!(file["sha"].as_str().is_some_and(|s| s.len() == 40));
    assert_eq!(
        decode(file["content"].as_str().unwrap_or_default())?,
        b"a\nB\nc\nd\n"
    );

    // Pinned to the base commit, the same path has the older body.
    let old = ok(&s, &format!(
        "/api/v3/repos/acme/docs/contents/pages/index.mdx?ref={}",
        f.base
    ))
    .await?;
    assert_eq!(
        decode(old["content"].as_str().unwrap_or_default())?,
        b"a\nb\nc\n"
    );

    // Raw bytes, which `getMediaStreamByPath` requires the content type for.
    let (status, ct, bytes) = get_accept(
        &s,
        "/api/v3/repos/acme/docs/contents/pages/index.mdx?ref=main",
        "application/vnd.github.raw+json",
    )
    .await?;
    assert!(status.is_success());
    assert!(ct.starts_with("application/vnd.github.raw"), "content-type {ct}");
    assert_eq!(bytes, b"a\nB\nc\nd\n");

    // `object+json` turns the array into an envelope with `entries`.
    let (status, _, bytes) = get_accept(
        &s,
        "/api/v3/repos/acme/docs/contents/pages",
        "application/vnd.github.object+json",
    )
    .await?;
    assert!(status.is_success());
    let obj: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(obj["type"], "dir");
    assert!(obj["entries"].as_array().is_some_and(|a| a.len() == 2), "{obj}");

    assert_eq!(
        get(&s, "/api/v3/repos/acme/docs/contents/nope.mdx").await?.0,
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&s, "/api/v3/repos/acme/docs/contents/pages/index.mdx?ref=missing")
            .await?
            .0,
        reqwest::StatusCode::NOT_FOUND
    );

    // An empty repository is a 404, not an empty listing.
    s.put_repo("acme", "empty").await?;
    assert_eq!(
        get(&s, "/api/v3/repos/acme/empty/contents").await?.0,
        reqwest::StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compare_reports_ahead_diverged_and_identical() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    // base...main: one commit ahead, carrying a rename and a binary file.
    let ahead = ok(
        &s,
        &format!("/api/v3/repos/acme/docs/compare/{}...main", f.base),
    )
    .await?;
    assert_eq!(ahead["status"], "ahead");
    assert_eq!(ahead["ahead_by"], 1);
    assert_eq!(ahead["behind_by"], 0);
    assert_eq!(ahead["total_commits"], 1);
    assert_eq!(ahead["merge_base_commit"]["sha"], f.base);
    assert_eq!(ahead["base_commit"]["sha"], f.base);
    let commits = ahead["commits"].as_array().expect("commits");
    assert_eq!(commits.len(), 1);
    let last = commits.last().expect("head commit");
    assert_eq!(last["sha"], f.main);
    assert_eq!(last["commit"]["message"], "rename, edit and a binary");
    assert!(last["commit"]["committer"]["date"].is_string());
    assert!(last["author"]["login"].is_string());
    assert!(last["commit"]["author"]["name"].is_string());

    let files = ahead["files"].as_array().expect("files");
    let by_name = |n: &str| files.iter().find(|f| f["filename"] == n).cloned();

    let renamed = by_name("pages/new.mdx").expect("rename");
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["previous_filename"], "pages/old.mdx");
    assert!(renamed["sha"].as_str().is_some_and(|s| s.len() == 40));
    assert!(renamed["blob_url"].is_string());
    assert!(renamed["raw_url"].is_string());
    assert!(renamed["contents_url"].is_string());

    let binary = by_name("bin.dat").expect("binary");
    assert_eq!(binary["status"], "added");
    assert!(binary.get("patch").is_none(), "binaries carry no patch: {binary}");

    let edited = by_name("pages/index.mdx").expect("edit");
    assert_eq!(edited["status"], "modified");
    assert_eq!(edited["additions"], 2);
    assert_eq!(edited["deletions"], 1);
    assert_eq!(edited["changes"], 3);
    let patch = edited["patch"].as_str().expect("patch");
    assert!(patch.starts_with("@@"), "patch: {patch}");
    assert!(patch.contains("+B"), "patch: {patch}");

    // main...topic: the two branches left the same base in different
    // directions, so `files` is measured from the merge base, not from main.
    let diverged = ok(&s, "/api/v3/repos/acme/docs/compare/main...topic").await?;
    assert_eq!(diverged["status"], "diverged");
    assert_eq!(diverged["ahead_by"], 1);
    assert_eq!(diverged["behind_by"], 1);
    assert_eq!(diverged["merge_base_commit"]["sha"], f.base);
    assert_eq!(diverged["commits"][0]["sha"], f.topic);
    let names: Vec<&str> = diverged["files"]
        .as_array()
        .map(|a| a.iter().filter_map(|f| f["filename"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(names, ["pages/topic.mdx"]);

    let identical = ok(&s, "/api/v3/repos/acme/docs/compare/main...main").await?;
    assert_eq!(identical["status"], "identical");
    assert_eq!(identical["ahead_by"], 0);
    assert_eq!(identical["behind_by"], 0);
    assert_eq!(identical["commits"].as_array().map(Vec::len), Some(0));
    assert_eq!(identical["files"].as_array().map(Vec::len), Some(0));

    // `owner:branch` is accepted by dropping the owner, and the shas resolve.
    let qualified = ok(&s, "/api/v3/repos/acme/docs/compare/acme:main...acme:topic").await?;
    assert_eq!(qualified["status"], "diverged");

    // Pagination: `per_page` bounds both lists.
    let paged = ok(
        &s,
        &format!("/api/v3/repos/acme/docs/compare/{}...main?per_page=1&page=2", f.base),
    )
    .await?;
    assert_eq!(paged["commits"].as_array().map(Vec::len), Some(0));

    assert_eq!(
        get(&s, "/api/v3/repos/acme/docs/compare/main...missing")
            .await?
            .0,
        reqwest::StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readme_in_both_representations() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let readme = ok(&s, "/api/v3/repos/acme/docs/readme").await?;
    assert_eq!(readme["name"], "README.md");
    assert_eq!(readme["path"], "README.md");
    assert_eq!(readme["type"], "file");
    assert_eq!(readme["encoding"], "base64");
    assert_eq!(
        decode(readme["content"].as_str().unwrap_or_default())?,
        b"# docs\n\nread me\n"
    );

    let pinned = ok(&s, &format!("/api/v3/repos/acme/docs/readme?ref={}", f.base)).await?;
    assert_eq!(pinned["sha"], readme["sha"]);

    let (status, ct, bytes) = get_accept(
        &s,
        "/api/v3/repos/acme/docs/readme",
        "application/vnd.github.raw",
    )
    .await?;
    assert!(status.is_success());
    assert!(ct.starts_with("application/vnd.github.raw"), "content-type {ct}");
    assert_eq!(bytes, b"# docs\n\nread me\n");

    // A repository with no README at the root is a 404.
    let bare = tempfile::tempdir()?;
    git_in(bare.path(), &["init", "-q", "-b", "main"])?;
    git_in(bare.path(), &["config", "user.email", "dev@walgit.test"])?;
    git_in(bare.path(), &["config", "user.name", "Dev"])?;
    std::fs::write(bare.path().join("only.mdx"), "x\n")?;
    git_in(bare.path(), &["add", "-A"])?;
    git_in(bare.path(), &["commit", "-q", "-m", "no readme"])?;
    git_in(
        bare.path(),
        &["remote", "add", "origin", &s.repo_url("acme", "bare")],
    )?;
    git_in(bare.path(), &["push", "-q", "origin", "main"])?;
    assert_eq!(
        get(&s, "/api/v3/repos/acme/bare/readme").await?.0,
        reqwest::StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archives_stream_a_real_zip_and_tarball() -> TestResult {
    let s = server().await?;
    let f = fixture(&s)?;

    let resp = client()
        .get(format!("{}/api/v3/repos/acme/docs/zipball/main", s.base_url))
        .send()
        .await?;
    assert!(resp.status().is_success(), "zipball -> {}", resp.status());
    let disposition = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(disposition.contains("attachment"), "{disposition}");
    assert!(disposition.contains("acme-docs-"), "{disposition}");
    let zip = resp.bytes().await?.to_vec();
    assert!(zip.len() > 100, "zip is {} bytes", zip.len());
    assert_eq!(zip.first(), Some(&b'P'));
    assert_eq!(zip.get(1), Some(&b'K'));
    // The archive really contains the tree, under GitHub's `<repo>-<sha>/`
    // prefix that the template extractor strips.
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("repo.zip");
    std::fs::write(&path, &zip)?;
    let listing = std::process::Command::new("unzip")
        .arg("-l")
        .arg(&path)
        .output()?;
    let listing = String::from_utf8_lossy(&listing.stdout).to_string();
    let short: String = f.main.chars().take(7).collect();
    assert!(listing.contains(&format!("docs-{short}/pages/index.mdx")), "{listing}");

    // The tarball defaults to the default branch when the ref is empty, which
    // is what `onboardingTemplateSeed` sends.
    let resp = client()
        .get(format!("{}/api/v3/repos/acme/docs/tarball", s.base_url))
        .send()
        .await?;
    assert!(resp.status().is_success(), "tarball -> {}", resp.status());
    let tgz = resp.bytes().await?.to_vec();
    assert!(tgz.len() > 100, "tarball is {} bytes", tgz.len());
    assert_eq!(tgz.first(), Some(&0x1f));
    assert_eq!(tgz.get(1), Some(&0x8b));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branch_protection_follows_the_bucket_toggle() -> TestResult {
    let s = server().await?;
    let _f = fixture(&s)?;

    // Nothing is protected until the toggle says so.
    let rules = ok(&s, "/api/v3/repos/acme/docs/rules/branches/main").await?;
    assert_eq!(rules.as_array().map(Vec::len), Some(0));
    let branch = ok(&s, "/api/v3/repos/acme/docs/branches/main").await?;
    assert_eq!(branch["protected"], false);
    let (status, body) = get(&s, "/api/v3/repos/acme/docs/branches/main/protection").await?;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(body["message"], "Branch not protected");

    let put = client()
        .put(format!(
            "{}/api/v3/_dev/repos/acme/docs/protection",
            s.base_url
        ))
        .json(&serde_json::json!({
            "protected_branches": ["main"],
            "required_approving_review_count": 1,
        }))
        .send()
        .await?;
    assert!(put.status().is_success(), "PUT -> {}", put.status());

    let rules = ok(&s, "/api/v3/repos/acme/docs/rules/branches/main").await?;
    let arr = rules.as_array().expect("rules");
    let pr = arr
        .iter()
        .find(|r| r["type"] == "pull_request")
        .expect("pull_request rule");
    assert_eq!(pr["parameters"]["required_approving_review_count"], 1);
    assert_eq!(pr["parameters"]["require_code_owner_review"], false);
    assert!(arr.iter().any(|r| r["type"] == "non_fast_forward"), "{rules}");

    // `protected` is what short-circuits `getBranchProtections`, so it has to
    // move with the toggle or the rules above are never asked for.
    let branch = ok(&s, "/api/v3/repos/acme/docs/branches/main").await?;
    assert_eq!(branch["protected"], true);
    assert_eq!(branch["protection"]["enabled"], true);

    let protection = ok(&s, "/api/v3/repos/acme/docs/branches/main/protection").await?;
    assert_eq!(
        protection["required_pull_request_reviews"]["required_approving_review_count"],
        1
    );
    assert_eq!(protection["allow_force_pushes"]["enabled"], false);

    // An unlisted branch is untouched.
    let rules = ok(&s, "/api/v3/repos/acme/docs/rules/branches/topic").await?;
    assert_eq!(rules.as_array().map(Vec::len), Some(0));
    let branch = ok(&s, "/api/v3/repos/acme/docs/branches/topic").await?;
    assert_eq!(branch["protected"], false);

    // Turning it back off is the same call with an empty list.
    let put = client()
        .put(format!(
            "{}/api/v3/_dev/repos/acme/docs/protection",
            s.base_url
        ))
        .json(&serde_json::json!({ "protected_branches": [] }))
        .send()
        .await?;
    assert!(put.status().is_success(), "PUT -> {}", put.status());
    let rules = ok(&s, "/api/v3/repos/acme/docs/rules/branches/main").await?;
    assert_eq!(rules.as_array().map(Vec::len), Some(0));
    Ok(())
}
