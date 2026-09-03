//! Repository resolution and the milestone-1 read surface.
//!
//! Resolution is walgit's, not a second copy of it: `Registry::open` for the
//! handle, `sync_refs` for anything answerable from the ref snapshot (repo
//! metadata, branches, refs) and `sync_objects` for anything that must read a
//! commit. A repository that this instance can only serve *remotely* (its pack
//! set does not fit) answers object work with 503 — the facade renders through
//! stock `git` against the local copy and has no remote-reader path yet
//! (`docs/GITHUB.md`, "what later phases add").

#![allow(clippy::unused_async)]

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use walgit_git::RepoId;
use walgit_wal::RepoHandle;

use super::error::{GhError, GhResult};
use super::models::{self, CommitFacts, Urls};
use crate::AppState;
use crate::cache::RefIndex;

const DEFAULT_PER_PAGE: usize = 30;
const MAX_PER_PAGE: usize = 100;

/// Open a repository handle. No auth: the facade is its own trust boundary.
pub async fn open(st: &Arc<AppState>, id: &RepoId) -> GhResult<Arc<RepoHandle>> {
    st.registry.open(id).await.map_err(GhError::from)
}

/// `{owner}/{repo}` from the path, with `.git` tolerated the way walgit's own
/// routes tolerate it.
pub fn repo_id(owner: &str, name: &str) -> GhResult<RepoId> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    RepoId::new(owner, name).map_err(|_| GhError::not_found(format!("{owner}/{name}")))
}

/// One synced view of a repository for the duration of a request.
pub struct View {
    pub handle: Arc<RepoHandle>,
    pub local: walgit_git::LocalRepo,
    pub index: Arc<RefIndex>,
    pub full_name: String,
}

async fn build(st: &Arc<AppState>, handle: Arc<RepoHandle>) -> GhResult<View> {
    let local = handle.local().clone();
    let version = handle
        .manifest_version()
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    let full_name = handle.id().to_string();
    let index = st
        .caches
        .ref_index
        .get_or_build(&full_name, &version, || local.refs())
        .map_err(|e| GhError::Internal(e.to_string()))?;
    Ok(View {
        handle,
        local,
        index,
        full_name,
    })
}

/// Refs only: no packs, so it answers on a cold instance in one manifest GET.
pub async fn refs_view(st: &Arc<AppState>, id: &RepoId) -> GhResult<View> {
    let handle = open(st, id).await?;
    let guard = handle.sync_refs().await?;
    let view = build(st, handle.clone()).await;
    drop(guard);
    view
}

/// Refs plus objects on this instance's disk.
pub async fn objects_view(st: &Arc<AppState>, id: &RepoId) -> GhResult<View> {
    let handle = open(st, id).await?;
    let (guard, access) = handle.sync_objects().await?;
    if access.is_remote() {
        drop(guard);
        return Err(GhError::Unavailable(format!(
            "{id} is served remotely on this instance; the github facade needs its packs locally"
        )));
    }
    let view = build(st, handle.clone()).await;
    drop(guard);
    view
}

/// Current oid of a full ref name, straight from the local ref store.
pub fn ref_oid(local: &walgit_git::LocalRepo, name: &str) -> GhResult<Option<String>> {
    let snap = local
        .refs()
        .map_err(|e| GhError::Internal(format!("read refs: {e}")))?;
    Ok(snap
        .refs
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.oid.clone()))
}

/// The default branch, from HEAD. An unborn HEAD still names its target.
fn default_branch(index: &RefIndex) -> String {
    index
        .head_target
        .strip_prefix("refs/heads/")
        .unwrap_or("main")
        .to_string()
}

fn pushed_at(handle: &RepoHandle) -> String {
    let m = handle.manifest();
    let Some(ts) = m.updated_at.as_ref() else {
        return models::EPOCH.to_string();
    };
    chrono::DateTime::<chrono::Utc>::from(walgit_proto::time::to_system(ts))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---- git plumbing ------------------------------------------------------------

/// One record per commit, `\x1e`-separated, fields `\0`-separated. The message
/// (`%B`) is last so it may contain anything but a NUL.
pub(super) const LOG_FORMAT: &str =
    "%x1e%H%x00%T%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";

/// Parse the output of a `git log`/`git show` run with [`LOG_FORMAT`].
pub(super) fn parse_commits(bytes: &[u8]) -> Vec<CommitFacts> {
    String::from_utf8_lossy(bytes)
        .split('\x1e')
        .filter_map(parse_commit)
        .collect()
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

/// Run `git` in the serving copy. A non-zero exit is a 404: every caller is
/// naming an object or a revision the request supplied.
pub(super) async fn git(local: &walgit_git::LocalRepo, args: &[&str]) -> GhResult<Vec<u8>> {
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

/// GitHub's `{ref}` for `commits/{ref}`: a branch, a tag or a commit sha.
pub async fn resolve_commitish(view: &View, r: &str) -> GhResult<String> {
    if let Some(sha) = view.index.branch(r) {
        return Ok(sha.to_string());
    }
    if let Some(sha) = view.index.tag(r) {
        return Ok(sha.to_string());
    }
    if let Some((oid, peeled)) = view.index.by_name.get(r) {
        return Ok(if peeled.is_empty() {
            oid.clone()
        } else {
            peeled.clone()
        });
    }
    if r.is_empty() || r.starts_with('-') {
        return Err(GhError::not_found(r));
    }
    let out = git(
        &view.local,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{r}^{{commit}}"),
        ],
    )
    .await
    .map_err(|_| GhError::not_found(r))?;
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    if sha.is_empty() {
        return Err(GhError::not_found(r));
    }
    Ok(sha)
}

/// One commit object, parsed.
pub(super) async fn commit_facts(
    local: &walgit_git::LocalRepo,
    sha: &str,
) -> GhResult<CommitFacts> {
    let out = git(
        local,
        &[
            "show",
            "-s",
            "--diff-merges=off",
            &format!("--format={LOG_FORMAT}"),
            "--end-of-options",
            sha,
        ],
    )
    .await?;
    parse_commits(&out)
        .into_iter()
        .next()
        .ok_or_else(|| GhError::not_found(sha))
}

/// `git merge-base a b`. `None` when the histories are unrelated.
pub(super) async fn merge_base(
    local: &walgit_git::LocalRepo,
    a: &str,
    b: &str,
) -> GhResult<Option<String>> {
    let Ok(out) = git(local, &["merge-base", a, b]).await else {
        return Ok(None);
    };
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    Ok((!sha.is_empty()).then_some(sha))
}

/// `git rev-list --count base..head` — GitHub's "commits between".
pub(super) async fn commit_count(
    local: &walgit_git::LocalRepo,
    base: &str,
    head: &str,
) -> GhResult<u64> {
    let range = format!("{base}..{head}");
    let out = git(local, &["rev-list", "--count", "--end-of-options", &range]).await?;
    Ok(String::from_utf8_lossy(&out).trim().parse().unwrap_or(0))
}

/// The shas of `base..head`, oldest first.
pub(super) async fn commits_between(
    local: &walgit_git::LocalRepo,
    base: &str,
    head: &str,
) -> GhResult<Vec<String>> {
    let range = format!("{base}..{head}");
    let out = git(
        local,
        &["rev-list", "--reverse", "--end-of-options", &range],
    )
    .await?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

// ---- pagination --------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
pub struct PageQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub sha: Option<String>,
    pub path: Option<String>,
}

struct Page {
    page: usize,
    per_page: usize,
}

impl Page {
    fn from(q: &PageQuery) -> Self {
        Self {
            page: q.page.unwrap_or(1).max(1),
            per_page: q.per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
        }
    }
    fn skip(&self) -> usize {
        (self.page - 1).saturating_mul(self.per_page)
    }
}

/// GitHub's `Link` header. Only `next`/`prev`/`first` are emitted: a total
/// count would mean walking the whole history, and no client reads `last`
/// off a commit listing.
fn link_header(urls: &Urls, path: &str, query: &str, page: &Page, more: bool) -> Option<String> {
    let mut parts = Vec::new();
    let base = format!("{}{path}", urls.api);
    let sep = if query.is_empty() { "" } else { "&" };
    if more {
        parts.push(format!(
            "<{base}?{query}{sep}page={}&per_page={}>; rel=\"next\"",
            page.page + 1,
            page.per_page
        ));
    }
    if page.page > 1 {
        parts.push(format!(
            "<{base}?{query}{sep}page={}&per_page={}>; rel=\"prev\"",
            page.page - 1,
            page.per_page
        ));
        parts.push(format!(
            "<{base}?{query}{sep}page=1&per_page={}>; rel=\"first\"",
            page.per_page
        ));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn with_link(mut resp: Response, link: Option<String>) -> Response {
    if let Some(l) = link
        && let Ok(v) = axum::http::HeaderValue::from_str(&l)
    {
        resp.headers_mut().insert(axum::http::header::LINK, v);
    }
    resp
}

// ---- handlers ----------------------------------------------------------------

/// `GET /api/v3/repos/{owner}/{repo}`.
pub async fn get_repo(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = refs_view(&st, &id).await?;
    let urls = Urls::from_request(&st, &headers);
    Ok(axum::Json(models::repository(
        &urls,
        id.owner(),
        id.name(),
        &default_branch(&view.index),
        &pushed_at(&view.handle),
    ))
    .into_response())
}

/// `GET /api/v3/installation/repositories` — every repository in the bucket.
pub async fn installation_repositories(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let page = Page::from(&q);
    let mut ids = st
        .registry
        .list()
        .await
        .map_err(|e| GhError::Internal(e.to_string()))?;
    ids.sort_by_key(ToString::to_string);
    let total = ids.len();
    let mut repos = Vec::new();
    for id in ids.iter().skip(page.skip()).take(page.per_page) {
        let (branch, pushed) = match refs_view(&st, id).await {
            Ok(v) => (default_branch(&v.index), pushed_at(&v.handle)),
            Err(_) => ("main".to_string(), models::EPOCH.to_string()),
        };
        repos.push(models::repository(
            &urls,
            id.owner(),
            id.name(),
            &branch,
            &pushed,
        ));
    }
    let more = page.skip() + repos.len() < total;
    let body = axum::Json(serde_json::json!({
        "total_count": total,
        "repository_selection": "all",
        "repositories": repos,
    }))
    .into_response();
    Ok(with_link(
        body,
        link_header(&urls, "/installation/repositories", "", &page, more),
    ))
}

/// `GET /api/v3/repos/{o}/{r}/commits/{ref}` — branch, tag or sha.
pub async fn get_commit(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = objects_view(&st, &id).await?;
    let sha = resolve_commitish(&view, &r).await?;
    let facts = commit_facts(&view.local, &sha).await?;
    let urls = Urls::from_request(&st, &headers);
    Ok(axum::Json(models::commit(&urls, &view.full_name, &facts)).into_response())
}

/// `GET /api/v3/repos/{o}/{r}/git/commits/{sha}` — the git-data shape.
pub async fn get_git_commit(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = objects_view(&st, &id).await?;
    let sha = resolve_commitish(&view, &sha).await?;
    let facts = commit_facts(&view.local, &sha).await?;
    let urls = Urls::from_request(&st, &headers);
    Ok(axum::Json(models::git_commit(&urls, &view.full_name, &facts)).into_response())
}

/// `GET /api/v3/repos/{o}/{r}/commits?sha=&path=&per_page=&page=`.
pub async fn list_commits(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = objects_view(&st, &id).await?;
    let start = if let Some(r) = q.sha.as_deref().filter(|s| !s.is_empty()) {
        resolve_commitish(&view, r).await?
    } else {
        let (_, sha) = view
            .index
            .head()
            .ok_or_else(|| GhError::not_found("Git Repository is empty."))?;
        sha
    };
    let page = Page::from(&q);
    let mut args: Vec<String> = vec![
        "log".into(),
        format!("--format={LOG_FORMAT}"),
        "--no-color".into(),
        format!("--skip={}", page.skip()),
        format!("-{}", page.per_page.saturating_add(1)),
        start,
    ];
    if let Some(p) = q.path.as_deref().filter(|p| !p.is_empty()) {
        args.push("--".into());
        args.push(p.to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut facts = parse_commits(&git(&view.local, &refs).await?);
    let more = facts.len() > page.per_page;
    facts.truncate(page.per_page);
    let urls = Urls::from_request(&st, &headers);
    let out: Vec<_> = facts
        .iter()
        .map(|c| models::commit(&urls, &view.full_name, c))
        .collect();
    let query = q.sha.as_deref().map_or_else(String::new, |s| format!("sha={s}"));
    let link = link_header(
        &urls,
        &format!("/repos/{}/commits", view.full_name),
        &query,
        &page,
        more,
    );
    Ok(with_link(axum::Json(out).into_response(), link))
}

/// `GET /api/v3/repos/{o}/{r}/branches/{branch}`.
pub async fn get_branch(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = objects_view(&st, &id).await?;
    let sha = view
        .index
        .branch(&branch)
        .ok_or_else(|| GhError::not_found(&branch))?
        .to_string();
    let facts = commit_facts(&view.local, &sha).await?;
    let urls = Urls::from_request(&st, &headers);
    Ok(axum::Json(models::branch(&urls, &view.full_name, &branch, &facts)).into_response())
}

/// `GET /api/v3/repos/{o}/{r}/branches`. The commit body of each entry is the
/// sha alone (GitHub's list shape), so this stays refs-level.
pub async fn list_branches(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = refs_view(&st, &id).await?;
    let urls = Urls::from_request(&st, &headers);
    let page = Page::from(&q);
    let total = view.index.branches.len();
    let out: Vec<_> = view
        .index
        .branches
        .iter()
        .skip(page.skip())
        .take(page.per_page)
        .map(|(n, sha)| {
            serde_json::json!({
                "name": n,
                "commit": {
                    "sha": sha,
                    "url": format!("{}/repos/{}/commits/{sha}", urls.api, view.full_name),
                },
                "protected": false,
            })
        })
        .collect();
    let more = page.skip() + out.len() < total;
    let link = link_header(
        &urls,
        &format!("/repos/{}/branches", view.full_name),
        "",
        &page,
        more,
    );
    Ok(with_link(axum::Json(out).into_response(), link))
}

/// The `type` of a ref's object: `tag` for an annotated tag (the index records
/// a peeled commit for exactly those), `commit` otherwise.
fn object_type(index: &RefIndex, full_ref: &str) -> &'static str {
    match index.by_name.get(full_ref) {
        Some((_, peeled)) if !peeled.is_empty() => "tag",
        _ => "commit",
    }
}

/// `GET /api/v3/repos/{o}/{r}/git/ref/{*ref}` — one ref, 404 when absent.
/// `{ref}` is the name without the `refs/` prefix (`heads/main`).
pub async fn get_ref(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo_id(&owner, &name)?;
    let view = refs_view(&st, &id).await?;
    let full = format!("refs/{}", r.trim_start_matches('/'));
    let (oid, _) = view
        .index
        .by_name
        .get(&full)
        .ok_or_else(|| GhError::not_found(&full))?;
    let urls = Urls::from_request(&st, &headers);
    Ok(axum::Json(models::git_ref(
        &urls,
        &view.full_name,
        &full,
        oid,
        object_type(&view.index, &full),
    ))
    .into_response())
}

/// `GET /api/v3/repos/{o}/{r}/git/refs[/{*ref}]` and `.../git/matching-refs/…`
/// — every ref whose name starts with `refs/{ref}`.
pub async fn matching_refs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> GhResult<Response> {
    matching(&st, &headers, &owner, &name, &r).await
}

/// `GET /api/v3/repos/{o}/{r}/git/refs` — all of them.
pub async fn all_refs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> GhResult<Response> {
    matching(&st, &headers, &owner, &name, "").await
}

async fn matching(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    r: &str,
) -> GhResult<Response> {
    let id = repo_id(owner, name)?;
    let view = refs_view(st, &id).await?;
    let prefix = format!("refs/{}", r.trim_start_matches('/'));
    let urls = Urls::from_request(st, headers);
    let mut names: Vec<&String> = view
        .index
        .by_name
        .keys()
        .filter(|n| n.starts_with(prefix.as_str()))
        .collect();
    names.sort();
    let out: Vec<_> = names
        .into_iter()
        .filter_map(|n| {
            let (oid, _) = view.index.by_name.get(n)?;
            Some(models::git_ref(
                &urls,
                &view.full_name,
                n,
                oid,
                object_type(&view.index, n),
            ))
        })
        .collect();
    Ok(axum::Json(out).into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn commit_record_parses_a_message_with_blank_lines() {
        let rec = "abc\x00tree1\x00p1 p2\x00A\x00a@x\x002024-01-01T00:00:00+00:00\x00C\x00c@x\x002024-01-02T00:00:00+00:00\x00subject\n\nbody\n\n";
        let c = super::parse_commit(rec).expect("record");
        assert_eq!(c.sha, "abc");
        assert_eq!(c.parents, vec!["p1", "p2"]);
        assert_eq!(c.message, "subject\n\nbody");
        assert_eq!(c.committer_email, "c@x");
    }

    #[test]
    fn empty_records_are_skipped() {
        assert!(super::parse_commits(b"").is_empty());
    }
}
