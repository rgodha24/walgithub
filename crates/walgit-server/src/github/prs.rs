//! The pull-request surface: PRs, their files and commits, reviews, comments,
//! reactions, issue search, and the two merge endpoints.
//!
//! State lives in the bucket ([`super::pr_store`]); refs, commits and diffs
//! are read out of the repository itself, so a PR never carries a stale head:
//! while it is open, `head.sha` is re-resolved from the branch on every read
//! and only freezes when the PR is merged or closed.

#![allow(clippy::unused_async)]

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use walgit_store::Prefixed;

use super::diff;
use super::error::{FieldError, GhError, GhResult};
use super::merge::{self, Method, Outcome};
use super::models::{self, Urls};
use super::pr_store::{self, Comment, PullRequest, Reaction, Review, Side};
use super::repo::{self, View};
use crate::AppState;

const DEFAULT_PER_PAGE: usize = 30;
const MAX_PER_PAGE: usize = 100;

/// Every route this module owns. Merged into the facade's router.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls",
            get(list_pulls).post(create_pull),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}",
            get(get_pull).patch(patch_pull),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/files",
            get(pull_files),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/commits",
            get(pull_commits),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/merge",
            put(merge_pull),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/reviews",
            get(list_reviews).post(create_review),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}",
            delete(delete_review),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events",
            post(submit_review),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/comments",
            get(list_review_comments).post(create_review_comment),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}/requested_reviewers",
            post(request_reviewers),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/comments/{comment_id}/reactions",
            post(create_reaction),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/comments/{comment_id}/reactions/{reaction_id}",
            delete(delete_reaction),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/{number}",
            get(get_issue),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/{number}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/comments/{comment_id}",
            get(get_comment).patch(patch_comment).delete(delete_comment),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions",
            post(create_reaction),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions/{reaction_id}",
            delete(delete_reaction),
        )
        .route("/api/v3/repos/{owner}/{repo}/merges", post(merge_branches))
        .route(
            "/api/v3/repos/{owner}/{repo}/generate",
            post(super::merge::generate),
        )
        .route("/api/v3/search/issues", get(search_issues))
}

// ---- shared plumbing ---------------------------------------------------------

/// Everything a PR handler needs: the synced repository, its store prefix and
/// the origin the response's URLs are built from.
struct Ctx {
    view: View,
    store: Prefixed,
    urls: Urls,
}

async fn ctx(st: &Arc<AppState>, headers: &HeaderMap, owner: &str, name: &str) -> GhResult<Ctx> {
    let id = repo::repo_id(owner, name)?;
    let view = repo::objects_view(st, &id).await?;
    let store = view.handle.store().clone();
    Ok(Ctx {
        view,
        store,
        urls: Urls::from_request(st, headers),
    })
}

/// Refs only — enough to create a PR or list them, and it answers on an
/// instance that does not hold the packs.
async fn refs_ctx(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> GhResult<Ctx> {
    let id = repo::repo_id(owner, name)?;
    let view = repo::refs_view(st, &id).await?;
    let store = view.handle.store().clone();
    Ok(Ctx {
        view,
        store,
        urls: Urls::from_request(st, headers),
    })
}

struct Page {
    page: usize,
    per_page: usize,
}

impl Page {
    fn new(page: Option<usize>, per_page: Option<usize>) -> Self {
        Self {
            page: page.unwrap_or(1).max(1),
            per_page: per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
        }
    }
    fn skip(&self) -> usize {
        (self.page - 1).saturating_mul(self.per_page)
    }
}

/// GitHub's `Link` header. `octokit.paginate` needs `next` and nothing else,
/// but `prev`/`first` cost nothing and a manual pager reads them.
fn paginated(urls: &Urls, path: &str, page: &Page, more: bool, body: Value) -> Response {
    let mut parts = Vec::new();
    let base = format!("{}{path}", urls.api);
    if more {
        parts.push(format!(
            "<{base}?page={}&per_page={}>; rel=\"next\"",
            page.page + 1,
            page.per_page
        ));
    }
    if page.page > 1 {
        parts.push(format!(
            "<{base}?page={}&per_page={}>; rel=\"prev\"",
            page.page - 1,
            page.per_page
        ));
        parts.push(format!(
            "<{base}?page=1&per_page={}>; rel=\"first\"",
            page.per_page
        ));
    }
    let mut resp = Json(body).into_response();
    if !parts.is_empty()
        && let Ok(v) = axum::http::HeaderValue::from_str(&parts.join(", "))
    {
        resp.headers_mut().insert(axum::http::header::LINK, v);
    }
    resp
}

fn validation(resource: &str, field: &str, message: impl Into<String>) -> GhError {
    GhError::Validation {
        message: "Validation Failed".to_string(),
        errors: vec![FieldError {
            resource: resource.to_string(),
            field: field.to_string(),
            code: "custom".to_string(),
            message: Some(message.into()),
        }],
    }
}

/// `owner:branch` and `branch` both name a branch here — the facade has no
/// forks, so the owner half is decoration.
fn branch_of(spec: &str) -> &str {
    spec.rsplit(':').next().unwrap_or(spec)
}

/// The tip a PR should report for its head: the live branch while it is open,
/// the frozen sha once it is merged or closed.
fn live_head(view: &View, pr: &PullRequest) -> String {
    if !pr.is_open() {
        return pr.head.sha.clone();
    }
    view.index
        .branch(&pr.head.ref_name)
        .map_or_else(|| pr.head.sha.clone(), ToString::to_string)
}

fn repo_json(view: &View, urls: &Urls) -> Value {
    let default_branch = view
        .index
        .head_target
        .strip_prefix("refs/heads/")
        .unwrap_or("main");
    let (owner, name) = view.full_name.split_once('/').unwrap_or(("", ""));
    serde_json::to_value(models::repository(
        urls,
        owner,
        name,
        default_branch,
        models::EPOCH,
    ))
    .unwrap_or(Value::Null)
}

/// Diff-derived numbers a single-PR read reports and a listing does not.
#[derive(Default)]
struct Detail {
    mergeable: Option<bool>,
    mergeable_state: &'static str,
    commits: u64,
    additions: u64,
    deletions: u64,
    changed_files: u64,
}

/// GitHub's PR object. `detail` fills the fields GitHub itself only computes
/// for a single-PR read.
fn pr_json(
    ctx: &Ctx,
    pr: &PullRequest,
    head_sha: &str,
    detail: Option<&Detail>,
) -> Value {
    let urls = &ctx.urls;
    let full = &ctx.view.full_name;
    let base = format!("{}/repos/{full}", urls.api);
    let n = pr.number;
    let owner = full.split('/').next().unwrap_or("");
    let repo = repo_json(&ctx.view, urls);
    let user = serde_json::to_value(models::named_user(urls, &pr.user)).unwrap_or(Value::Null);
    let mut body = json!({
        "id": models::id_for(&format!("{full}#pull{n}")),
        "node_id": pr.node_id,
        "number": n,
        "title": pr.title,
        "body": pr.body,
        "state": pr.state,
        "draft": pr.draft,
        "locked": false,
        "merged": pr.merged,
        "merged_at": pr.merged_at,
        "merge_commit_sha": pr.merge_commit_sha,
        "created_at": pr.created_at,
        "updated_at": pr.updated_at,
        "closed_at": pr.closed_at,
        "html_url": pr.html_url,
        "url": format!("{base}/pulls/{n}"),
        "diff_url": format!("{}/{full}/pull/{n}.diff", urls.html),
        "patch_url": format!("{}/{full}/pull/{n}.patch", urls.html),
        "issue_url": format!("{base}/issues/{n}"),
        "comments_url": format!("{base}/issues/{n}/comments"),
        "review_comments_url": format!("{base}/pulls/{n}/comments"),
        "commits_url": format!("{base}/pulls/{n}/commits"),
        "statuses_url": format!("{base}/statuses/{head_sha}"),
        "head": {
            "label": format!("{owner}:{}", pr.head.ref_name),
            "ref": pr.head.ref_name,
            "sha": head_sha,
            "user": user.clone(),
            "repo": repo.clone(),
        },
        "base": {
            "label": format!("{owner}:{}", pr.base.ref_name),
            "ref": pr.base.ref_name,
            "sha": pr.base.sha,
            "user": user.clone(),
            "repo": repo,
        },
        "user": user.clone(),
        "assignees": [],
        "requested_reviewers": [],
        "requested_teams": [],
        "labels": pr.labels,
        "milestone": Value::Null,
        "author_association": "OWNER",
        "auto_merge": Value::Null,
        "active_lock_reason": Value::Null,
        "maintainer_can_modify": pr.maintainer_can_modify,
        "merged_by": if pr.merged { user.clone() } else { Value::Null },
        "comments": pr.comments.len(),
        "review_comments": pr.review_comments.len(),
    });
    if let Some(d) = detail
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("mergeable".into(), json!(d.mergeable));
        obj.insert("mergeable_state".into(), json!(d.mergeable_state));
        obj.insert("rebaseable".into(), json!(d.mergeable));
        obj.insert("commits".into(), json!(d.commits));
        obj.insert("additions".into(), json!(d.additions));
        obj.insert("deletions".into(), json!(d.deletions));
        obj.insert("changed_files".into(), json!(d.changed_files));
    }
    body
}

/// The PR rendered as an issue — what `GET /issues/{n}` and `/search/issues`
/// answer with.
fn issue_json(ctx: &Ctx, pr: &PullRequest, head_sha: &str) -> Value {
    let full = &ctx.view.full_name;
    let n = pr.number;
    let base = format!("{}/repos/{full}", ctx.urls.api);
    json!({
        "id": models::id_for(&format!("{full}#issue{n}")),
        "node_id": pr.node_id,
        "number": n,
        "title": pr.title,
        "body": pr.body,
        "state": pr.state,
        "draft": pr.draft,
        "locked": false,
        "html_url": pr.html_url,
        "url": format!("{base}/issues/{n}"),
        "comments_url": format!("{base}/issues/{n}/comments"),
        "comments": pr.comments.len(),
        "created_at": pr.created_at,
        "updated_at": pr.updated_at,
        "closed_at": pr.closed_at,
        "labels": pr.labels,
        "assignees": [],
        "user": models::named_user(&ctx.urls, &pr.user),
        "author_association": "OWNER",
        "pull_request": {
            "url": format!("{base}/pulls/{n}"),
            "html_url": pr.html_url,
            "diff_url": format!("{}/{full}/pull/{n}.diff", ctx.urls.html),
            "patch_url": format!("{}/{full}/pull/{n}.patch", ctx.urls.html),
            "merged_at": pr.merged_at,
        },
        "head_sha": head_sha,
    })
}

fn comment_json(ctx: &Ctx, number: u64, c: &Comment, review: bool) -> Value {
    let full = &ctx.view.full_name;
    let base = format!("{}/repos/{full}", ctx.urls.api);
    let kind = if review { "pulls" } else { "issues" };
    json!({
        "id": c.id,
        "node_id": models::node_id("IssueComment", &format!("{full}#{}", c.id)),
        "url": format!("{base}/{kind}/comments/{}", c.id),
        "html_url": format!("{}/{full}/pull/{number}#issuecomment-{}", ctx.urls.html, c.id),
        "issue_url": format!("{base}/issues/{number}"),
        "pull_request_url": format!("{base}/pulls/{number}"),
        "body": c.body,
        "user": models::named_user(&ctx.urls, &c.user),
        "created_at": c.created_at,
        "updated_at": if c.updated_at.is_empty() { &c.created_at } else { &c.updated_at },
        "author_association": "OWNER",
        "path": c.path,
        "line": c.line,
        "original_line": c.line,
        "commit_id": c.commit_id,
        "reactions": {
            "total_count": c.reactions.len(),
            "url": format!("{base}/{kind}/comments/{}/reactions", c.id),
        },
    })
}

fn review_json(ctx: &Ctx, number: u64, r: &Review) -> Value {
    let full = &ctx.view.full_name;
    let base = format!("{}/repos/{full}", ctx.urls.api);
    json!({
        "id": r.id,
        "node_id": r.node_id,
        "state": r.state,
        "body": r.body,
        "user": models::named_user(&ctx.urls, &r.user),
        "submitted_at": r.submitted_at,
        "commit_id": r.commit_id,
        "html_url": format!("{}/{full}/pull/{number}#pullrequestreview-{}", ctx.urls.html, r.id),
        "pull_request_url": format!("{base}/pulls/{number}"),
        "author_association": "OWNER",
    })
}

/// A comment id carries the PR it belongs to, so the by-id routes need no
/// index scan: ids are `number * 1_000_000 + k` (`pr_store::take_id`).
fn pr_of_comment(id: u64) -> u64 {
    id / 1_000_000
}

// ---- pull requests -----------------------------------------------------------

#[derive(Deserialize)]
struct CreatePull {
    title: Option<String>,
    head: String,
    base: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
}

/// `POST /repos/{o}/{r}/pulls`.
async fn create_pull(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(req): Json<CreatePull>,
) -> GhResult<Response> {
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    let head_ref = branch_of(&req.head).to_string();
    let base_ref = branch_of(&req.base).to_string();
    if head_ref == base_ref {
        return Err(validation(
            "PullRequest",
            "head",
            format!("No commits between {base_ref} and {head_ref}"),
        ));
    }
    let head_sha = ctx
        .view
        .index
        .branch(&head_ref)
        .ok_or_else(|| validation("PullRequest", "head", format!("No commit found for the ref {head_ref}")))?
        .to_string();
    let base_sha = ctx
        .view
        .index
        .branch(&base_ref)
        .ok_or_else(|| validation("PullRequest", "base", format!("No commit found for the ref {base_ref}")))?
        .to_string();
    if repo::commit_count(&ctx.view.local, &base_sha, &head_sha).await? == 0 {
        return Err(validation(
            "PullRequest",
            "head",
            format!("No commits between {base_ref} and {head_ref}"),
        ));
    }

    let full = ctx.view.full_name.clone();
    let owner_login = owner.clone();
    let dup_head = head_ref.clone();
    let dup_base = base_ref.clone();
    let created = pr_store::create(
        &ctx.store,
        |index| {
            let clash = index
                .prs
                .iter()
                .find(|r| r.state == "open" && r.head_ref == dup_head && r.base_ref == dup_base);
            match clash {
                Some(row) => Err(validation(
                    "PullRequest",
                    "base",
                    format!(
                        "A pull request already exists for {owner_login}:{dup_head} (#{}).",
                        row.number
                    ),
                )),
                None => Ok(()),
            }
        },
        |number| {
            let now = pr_store::now();
            PullRequest {
                number,
                node_id: pr_store::node_id(&full, number),
                title: req
                    .title
                    .clone()
                    .unwrap_or_else(|| head_ref.clone()),
                body: req.body.clone().unwrap_or_default(),
                state: "open".to_string(),
                draft: req.draft,
                base: Side {
                    ref_name: base_ref.clone(),
                    sha: base_sha.clone(),
                },
                head: Side {
                    ref_name: head_ref.clone(),
                    sha: head_sha.clone(),
                },
                user: super::auth::USER_LOGIN.to_string(),
                created_at: now.clone(),
                updated_at: now,
                closed_at: None,
                merged: false,
                merged_at: None,
                merge_commit_sha: None,
                html_url: format!("{}/{full}/pull/{number}", ctx.urls.html),
                labels: Vec::new(),
                comments: Vec::new(),
                reviews: Vec::new(),
                review_comments: Vec::new(),
                review_threads: Vec::new(),
                maintainer_can_modify: true,
                next_id: 0,
            }
        },
    )
    .await?;

    let head_sha = live_head(&ctx.view, &created);
    Ok((
        StatusCode::CREATED,
        Json(pr_json(&ctx, &created, &head_sha, None)),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
struct ListQuery {
    state: Option<String>,
    head: Option<String>,
    base: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    page: Option<usize>,
    per_page: Option<usize>,
}

/// `GET /repos/{o}/{r}/pulls` — the index does the filtering, so a listing is
/// one GET plus one GET per PR on the page.
async fn list_pulls(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let (index, _) = pr_store::read_index(&ctx.store).await?;
    let want_state = q.state.as_deref().unwrap_or("open");
    let head = q.head.as_deref().map(branch_of);
    let base = q.base.as_deref().map(branch_of);
    let mut rows: Vec<_> = index
        .prs
        .iter()
        .filter(|r| want_state == "all" || r.state == want_state)
        .filter(|r| head.is_none_or(|h| r.head_ref == h))
        .filter(|r| base.is_none_or(|b| r.base_ref == b))
        .collect();
    match q.sort.as_deref() {
        Some("updated") => rows.sort_by(|a, b| a.updated_at.cmp(&b.updated_at)),
        // `popularity` and `long-running` have no meaning here; GitHub's
        // clients only use them to get a stable order.
        _ => rows.sort_by_key(|r| r.number),
    }
    if q.direction.as_deref() != Some("asc") {
        rows.reverse();
    }
    let page = Page::new(q.page, q.per_page);
    let total = rows.len();
    let mut out = Vec::new();
    for row in rows.iter().skip(page.skip()).take(page.per_page) {
        if let Some(pr) = pr_store::try_read(&ctx.store, row.number).await? {
            let head_sha = live_head(&ctx.view, &pr);
            out.push(pr_json(&ctx, &pr, &head_sha, None));
        }
    }
    let more = page.skip() + out.len() < total;
    Ok(paginated(
        &ctx.urls,
        &format!("/repos/{}/pulls", ctx.view.full_name),
        &page,
        more,
        Value::Array(out),
    ))
}

/// `GET /repos/{o}/{r}/pulls/{n}` — the field-hungry read, diff numbers and
/// mergeability included.
async fn get_pull(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let head_sha = live_head(&ctx.view, &pr);
    let detail = detail_for(&ctx, &pr, &head_sha).await?;
    Ok(Json(pr_json(&ctx, &pr, &head_sha, Some(&detail))).into_response())
}

async fn detail_for(ctx: &Ctx, pr: &PullRequest, head_sha: &str) -> GhResult<Detail> {
    let base_sha = ctx
        .view
        .index
        .branch(&pr.base.ref_name)
        .map_or_else(|| pr.base.sha.clone(), ToString::to_string);
    let merge_base = repo::merge_base(&ctx.view.local, &base_sha, head_sha)
        .await?
        .unwrap_or_else(|| base_sha.clone());
    let stats = diff::stats(&ctx.view.local, &merge_base, head_sha).await?;
    let commits = repo::commit_count(&ctx.view.local, &merge_base, head_sha).await?;
    let (mergeable, state) = if pr.merged {
        (None, "merged")
    } else if !pr.is_open() {
        (None, "unknown")
    } else {
        let scratch = super::write::Scratch::new(ctx.view.local.path()).await?;
        match scratch.merge_tree(&base_sha, head_sha, None).await? {
            Some(_) => (Some(true), "clean"),
            None => (Some(false), "dirty"),
        }
    };
    Ok(Detail {
        mergeable,
        mergeable_state: state,
        commits,
        additions: stats.additions,
        deletions: stats.deletions,
        changed_files: stats.changed_files,
    })
}

#[derive(Deserialize)]
struct PatchPull {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    maintainer_can_modify: Option<bool>,
}

/// `PATCH /repos/{o}/{r}/pulls/{n}` — title, body, base, and open/closed.
async fn patch_pull(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(req): Json<PatchPull>,
) -> GhResult<Response> {
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    if let Some(base) = req.base.as_deref() {
        let branch = branch_of(base);
        if ctx.view.index.branch(branch).is_none() {
            return Err(validation(
                "PullRequest",
                "base",
                format!("No commit found for the ref {branch}"),
            ));
        }
    }
    let index = ctx.view.index.clone();
    let pr = pr_store::update(&ctx.store, number, |pr| {
        if let Some(t) = &req.title {
            pr.title.clone_from(t);
        }
        if let Some(b) = &req.body {
            pr.body.clone_from(b);
        }
        if let Some(d) = req.draft {
            pr.draft = d;
        }
        if let Some(m) = req.maintainer_can_modify {
            pr.maintainer_can_modify = m;
        }
        if let Some(base) = &req.base {
            let branch = branch_of(base);
            pr.base.ref_name = branch.to_string();
            if let Some(sha) = index.branch(branch) {
                pr.base.sha = sha.to_string();
            }
        }
        match req.state.as_deref() {
            Some("closed") => {
                if pr.is_open() {
                    if let Some(sha) = index.branch(&pr.head.ref_name) {
                        pr.head.sha = sha.to_string();
                    }
                    pr.state = "closed".to_string();
                    pr.closed_at = Some(pr_store::now());
                }
            }
            Some("open") => {
                if pr.merged {
                    return Err(validation(
                        "PullRequest",
                        "state",
                        "A merged pull request cannot be reopened.",
                    ));
                }
                pr.state = "open".to_string();
                pr.closed_at = None;
            }
            _ => {}
        }
        Ok(())
    })
    .await?;
    let head_sha = live_head(&ctx.view, &pr);
    Ok(Json(pr_json(&ctx, &pr, &head_sha, None)).into_response())
}

#[derive(Deserialize, Default)]
struct PageQuery {
    page: Option<usize>,
    per_page: Option<usize>,
}

/// `GET /repos/{o}/{r}/pulls/{n}/files` — the diff from the merge base to the
/// head, GitHub's per-file shape, paginated with a `Link` header because
/// `octokit.paginate` drives this endpoint.
async fn pull_files(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let head_sha = live_head(&ctx.view, &pr);
    let base_sha = ctx
        .view
        .index
        .branch(&pr.base.ref_name)
        .map_or_else(|| pr.base.sha.clone(), ToString::to_string);
    let merge_base = repo::merge_base(&ctx.view.local, &base_sha, &head_sha)
        .await?
        .unwrap_or(base_sha);
    let files = diff::changed_files(&ctx.view.local, &merge_base, &head_sha).await?;
    let page = Page::new(q.page, q.per_page);
    let total = files.len();
    let full = &ctx.view.full_name;
    let out: Vec<Value> = files
        .iter()
        .skip(page.skip())
        .take(page.per_page)
        .map(|f| diff::file_json(&ctx.urls, full, &head_sha, f))
        .collect();
    let more = page.skip() + out.len() < total;
    Ok(paginated(
        &ctx.urls,
        &format!("/repos/{full}/pulls/{number}/files"),
        &page,
        more,
        Value::Array(out),
    ))
}

/// `GET /repos/{o}/{r}/pulls/{n}/commits`.
async fn pull_commits(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let head_sha = live_head(&ctx.view, &pr);
    let base_sha = ctx
        .view
        .index
        .branch(&pr.base.ref_name)
        .map_or_else(|| pr.base.sha.clone(), ToString::to_string);
    let merge_base = repo::merge_base(&ctx.view.local, &base_sha, &head_sha)
        .await?
        .unwrap_or(base_sha);
    let shas = repo::commits_between(&ctx.view.local, &merge_base, &head_sha).await?;
    let page = Page::new(q.page, q.per_page);
    let total = shas.len();
    let mut out = Vec::new();
    for sha in shas.iter().skip(page.skip()).take(page.per_page) {
        let facts = repo::commit_facts(&ctx.view.local, sha).await?;
        out.push(models::commit(&ctx.urls, &ctx.view.full_name, &facts));
    }
    let more = page.skip() + out.len() < total;
    Ok(paginated(
        &ctx.urls,
        &format!("/repos/{}/pulls/{number}/commits", ctx.view.full_name),
        &page,
        more,
        Value::Array(out),
    ))
}

/// `GET /repos/{o}/{r}/commits/{*ref}` and everything nested under it.
///
/// `commits/{ref}` is registered as a catch-all, and matchit refuses to put a
/// path *parameter* beside a catch-all — so `commits/{sha}/pulls`,
/// `.../check-runs`, `.../status` and `.../statuses` cannot be routes of their
/// own and are dispatched off the tail here instead.
pub async fn commit_or_subroute(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> Response {
    for (suffix, kind) in [
        ("/pulls", Sub::Pulls),
        ("/check-runs", Sub::CheckRuns),
        ("/status", Sub::Status),
        ("/statuses", Sub::Statuses),
    ] {
        let Some(sha) = r.strip_suffix(suffix) else {
            continue;
        };
        if sha.is_empty() {
            continue;
        }
        return match kind {
            Sub::Pulls => commit_pulls(&st, &headers, &owner, &name, sha)
                .await
                .unwrap_or_else(IntoResponse::into_response),
            Sub::CheckRuns => super::stubs::list_check_runs(sha),
            Sub::Status => {
                let urls = Urls::from_request(&st, &headers);
                super::stubs::combined_status(&urls, &format!("{owner}/{name}"), sha)
            }
            Sub::Statuses => super::stubs::commit_statuses(),
        };
    }
    repo::get_commit(State(st), headers, Path((owner, name, r)))
        .await
        .into_response()
}

enum Sub {
    Pulls,
    CheckRuns,
    Status,
    Statuses,
}

/// `GET /repos/{o}/{r}/commits/{sha}/pulls`. The caller may pass a branch name
/// instead of a sha, so all three of head sha, merge commit and head ref match.
async fn commit_pulls(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    sha: &str,
) -> GhResult<Response> {
    let sha = sha.to_string();
    let ctx = refs_ctx(st, headers, owner, name).await?;
    let resolved = ctx.view.index.branch(&sha).map(ToString::to_string);
    let (index, _) = pr_store::read_index(&ctx.store).await?;
    let matches = |row: &pr_store::Row| {
        row.head_sha == sha
            || row.head_ref == sha
            || row.merge_commit_sha.as_deref() == Some(sha.as_str())
            || resolved.as_deref().is_some_and(|r| row.head_sha == r)
    };
    let mut out = Vec::new();
    for row in index.prs.iter().filter(|r| matches(r)) {
        if let Some(pr) = pr_store::try_read(&ctx.store, row.number).await? {
            let head_sha = live_head(&ctx.view, &pr);
            out.push(pr_json(&ctx, &pr, &head_sha, None));
        }
    }
    Ok(Json(out).into_response())
}

// ---- merging -----------------------------------------------------------------

#[derive(Deserialize, Default)]
struct MergeRequest {
    #[serde(default)]
    merge_method: Option<String>,
    #[serde(default)]
    commit_title: Option<String>,
    #[serde(default)]
    commit_message: Option<String>,
    #[serde(default)]
    sha: Option<String>,
}

/// `PUT /repos/{o}/{r}/pulls/{n}/merge`.
async fn merge_pull(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    body: Option<Json<MergeRequest>>,
) -> GhResult<Response> {
    let req = body.map_or_else(MergeRequest::default, |Json(b)| b);
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    if pr.merged {
        return Ok(merge::status_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Pull Request is already merged",
        ));
    }
    if !pr.is_open() {
        return Ok(merge::status_error(
            StatusCode::METHOD_NOT_ALLOWED,
            merge::NOT_MERGEABLE,
        ));
    }
    let head_sha = live_head(&ctx.view, &pr);
    if let Some(want) = req.sha.as_deref().filter(|s| !s.is_empty())
        && want != head_sha
    {
        return Ok(merge::status_error(
            StatusCode::CONFLICT,
            merge::HEAD_MODIFIED,
        ));
    }
    let method = Method::parse(req.merge_method.as_deref().unwrap_or("merge"))?;
    let title = req.commit_title.clone().unwrap_or_else(|| match method {
        Method::Squash => format!("{} (#{number})", pr.title),
        _ => format!("Merge pull request #{number} from {}", pr.head.ref_name),
    });
    // GitHub defaults a merge commit's body to the PR title and a squash's to
    // the PR description; neither client sends `commit_message`.
    let message = req.commit_message.clone().unwrap_or_else(|| match method {
        Method::Squash => pr.body.clone(),
        _ => pr.title.clone(),
    });
    let base_ref = format!("refs/heads/{}", pr.base.ref_name);
    let outcome = merge::merge_into_ref(
        &st,
        ctx.view.handle.id(),
        &base_ref,
        &head_sha,
        method,
        &title,
        &message,
    )
    .await?;
    let sha = match outcome {
        Outcome::Conflict => {
            return Ok(merge::status_error(
                StatusCode::METHOD_NOT_ALLOWED,
                merge::NOT_MERGEABLE,
            ));
        }
        Outcome::UpToDate => head_sha.clone(),
        Outcome::Merged(sha) => sha,
    };
    let merged_sha = sha.clone();
    let frozen = head_sha.clone();
    pr_store::update(&ctx.store, number, |pr| {
        pr.state = "closed".to_string();
        pr.merged = true;
        pr.merged_at = Some(pr_store::now());
        pr.closed_at = Some(pr_store::now());
        pr.merge_commit_sha = Some(merged_sha.clone());
        pr.head.sha.clone_from(&frozen);
        Ok(())
    })
    .await?;
    Ok(Json(json!({
        "sha": sha,
        "merged": true,
        "message": "Pull Request successfully merged",
    }))
    .into_response())
}

#[derive(Deserialize)]
struct MergeBranches {
    base: String,
    head: String,
    #[serde(default)]
    commit_message: Option<String>,
}

/// `POST /repos/{o}/{r}/merges` — the same machinery with no PR attached.
/// `204` when there is nothing to merge, `409` on a conflict.
async fn merge_branches(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(req): Json<MergeBranches>,
) -> GhResult<Response> {
    let ctx = ctx(&st, &headers, &owner, &name).await?;
    let base = branch_of(&req.base).to_string();
    let head_sha = match ctx.view.index.branch(branch_of(&req.head)) {
        Some(sha) => sha.to_string(),
        None => repo::resolve_commitish(&ctx.view, branch_of(&req.head)).await?,
    };
    if ctx.view.index.branch(&base).is_none() {
        return Err(GhError::not_found(base));
    }
    let title = req
        .commit_message
        .clone()
        .unwrap_or_else(|| format!("Merge {} into {base}", req.head));
    let outcome = merge::merge_into_ref(
        &st,
        ctx.view.handle.id(),
        &format!("refs/heads/{base}"),
        &head_sha,
        Method::Merge,
        &title,
        "",
    )
    .await?;
    match outcome {
        Outcome::UpToDate => Ok(StatusCode::NO_CONTENT.into_response()),
        Outcome::Conflict => Ok(merge::status_error(
            StatusCode::CONFLICT,
            "Merge conflict",
        )),
        Outcome::Merged(sha) => {
            let facts = repo::commit_facts(&ctx.view.local, &sha).await?;
            let commit = models::commit(&ctx.urls, &ctx.view.full_name, &facts);
            Ok((StatusCode::CREATED, Json(commit)).into_response())
        }
    }
}

// ---- reviews -----------------------------------------------------------------

/// `GET /repos/{o}/{r}/pulls/{n}/reviews`.
async fn list_reviews(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let page = Page::new(q.page, q.per_page);
    let total = pr.reviews.len();
    let out: Vec<Value> = pr
        .reviews
        .iter()
        .skip(page.skip())
        .take(page.per_page)
        .map(|r| review_json(&ctx, number, r))
        .collect();
    let more = page.skip() + out.len() < total;
    Ok(paginated(
        &ctx.urls,
        &format!("/repos/{}/pulls/{number}/reviews", ctx.view.full_name),
        &page,
        more,
        Value::Array(out),
    ))
}

#[derive(Deserialize, Default)]
struct CreateReview {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    commit_id: Option<String>,
    #[serde(default)]
    comments: Vec<ReviewCommentInput>,
}

#[derive(Deserialize)]
struct ReviewCommentInput {
    path: String,
    #[serde(default)]
    line: Option<u64>,
    body: String,
}

fn review_state(event: Option<&str>) -> &'static str {
    match event {
        Some("APPROVE") => "APPROVED",
        Some("REQUEST_CHANGES") => "CHANGES_REQUESTED",
        Some("COMMENT") => "COMMENTED",
        // A body-less POST creates a *pending* review whose node id is fed to
        // the GraphQL thread mutation (`docs/GITHUB_SHAPES.md`, Tier 3).
        _ => "PENDING",
    }
}

/// `POST /repos/{o}/{r}/pulls/{n}/reviews`.
async fn create_review(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    body: Option<Json<CreateReview>>,
) -> GhResult<Response> {
    let req = body.map_or_else(CreateReview::default, |Json(b)| b);
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let full = ctx.view.full_name.clone();
    let state = review_state(req.event.as_deref());
    let mut created_id = 0;
    let pr = pr_store::update(&ctx.store, number, |pr| {
        let head = pr.head.sha.clone();
        let id = pr.take_id();
        created_id = id;
        pr.reviews.push(Review {
            id,
            node_id: models::node_id("PullRequestReview", &format!("{full}#{id}")),
            state: state.to_string(),
            body: req.body.clone().unwrap_or_default(),
            user: super::auth::USER_LOGIN.to_string(),
            submitted_at: (state != "PENDING").then(pr_store::now),
            commit_id: req.commit_id.clone().unwrap_or(head),
        });
        for c in &req.comments {
            let cid = pr.take_id();
            pr.review_comments.push(Comment {
                id: cid,
                body: c.body.clone(),
                user: super::auth::USER_LOGIN.to_string(),
                created_at: pr_store::now(),
                updated_at: String::new(),
                path: Some(c.path.clone()),
                line: c.line,
                commit_id: req.commit_id.clone(),
                reactions: Vec::new(),
            });
        }
        Ok(())
    })
    .await?;
    let review = pr
        .reviews
        .iter()
        .find(|r| r.id == created_id)
        .ok_or_else(|| GhError::Internal("review vanished".into()))?;
    Ok((StatusCode::OK, Json(review_json(&ctx, number, review))).into_response())
}

/// `POST /repos/{o}/{r}/pulls/{n}/reviews/{id}/events` — submit a pending
/// review.
async fn submit_review(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, review_id)): Path<(String, String, u64, u64)>,
    body: Option<Json<CreateReview>>,
) -> GhResult<Response> {
    let req = body.map_or_else(CreateReview::default, |Json(b)| b);
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let state = review_state(req.event.as_deref());
    let pr = pr_store::update(&ctx.store, number, |pr| {
        let Some(r) = pr.reviews.iter_mut().find(|r| r.id == review_id) else {
            return Err(GhError::not_found(format!("review {review_id}")));
        };
        r.state = state.to_string();
        if let Some(b) = &req.body {
            r.body.clone_from(b);
        }
        r.submitted_at = Some(pr_store::now());
        Ok(())
    })
    .await?;
    let review = pr
        .reviews
        .iter()
        .find(|r| r.id == review_id)
        .ok_or_else(|| GhError::not_found(format!("review {review_id}")))?;
    Ok(Json(review_json(&ctx, number, review)).into_response())
}

/// `DELETE /repos/{o}/{r}/pulls/{n}/reviews/{id}` — the cleanup path a failed
/// review-thread mutation takes.
async fn delete_review(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, review_id)): Path<(String, String, u64, u64)>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    pr_store::update(&ctx.store, number, |pr| {
        pr.reviews.retain(|r| r.id != review_id);
        Ok(())
    })
    .await?;
    Ok(StatusCode::OK.into_response())
}

/// `POST /repos/{o}/{r}/pulls/{n}/requested_reviewers` — nothing to request.
async fn request_reviewers(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let head_sha = live_head(&ctx.view, &pr);
    Ok((
        StatusCode::CREATED,
        Json(pr_json(&ctx, &pr, &head_sha, None)),
    )
        .into_response())
}

// ---- comments ----------------------------------------------------------------

#[derive(Deserialize)]
struct CommentBody {
    body: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    commit_id: Option<String>,
}

/// `GET /repos/{o}/{r}/issues/{n}/comments`.
async fn list_comments(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    Ok(comment_page(&ctx, number, &pr.comments, &q, false, "issues"))
}

/// `GET /repos/{o}/{r}/pulls/{n}/comments` — review comments.
async fn list_review_comments(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    Ok(comment_page(
        &ctx,
        number,
        &pr.review_comments,
        &q,
        true,
        "pulls",
    ))
}

fn comment_page(
    ctx: &Ctx,
    number: u64,
    comments: &[Comment],
    q: &PageQuery,
    review: bool,
    kind: &str,
) -> Response {
    let page = Page::new(q.page, q.per_page);
    let total = comments.len();
    let out: Vec<Value> = comments
        .iter()
        .skip(page.skip())
        .take(page.per_page)
        .map(|c| comment_json(ctx, number, c, review))
        .collect();
    let more = page.skip() + out.len() < total;
    paginated(
        &ctx.urls,
        &format!("/repos/{}/{kind}/{number}/comments", ctx.view.full_name),
        &page,
        more,
        Value::Array(out),
    )
}

/// `POST /repos/{o}/{r}/issues/{n}/comments`.
async fn create_comment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(req): Json<CommentBody>,
) -> GhResult<Response> {
    add_comment(&st, &headers, &owner, &name, number, req, false).await
}

/// `POST /repos/{o}/{r}/pulls/{n}/comments` — a review comment on a file.
async fn create_review_comment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(req): Json<CommentBody>,
) -> GhResult<Response> {
    add_comment(&st, &headers, &owner, &name, number, req, true).await
}

async fn add_comment(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    number: u64,
    req: CommentBody,
    review: bool,
) -> GhResult<Response> {
    let ctx = refs_ctx(st, headers, owner, name).await?;
    let mut created = 0;
    let pr = pr_store::update(&ctx.store, number, |pr| {
        let id = pr.take_id();
        created = id;
        let c = Comment {
            id,
            body: req.body.clone(),
            user: super::auth::USER_LOGIN.to_string(),
            created_at: pr_store::now(),
            updated_at: String::new(),
            path: req.path.clone(),
            line: req.line,
            commit_id: req.commit_id.clone(),
            reactions: Vec::new(),
        };
        if review {
            pr.review_comments.push(c);
        } else {
            pr.comments.push(c);
        }
        Ok(())
    })
    .await?;
    let list = if review { &pr.review_comments } else { &pr.comments };
    let c = list
        .iter()
        .find(|c| c.id == created)
        .ok_or_else(|| GhError::Internal("comment vanished".into()))?;
    Ok((
        StatusCode::CREATED,
        Json(comment_json(&ctx, number, c, review)),
    )
        .into_response())
}

/// `GET /repos/{o}/{r}/issues/comments/{id}`.
async fn get_comment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, comment_id)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let number = pr_of_comment(comment_id);
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let c = pr
        .comments
        .iter()
        .chain(pr.review_comments.iter())
        .find(|c| c.id == comment_id)
        .ok_or_else(|| GhError::not_found(format!("comment {comment_id}")))?;
    Ok(Json(comment_json(&ctx, number, c, false)).into_response())
}

/// `PATCH /repos/{o}/{r}/issues/comments/{id}`.
async fn patch_comment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, comment_id)): Path<(String, String, u64)>,
    Json(req): Json<CommentBody>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let number = pr_of_comment(comment_id);
    let pr = pr_store::update(&ctx.store, number, |pr| {
        let found = pr
            .comments
            .iter_mut()
            .chain(pr.review_comments.iter_mut())
            .find(|c| c.id == comment_id);
        let Some(c) = found else {
            return Err(GhError::not_found(format!("comment {comment_id}")));
        };
        c.body.clone_from(&req.body);
        c.updated_at = pr_store::now();
        Ok(())
    })
    .await?;
    let c = pr
        .comments
        .iter()
        .chain(pr.review_comments.iter())
        .find(|c| c.id == comment_id)
        .ok_or_else(|| GhError::not_found(format!("comment {comment_id}")))?;
    Ok(Json(comment_json(&ctx, number, c, false)).into_response())
}

/// `DELETE /repos/{o}/{r}/issues/comments/{id}`.
async fn delete_comment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, comment_id)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let number = pr_of_comment(comment_id);
    pr_store::update(&ctx.store, number, |pr| {
        pr.comments.retain(|c| c.id != comment_id);
        pr.review_comments.retain(|c| c.id != comment_id);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct ReactionBody {
    content: String,
}

/// `POST …/comments/{id}/reactions`. Only the created `id` is ever read back,
/// as the `reaction_id` of the later delete.
async fn create_reaction(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, comment_id)): Path<(String, String, u64)>,
    Json(req): Json<ReactionBody>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let number = pr_of_comment(comment_id);
    let mut created = 0;
    pr_store::update(&ctx.store, number, |pr| {
        let id = pr.take_id();
        created = id;
        let found = pr
            .comments
            .iter_mut()
            .chain(pr.review_comments.iter_mut())
            .find(|c| c.id == comment_id);
        let Some(c) = found else {
            return Err(GhError::not_found(format!("comment {comment_id}")));
        };
        c.reactions.push(Reaction {
            id,
            content: req.content.clone(),
            user: super::auth::USER_LOGIN.to_string(),
            created_at: pr_store::now(),
        });
        Ok(())
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": created,
            "node_id": models::node_id("Reaction", &created.to_string()),
            "content": req.content,
            "user": models::named_user(&ctx.urls, super::auth::USER_LOGIN),
            "created_at": pr_store::now(),
        })),
    )
        .into_response())
}

/// `DELETE …/comments/{id}/reactions/{reaction_id}`.
async fn delete_reaction(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, comment_id, reaction_id)): Path<(String, String, u64, u64)>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let number = pr_of_comment(comment_id);
    pr_store::update(&ctx.store, number, |pr| {
        for c in pr.comments.iter_mut().chain(pr.review_comments.iter_mut()) {
            c.reactions.retain(|r| r.id != reaction_id);
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- issues and search -------------------------------------------------------

/// `GET /repos/{o}/{r}/issues/{n}` — a PR is an issue, and only PRs exist here.
async fn get_issue(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    let ctx = refs_ctx(&st, &headers, &owner, &name).await?;
    let (pr, _) = pr_store::read(&ctx.store, number).await?;
    let head_sha = live_head(&ctx.view, &pr);
    Ok(Json(issue_json(&ctx, &pr, &head_sha)).into_response())
}

#[derive(Deserialize, Default)]
struct SearchQuery {
    q: Option<String>,
    per_page: Option<usize>,
    page: Option<usize>,
}

/// `GET /search/issues?q=repo:o/r is:pr head:branch state:open …`.
///
/// Only the qualifiers the Mintlify server sends are understood; every other
/// word is matched against the title and the body, which is what a client
/// typing free text expects.
async fn search_issues(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> GhResult<Response> {
    let query = q.q.unwrap_or_default();
    let mut repo_spec = None;
    let mut want_state = None;
    let mut want_head = None;
    let mut want_base = None;
    let mut terms: Vec<String> = Vec::new();
    for token in query.split_whitespace() {
        match token.split_once(':') {
            Some(("repo", v)) => repo_spec = Some(v.to_string()),
            Some(("state", v)) => want_state = Some(v.to_string()),
            Some(("is", state @ ("open" | "closed" | "merged"))) => {
                want_state = Some((*state).to_string());
            }
            Some(("head", v)) => want_head = Some(v.to_string()),
            Some(("base", v)) => want_base = Some(v.to_string()),
            // Qualifiers with no meaning against one repository of PRs.
            Some(("is" | "type" | "author" | "sort" | "in" | "label", _)) => {}
            _ => terms.push(token.to_lowercase()),
        }
    }
    let Some((owner, name)) = repo_spec.as_deref().and_then(|s| s.split_once('/')) else {
        return Ok(Json(json!({
            "total_count": 0, "incomplete_results": false, "items": [],
        }))
        .into_response());
    };
    let ctx = refs_ctx(&st, &headers, owner, name).await?;
    let (index, _) = pr_store::read_index(&ctx.store).await?;
    let mut items = Vec::new();
    for row in &index.prs {
        if let Some(s) = want_state.as_deref()
            && s != "all"
            && !(row.state == s || (s == "merged" && row.merged))
        {
            continue;
        }
        if want_head
            .as_deref()
            .is_some_and(|h| row.head_ref != branch_of(h))
        {
            continue;
        }
        if want_base
            .as_deref()
            .is_some_and(|b| row.base_ref != branch_of(b))
        {
            continue;
        }
        let Some(pr) = pr_store::try_read(&ctx.store, row.number).await? else {
            continue;
        };
        let haystack = format!("{} {}", pr.title, pr.body).to_lowercase();
        if !terms.iter().all(|t| haystack.contains(t.as_str())) {
            continue;
        }
        let head_sha = live_head(&ctx.view, &pr);
        items.push(issue_json(&ctx, &pr, &head_sha));
    }
    items.reverse();
    let page = Page::new(q.page, q.per_page);
    let total = items.len();
    let items: Vec<Value> = items
        .into_iter()
        .skip(page.skip())
        .take(page.per_page)
        .collect();
    Ok(Json(json!({
        "total_count": total,
        "incomplete_results": false,
        "items": items,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::{branch_of, pr_of_comment, review_state};

    #[test]
    fn a_head_spec_is_a_branch_with_or_without_an_owner() {
        assert_eq!(branch_of("acme:editor/quickstart"), "editor/quickstart");
        assert_eq!(branch_of("main"), "main");
    }

    #[test]
    fn comment_ids_name_their_pull_request() {
        assert_eq!(pr_of_comment(412_000_003), 412);
        assert_eq!(pr_of_comment(1_000_001), 1);
    }

    #[test]
    fn a_body_less_review_is_pending() {
        assert_eq!(review_state(None), "PENDING");
        assert_eq!(review_state(Some("APPROVE")), "APPROVED");
        assert_eq!(review_state(Some("REQUEST_CHANGES")), "CHANGES_REQUESTED");
        assert_eq!(review_state(Some("COMMENT")), "COMMENTED");
    }
}
