//! The object-read surface: git trees, git blobs, the README, source
//! archives, and the per-repository branch-protection toggle.
//!
//! Everything here renders through stock `git` against the **bare** serving
//! copy — `ls-tree`, `cat-file` and `archive` all work on a bare repository,
//! so there is no scratch checkout (`docs/GITHUB.md` §8 predicted one; none
//! turned out to be needed). Object reads go through
//! [`super::repo::objects_view`], which refuses with 503 when this instance
//! serves the repository remotely.
//!
//! This module is also where the plumbing the other read modules share lives:
//! [`git`], [`ls_tree`], [`commit_facts`] and [`base64_github`]. `repo.rs` has
//! private copies of two of these; they are duplicated rather than made public
//! there so this phase touches no existing file.

#![allow(clippy::unused_async)]

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use walgit_git::RepoId;
use walgit_store::{ObjectStoreExt, PutMode};

use super::error::{GhError, GhResult};
use super::models::{self, CommitFacts, Urls};
use super::repo::{self, View};
use crate::AppState;

// ---- shared plumbing ---------------------------------------------------------

/// One record per commit, `\x1e`-separated, fields `\0`-separated. The message
/// (`%B`) is last so it may contain anything but a NUL.
pub const LOG_FORMAT: &str =
    "%x1e%H%x00%T%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";

/// Run `git` in the serving copy. A non-zero exit is a 404: every caller here
/// is naming an object or a revision that the request supplied.
pub async fn git(local: &walgit_git::LocalRepo, args: &[&str]) -> GhResult<Vec<u8>> {
    let out = local
        .git(args)
        .await
        .map_err(|e| GhError::Internal(format!("git: {e}")))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(GhError::not_found(
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

fn parse_commit(record: &str) -> Option<CommitFacts> {
    let mut f = record.split('\0');
    let sha = f.next()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    Some(CommitFacts {
        sha,
        tree: f.next()?.to_string(),
        parents: f.next()?.split_whitespace().map(str::to_string).collect(),
        author_name: f.next()?.to_string(),
        author_email: f.next()?.to_string(),
        author_date: f.next()?.to_string(),
        committer_name: f.next()?.to_string(),
        committer_email: f.next()?.to_string(),
        committer_date: f.next()?.to_string(),
        message: f.next().unwrap_or("").trim_end_matches('\n').to_string(),
    })
}

/// Parse the output of a `git log`/`git show` run with [`LOG_FORMAT`].
pub fn parse_commits(bytes: &[u8]) -> Vec<CommitFacts> {
    String::from_utf8_lossy(bytes)
        .split('\x1e')
        .filter_map(parse_commit)
        .collect()
}

/// One commit object, parsed.
pub async fn commit_facts(local: &walgit_git::LocalRepo, sha: &str) -> GhResult<CommitFacts> {
    let out = git(
        local,
        &[
            "show",
            "-s",
            "--diff-merges=off",
            &format!("--format={LOG_FORMAT}"),
            sha,
        ],
    )
    .await?;
    parse_commits(&out)
        .into_iter()
        .next()
        .ok_or_else(|| GhError::not_found(sha))
}

/// One `ls-tree` row. `size` is `None` for anything that is not a blob.
#[derive(Debug, Clone)]
pub struct Entry {
    pub mode: String,
    /// `blob`, `tree` or `commit` (a submodule).
    pub kind: String,
    pub sha: String,
    pub size: Option<u64>,
    pub path: String,
}

const LS_TREE_FORMAT: &str = "%(objectmode) %(objecttype) %(objectname) %(objectsize) %(path)";

/// `git ls-tree` over a tree-ish. `recursive` descends and yields
/// repo-relative paths; without it the paths are the bare names of one level,
/// which is exactly the distinction GitHub draws.
pub async fn ls_tree(
    local: &walgit_git::LocalRepo,
    treeish: &str,
    recursive: bool,
) -> GhResult<Vec<Entry>> {
    let mut args: Vec<&str> = vec!["ls-tree", "-z", "--full-tree"];
    let fmt = format!("--format={LS_TREE_FORMAT}");
    args.push(&fmt);
    if recursive {
        // `-t` keeps the tree entries themselves, which GitHub's recursive
        // listing includes and `fetchFullGitTree`'s BFS frontier reads.
        args.push("-r");
        args.push("-t");
    }
    args.push(treeish);
    let out = git(local, &args).await?;
    Ok(String::from_utf8_lossy(&out)
        .split('\0')
        .filter_map(parse_ls_tree_row)
        .collect())
}

fn parse_ls_tree_row(row: &str) -> Option<Entry> {
    if row.trim().is_empty() {
        return None;
    }
    let mut f = row.splitn(5, ' ');
    let mode = f.next()?.to_string();
    let kind = f.next()?.to_string();
    let sha = f.next()?.to_string();
    let size = f.next()?.parse::<u64>().ok();
    let path = f.next()?.to_string();
    Some(Entry {
        mode,
        kind,
        sha,
        size,
        path,
    })
}

/// GitHub's base64: standard alphabet, a newline every 60 characters and a
/// trailing newline. Clients decode it with `Buffer.from(_, "base64")`, which
/// ignores the newlines — but a fixture diffed against GitHub's own body
/// should not differ in whitespace.
pub fn base64_github(bytes: &[u8]) -> String {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = String::with_capacity(raw.len() + raw.len() / 60 + 1);
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && i % 60 == 0 {
            out.push('\n');
        }
        out.push(c);
    }
    out.push('\n');
    out
}

fn accept(headers: &HeaderMap) -> &str {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// `application/vnd.github.raw`, `…raw+json` and octokit's
/// `mediaType: {format: "raw"}` (`application/vnd.github.v3.raw`) all mean
/// "the bytes, not the envelope".
pub fn wants_raw(headers: &HeaderMap) -> bool {
    accept(headers).contains("raw")
}

/// `application/vnd.github.object+json` — a directory as an object with
/// `entries` rather than as a bare array.
pub fn wants_object(headers: &HeaderMap) -> bool {
    accept(headers).contains("object")
}

/// The bytes of a blob, with `Content-Type: application/vnd.github.raw`. The
/// bypass client destroys the stream unless the content type starts with that
/// (`docs/GITHUB_SHAPES.md`).
pub fn raw_response(bytes: Vec<u8>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.github.raw"),
        )],
        bytes,
    )
        .into_response()
}

/// The `?ref=` of a contents/readme/archive read: a branch, a tag or a sha,
/// defaulting to the repository's HEAD.
pub async fn resolve_ref(view: &View, r: Option<&str>) -> GhResult<String> {
    if let Some(r) = r.filter(|s| !s.is_empty()) {
        return repo::resolve_commitish(view, r).await;
    }
    let (_, sha) = view
        .index
        .head()
        .ok_or_else(|| GhError::not_found("This repository is empty."))?;
    Ok(sha)
}

#[derive(Deserialize, Default)]
pub struct RefQuery {
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
}

// ---- git/trees ---------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct TreeQuery {
    recursive: Option<String>,
}

fn truthy(v: Option<&String>) -> bool {
    matches!(v.map(String::as_str), Some(s) if !matches!(s, "" | "0" | "false"))
}

/// `GET /api/v3/repos/{o}/{r}/git/trees/{sha}?recursive=1`.
///
/// `{sha}` is a tree sha, a commit sha or a ref name — GitHub accepts all
/// three and `onboardingTemplateSeed` passes `repo.default_branch`.
/// `truncated` is always `false`: there is no cap here, and every caller
/// treats `true` as either a BFS trigger or a fatal error.
pub async fn get_tree(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Query(q): Query<TreeQuery>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let view = repo::objects_view(&st, &id).await?;
    let tree = resolve_tree(&view, &sha).await?;
    let entries = ls_tree(&view.local, &tree, truthy(q.recursive.as_ref())).await?;
    let urls = Urls::from_request(&st, &headers);
    let base = format!("{}/repos/{}", urls.api, view.full_name);
    let out: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let mut v = serde_json::json!({
                "path": e.path,
                "mode": e.mode,
                "type": e.kind,
                "sha": e.sha,
            });
            if e.kind == "blob"
                && let Some(size) = e.size
                && let Some(map) = v.as_object_mut()
            {
                map.insert("size".into(), serde_json::json!(size));
            }
            if let Some(map) = v.as_object_mut() {
                let url = match e.kind.as_str() {
                    "blob" => Some(format!("{base}/git/blobs/{}", e.sha)),
                    "tree" => Some(format!("{base}/git/trees/{}", e.sha)),
                    _ => None,
                };
                if let Some(url) = url {
                    map.insert("url".into(), serde_json::json!(url));
                }
            }
            v
        })
        .collect();
    Ok(axum::Json(serde_json::json!({
        "sha": tree,
        "url": format!("{base}/git/trees/{tree}"),
        "truncated": false,
        "tree": out,
    }))
    .into_response())
}

/// A ref name, a commit sha or a tree sha, all peeled to the tree.
async fn resolve_tree(view: &View, what: &str) -> GhResult<String> {
    let start = match repo::resolve_commitish(view, what).await {
        Ok(commit) => commit,
        Err(_) => what.to_string(),
    };
    if start.is_empty() || start.starts_with('-') {
        return Err(GhError::not_found(what));
    }
    let out = git(
        &view.local,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{start}^{{tree}}"),
        ],
    )
    .await
    .map_err(|_| GhError::not_found(what))?;
    let tree = String::from_utf8_lossy(&out).trim().to_string();
    if tree.is_empty() {
        return Err(GhError::not_found(what));
    }
    Ok(tree)
}

// ---- git/blobs ---------------------------------------------------------------

/// `GET /api/v3/repos/{o}/{r}/git/blobs/{sha}`.
///
/// JSON with base64 `content` by default; the raw bytes when the client asks
/// for `application/vnd.github.raw` — the bypass client's `getFileBufferBySha`
/// consumes that as a byte stream and never parses JSON.
pub async fn get_blob(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let view = repo::objects_view(&st, &id).await?;
    let (oid, bytes) = read_blob(&view, &sha).await?;
    if wants_raw(&headers) {
        return Ok(raw_response(bytes));
    }
    let urls = Urls::from_request(&st, &headers);
    Ok(axum::Json(blob_json(&urls, &view.full_name, &oid, &bytes)).into_response())
}

/// The blob body plus its resolved oid. 404 when the name is not a blob.
pub async fn read_blob(view: &View, what: &str) -> GhResult<(String, Vec<u8>)> {
    if what.is_empty() || what.starts_with('-') {
        return Err(GhError::not_found(what));
    }
    let kind = git(
        &view.local,
        &["cat-file", "-t", "--end-of-options", what],
    )
    .await
    .map_err(|_| GhError::not_found(what))?;
    if String::from_utf8_lossy(&kind).trim() != "blob" {
        return Err(GhError::not_found(what));
    }
    let oid = git(
        &view.local,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", what],
    )
    .await
    .map_err(|_| GhError::not_found(what))?;
    let oid = String::from_utf8_lossy(&oid).trim().to_string();
    let bytes = git(&view.local, &["cat-file", "blob", &oid]).await?;
    Ok((oid, bytes))
}

/// GitHub's git-data blob shape.
pub fn blob_json(urls: &Urls, full_name: &str, oid: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "sha": oid,
        "node_id": models::node_id("Blob", &format!("{full_name}:{oid}")),
        "size": bytes.len(),
        "url": format!("{}/repos/{full_name}/git/blobs/{oid}", urls.api),
        "content": base64_github(bytes),
        "encoding": "base64",
    })
}

// ---- readme ------------------------------------------------------------------

/// GitHub's own preference order, then anything else that starts with
/// `readme` (case-insensitively), alphabetically.
fn readme_rank(name: &str) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    if !lower.starts_with("readme") {
        return None;
    }
    Some(
        ["readme.md", "readme.markdown", "readme.rst", "readme.txt", "readme"]
            .iter()
            .position(|c| *c == lower)
            .unwrap_or(usize::MAX - 1),
    )
}

/// `GET /api/v3/repos/{o}/{r}/readme?ref=` — the root README in the contents
/// file shape, 404 when there is none. `mediaType: {format: "raw"}` gets the
/// bytes (`repoDescription.service` consumes `String(data)`).
pub async fn get_readme(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<RefQuery>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let view = repo::objects_view(&st, &id).await?;
    let commit = resolve_ref(&view, q.git_ref.as_deref()).await?;
    let entries = ls_tree(&view.local, &format!("{commit}^{{tree}}"), false).await?;
    let mut candidates: Vec<(usize, &Entry)> = entries
        .iter()
        .filter(|e| e.kind == "blob")
        .filter_map(|e| readme_rank(&e.path).map(|r| (r, e)))
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
    let entry = candidates
        .first()
        .map(|(_, e)| (*e).clone())
        .ok_or_else(|| GhError::not_found("README"))?;
    let urls = Urls::from_request(&st, &headers);
    super::contents::file_response(&view, &urls, &headers, &commit, &entry).await
}

// ---- zipball / tarball -------------------------------------------------------

/// `GET /api/v3/repos/{o}/{r}/{zipball,tarball}/{ref}`.
///
/// GitHub 302s to codeload; this streams the archive on the 200 instead.
/// Both callers read `response.data` as an `ArrayBuffer` and the tarball
/// caller sets `redirect: "follow"`, so a body on the first response is what
/// they end up with either way — and it saves inventing a second origin that
/// would have to be reachable from the client.
pub async fn archive(
    st: &Arc<AppState>,
    owner: &str,
    name: &str,
    r: Option<&str>,
    zip: bool,
) -> GhResult<Response> {
    let id = repo::repo_id(owner, name)?;
    let view = repo::objects_view(st, &id).await?;
    let commit = resolve_ref(&view, r).await?;
    let short: String = commit.chars().take(7).collect();
    let prefix = format!("{}-{short}/", id.name());
    let (format, content_type, ext) = if zip {
        ("zip", "application/zip", "zip")
    } else {
        ("tar.gz", "application/gzip", "tar.gz")
    };
    let filename = format!("{}-{}-{short}.{ext}", id.owner(), id.name());

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(view.local.path())
        .args([
            "archive",
            "--format",
            format,
            "--prefix",
            &prefix,
            &commit,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| GhError::Internal(format!("spawn git archive: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GhError::Internal("git archive: no stdout".into()))?;
    // Reap the child once the body is done rather than leaving a zombie; the
    // status is uninteresting because the bytes have already been sent.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(stdout));
    let disposition = format!("attachment; filename={filename}");
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// `GET /api/v3/repos/{o}/{r}/zipball[/{ref}]`.
pub async fn zipball(
    State(st): State<Arc<AppState>>,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> GhResult<Response> {
    archive(&st, &owner, &name, Some(r.as_str()), true).await
}

/// `GET /api/v3/repos/{o}/{r}/zipball` — the default branch.
pub async fn zipball_default(
    State(st): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
) -> GhResult<Response> {
    archive(&st, &owner, &name, None, true).await
}

/// `GET /api/v3/repos/{o}/{r}/tarball[/{ref}]`.
pub async fn tarball(
    State(st): State<Arc<AppState>>,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> GhResult<Response> {
    archive(&st, &owner, &name, Some(r.as_str()), false).await
}

/// `GET /api/v3/repos/{o}/{r}/tarball` — the default branch. Template seeding
/// sends `ref: ""`, which octokit renders as this path.
pub async fn tarball_default(
    State(st): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
) -> GhResult<Response> {
    archive(&st, &owner, &name, None, false).await
}

// ---- branch protection -------------------------------------------------------

/// The per-repository protection toggle, at `github/protection.json` under the
/// repository's prefix in the bucket.
///
/// GitHub's rulesets API is an order of magnitude more surface than anything
/// reads here — the server only ever asks "is this branch protected, does it
/// need approvals, may it be force-pushed" — so the facade stores the answer
/// and renders GitHub's rule objects from it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Protection {
    #[serde(default)]
    pub protected_branches: Vec<String>,
    /// What `parameters.required_approving_review_count` reports. `0` (the
    /// default) is a protected branch that still needs no approvals, which is
    /// what a developer exercising the PR flow alone wants.
    #[serde(default)]
    pub required_approving_review_count: u32,
}

impl Protection {
    pub fn covers(&self, branch: &str) -> bool {
        self.protected_branches.iter().any(|b| b == branch)
    }
}

fn protection_key(id: &RepoId) -> String {
    format!("{}github/protection.json", id.store_prefix())
}

/// Read the toggle. An absent or unparseable object is "nothing is protected":
/// this is a development affordance, not a security control.
pub async fn load_protection(st: &Arc<AppState>, id: &RepoId) -> GhResult<Protection> {
    let key = protection_key(id);
    let got = st
        .store
        .get_bytes(&key)
        .await
        .map_err(|e| GhError::Internal(format!("read {key}: {e}")))?;
    let Some((_, bytes)) = got else {
        return Ok(Protection::default());
    };
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

/// `PUT /api/v3/_dev/repos/{o}/{r}/protection` — set the toggle.
///
/// Not a GitHub route: it lives under `_dev` precisely so no client mistakes
/// it for one. The write is a CAS against the object's current version, so two
/// concurrent PUTs cannot silently lose one.
pub async fn set_protection(
    State(st): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Json(body): axum::Json<Protection>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    repo::open(&st, &id).await?;
    let key = protection_key(&id);
    let current = st
        .store
        .head(&key)
        .await
        .map_err(|e| GhError::Internal(format!("head {key}: {e}")))?;
    let mode = current.map_or(PutMode::Create, |m| PutMode::Update(m.version));
    let encoded = serde_json::to_vec(&body)
        .map_err(|e| GhError::Internal(format!("encode protection: {e}")))?;
    st.store
        .put_bytes(&key, encoded, mode)
        .await
        .map_err(|e| match e {
            walgit_store::StoreError::PreconditionFailed { .. } => {
                GhError::Conflict(format!("{key} changed under this write"))
            }
            other => GhError::Internal(format!("write {key}: {other}")),
        })?;
    Ok(axum::Json(body).into_response())
}

fn rules(p: &Protection) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "pull_request",
            "parameters": {
                "required_approving_review_count": p.required_approving_review_count,
                "require_code_owner_review": false,
                "dismiss_stale_reviews_on_push": false,
                "require_last_push_approval": false,
                "required_review_thread_resolution": false,
                "allowed_merge_methods": ["merge", "squash", "rebase"],
            },
        },
        { "type": "non_fast_forward" },
        { "type": "deletion" },
    ])
}

/// `GET /api/v3/repos/{o}/{r}/rules/branches/{branch}` — the rules that apply
/// to a branch, `[]` when it is not protected.
pub async fn branch_rules(
    State(st): State<Arc<AppState>>,
    Path((owner, name, branch)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    repo::open(&st, &id).await?;
    let p = load_protection(&st, &id).await?;
    let body = if p.covers(&branch) {
        rules(&p)
    } else {
        serde_json::json!([])
    };
    Ok(axum::Json(body).into_response())
}

/// GitHub answers an unprotected branch with a 404 whose `message` is
/// `Branch not protected`, not the bare `Not Found` every other 404 carries;
/// `GhError::NotFound` renders the latter, so this one is built by hand.
fn not_protected() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "message": "Branch not protected",
            "documentation_url": "https://docs.github.com/rest",
        })),
    )
        .into_response()
}

/// `GET /api/v3/repos/{o}/{r}/branches/{branch}/protection` — GitHub's legacy
/// protection object, 404 `Branch not protected` when it is not.
///
/// Reached through [`get_branch`] rather than through a route of its own:
/// `branches/{*branch}` is a catch-all (branch names contain `/`), and matchit
/// refuses to register a second pattern underneath one. The cost is that a
/// branch literally named `<x>/protection` is unreachable.
async fn branch_protection(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    branch: &str,
) -> GhResult<Response> {
    let id = repo::repo_id(owner, name)?;
    repo::open(st, &id).await?;
    let p = load_protection(st, &id).await?;
    if !p.covers(branch) {
        return Ok(not_protected());
    }
    let urls = Urls::from_request(st, headers);
    let base = format!(
        "{}/repos/{owner}/{name}/branches/{branch}/protection",
        urls.api
    );
    Ok(axum::Json(serde_json::json!({
        "url": base,
        "required_pull_request_reviews": {
            "url": format!("{base}/required_pull_request_reviews"),
            "dismiss_stale_reviews": false,
            "require_code_owner_reviews": false,
            "require_last_push_approval": false,
            "required_approving_review_count": p.required_approving_review_count,
        },
        "required_status_checks": {
            "url": format!("{base}/required_status_checks"),
            "strict": false,
            "contexts": [],
            "checks": [],
        },
        "enforce_admins": { "url": format!("{base}/enforce_admins"), "enabled": false },
        "required_linear_history": { "enabled": false },
        "allow_force_pushes": { "enabled": false },
        "allow_deletions": { "enabled": false },
        "block_creations": { "enabled": false },
        "required_conversation_resolution": { "enabled": false },
        "lock_branch": { "enabled": false },
        "allow_fork_syncing": { "enabled": false },
    }))
    .into_response())
}

/// `GET /api/v3/repos/{o}/{r}/branches/{branch}`.
///
/// This replaces `repo::get_branch` for one reason: `protected` short-circuits
/// `getBranchProtections` to all-false without ever calling the rules endpoint
/// (`docs/GITHUB_SHAPES.md`), so a hardcoded `false` there would make the
/// toggle above unobservable.
pub async fn get_branch(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
) -> GhResult<Response> {
    if let Some(b) = branch.strip_suffix("/protection") {
        return branch_protection(&st, &headers, &owner, &name, b).await;
    }
    let id = repo::repo_id(&owner, &name)?;
    let view = repo::objects_view(&st, &id).await?;
    let sha = view
        .index
        .branch(&branch)
        .ok_or_else(|| GhError::not_found(&branch))?
        .to_string();
    let facts = commit_facts(&view.local, &sha).await?;
    let urls = Urls::from_request(&st, &headers);
    let protected = load_protection(&st, &id).await?.covers(&branch);
    let mut body = models::branch(&urls, &view.full_name, &branch, &facts);
    if let Some(map) = body.as_object_mut() {
        map.insert("protected".into(), serde_json::json!(protected));
        map.insert(
            "protection".into(),
            serde_json::json!({
                "enabled": protected,
                "required_status_checks": { "enforcement_level": "off", "contexts": [] },
            }),
        );
    }
    Ok(axum::Json(body).into_response())
}

#[cfg(test)]
mod tests {
    use super::{Protection, base64_github, parse_ls_tree_row, readme_rank, truthy};

    #[test]
    fn base64_wraps_at_sixty_like_github() {
        use base64::Engine;
        let s = base64_github(&[b'a'; 100]);
        let lines: Vec<&str> = s.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.first().map(|l| l.len()), Some(60));
        assert!(s.ends_with('\n'));
        let joined = lines.concat();
        let back = base64::engine::general_purpose::STANDARD
            .decode(joined)
            .expect("decodes");
        assert_eq!(back, vec![b'a'; 100]);
    }

    #[test]
    fn ls_tree_rows_keep_spaces_in_paths() {
        let e = parse_ls_tree_row("100644 blob abc123 42 a dir/a file.md").expect("row");
        assert_eq!(e.path, "a dir/a file.md");
        assert_eq!(e.size, Some(42));
        assert_eq!(e.kind, "blob");
        let t = parse_ls_tree_row("040000 tree def456 - pages").expect("row");
        assert_eq!(t.size, None);
        assert!(parse_ls_tree_row("").is_none());
    }

    #[test]
    fn readme_preference_order() {
        assert_eq!(readme_rank("README.md"), Some(0));
        assert!(readme_rank("README.md") < readme_rank("README"));
        assert!(readme_rank("README") < readme_rank("README.adoc"));
        assert_eq!(readme_rank("index.md"), None);
    }

    #[test]
    fn recursive_is_any_truthy_string() {
        assert!(truthy(Some(&"1".to_string())));
        assert!(truthy(Some(&"true".to_string())));
        assert!(!truthy(Some(&"0".to_string())));
        assert!(!truthy(Some(&String::new())));
        assert!(!truthy(None));
    }

    #[test]
    fn protection_defaults_to_nothing_protected() {
        let p: Protection = serde_json::from_str("{}").expect("parses");
        assert!(!p.covers("main"));
        let p: Protection =
            serde_json::from_str(r#"{"protected_branches":["main"]}"#).expect("parses");
        assert!(p.covers("main"));
        assert!(!p.covers("topic"));
        assert_eq!(p.required_approving_review_count, 0);
    }
}
