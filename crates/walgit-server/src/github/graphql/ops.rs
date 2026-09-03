//! The query side: `repository`, `repositoryOwner` and `search`.
//!
//! Every arm answers the **full** documented shape of its node, not only the
//! fields the document selected: the client's documents are literals that get
//! edited, and a response that only carries today's selection breaks on the
//! next edit for no reason. Selection still decides *work* — `history` and
//! `blame` are only computed when they are asked for.

use std::sync::Arc;

use serde_json::{Value, json};
use walgit_git::RepoId;

use super::error::GqlError;
use super::parse::Field;
use crate::AppState;
use crate::github::models::{CommitFacts, Urls, node_id};
use crate::github::repo::{self, View};

/// The most commits one `history` selection will walk. GitHub's own page cap
/// is 100 and the only call site in the contract asks for one.
const MAX_HISTORY: usize = 100;
/// The most repositories one page of `repositories`/`search` answers.
const MAX_REPOS: usize = 100;
/// A blob above this is returned with `text: null`, the way GitHub truncates.
const MAX_BLOB_TEXT: u64 = 4 << 20;

/// One request's server state and origins.
pub struct Ctx {
    pub st: Arc<AppState>,
    pub urls: Urls,
}

impl Ctx {
    /// GitHub's own wording for a repository that does not resolve — the
    /// client greps the message for it (`docs/GITHUB_SHAPES.md`).
    pub fn repo_id(&self, owner: &str, name: &str) -> Result<RepoId, GqlError> {
        RepoId::new(owner, name).map_err(|_| {
            GqlError::not_found(format!(
                "Could not resolve to a Repository with the name '{owner}/{name}'."
            ))
        })
    }

    pub async fn objects(&self, id: &RepoId) -> Result<View, GqlError> {
        repo::objects_view(&self.st, id).await.map_err(|e| {
            not_found_as_repository(e, id)
        })
    }

    pub async fn refs(&self, id: &RepoId) -> Result<View, GqlError> {
        repo::refs_view(&self.st, id)
            .await
            .map_err(|e| not_found_as_repository(e, id))
    }
}

fn not_found_as_repository(e: crate::github::error::GhError, id: &RepoId) -> GqlError {
    match e {
        crate::github::error::GhError::NotFound(_) => GqlError::not_found(format!(
            "Could not resolve to a Repository with the name '{id}'."
        )),
        other => GqlError::from(other),
    }
}

// ---- git ---------------------------------------------------------------------

/// `\x1e`-separated records, `\0`-separated fields, message last.
const LOG_FORMAT: &str =
    "%x1e%H%x00%T%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";

pub async fn git(view: &View, args: &[&str]) -> Result<Vec<u8>, GqlError> {
    let out = view
        .local
        .git(args)
        .await
        .map_err(|e| GqlError::internal(format!("git: {e}")))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(GqlError::not_found(
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

fn parse_commits(bytes: &[u8]) -> Vec<CommitFacts> {
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

pub async fn commit_facts(view: &View, sha: &str) -> Result<CommitFacts, GqlError> {
    let out = git(
        view,
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
        .ok_or_else(|| GqlError::not_found(format!("Could not resolve to a Commit with {sha}.")))
}

// ---- nodes -------------------------------------------------------------------

fn abbreviated(oid: &str) -> String {
    oid.chars().take(7).collect()
}

fn headline_and_body(message: &str) -> (String, String) {
    let mut lines = message.splitn(2, '\n');
    let headline = lines.next().unwrap_or("").trim_end().to_string();
    let body = lines.next().unwrap_or("").trim_start_matches('\n').to_string();
    (headline, body)
}

fn actor(urls: &Urls, name: &str, email: &str) -> Value {
    let login = email
        .split('@')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    json!({
        "login": login,
        "email": email,
        "url": format!("{}/{login}", urls.html),
    })
}

fn signature(urls: &Urls, name: &str, email: &str, date: &str) -> Value {
    json!({
        "name": name,
        "email": email,
        "date": date,
        "user": actor(urls, name, email),
    })
}

/// A GraphQL `Commit`. `url` is the browser URL: the client stores it as the
/// commit's `link` (`docs/GITHUB_SHAPES.md`, `createCommitOnBranch`).
pub fn commit_node(urls: &Urls, full_name: &str, c: &CommitFacts) -> Value {
    let (headline, body) = headline_and_body(&c.message);
    let url = format!("{}/{full_name}/commit/{}", urls.html, c.sha);
    json!({
        "id": node_id("Commit", &format!("{full_name}:{}", c.sha)),
        "oid": c.sha,
        "abbreviatedOid": abbreviated(&c.sha),
        "url": url,
        "commitUrl": url,
        "message": c.message,
        "messageHeadline": headline,
        "messageBody": body,
        "authoredDate": c.author_date,
        "committedDate": c.committer_date,
        "author": signature(urls, &c.author_name, &c.author_email, &c.author_date),
        "committer": signature(urls, &c.committer_name, &c.committer_email, &c.committer_date),
        "tree": { "oid": c.tree },
        "parents": {
            "totalCount": c.parents.len(),
            "nodes": c.parents.iter().map(|p| json!({ "oid": p })).collect::<Vec<_>>(),
        },
    })
}

/// A GraphQL `Repository`, as much of it as the bucket knows.
pub fn repository_node(
    urls: &Urls,
    id: &RepoId,
    default_branch: &str,
    pushed_at: &str,
) -> Value {
    let full = id.to_string();
    json!({
        "id": node_id("Repository", &full),
        "name": id.name(),
        "nameWithOwner": full,
        "url": format!("{}/{full}", urls.html),
        "description": Value::Null,
        "isFork": false,
        "isPrivate": true,
        "isArchived": false,
        "isEmpty": false,
        "owner": { "login": id.owner(), "url": format!("{}/{}", urls.html, id.owner()) },
        "defaultBranchRef": { "name": default_branch, "prefix": "refs/heads/" },
        "pushedAt": pushed_at,
        "updatedAt": pushed_at,
    })
}

// ---- cursors -----------------------------------------------------------------

/// An opaque cursor is an offset. GitHub's are opaque too and no client in the
/// contract does anything with one but hand it back.
fn decode_cursor(after: Option<&str>) -> usize {
    use base64::Engine;
    after
        .and_then(|c| base64::engine::general_purpose::STANDARD.decode(c).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.strip_prefix("offset:").and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

fn encode_cursor(offset: usize) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("offset:{offset}"))
}

fn page_info(next: usize, total: usize, start: usize) -> Value {
    json!({
        "hasNextPage": next < total,
        "hasPreviousPage": start > 0,
        "endCursor": if next > start { Value::from(encode_cursor(next)) } else { Value::Null },
        "startCursor": if next > start { Value::from(encode_cursor(start)) } else { Value::Null },
    })
}

// ---- query dispatch ----------------------------------------------------------

pub async fn query(ctx: &Ctx, label: &str, fields: &[Field]) -> Result<Value, GqlError> {
    let mut data = serde_json::Map::new();
    for f in fields {
        let value = match f.name.as_str() {
            "repository" => repository(ctx, f, label).await?,
            "repositoryOwner" => repository_owner(ctx, f).await?,
            "search" => search(ctx, f).await?,
            other => return Err(GqlError::not_implemented(format!("{label}.{other}"))),
        };
        data.insert(f.name.clone(), value);
    }
    Ok(Value::Object(data))
}

/// `repository(owner:, name:)`. A missing repository is `NOT_FOUND` and not a
/// null: `getFileShaByPath` swallows every error to `null` anyway, and
/// `createCommitOnBranch`'s error branch wants the type.
async fn repository(ctx: &Ctx, f: &Field, label: &str) -> Result<Value, GqlError> {
    let owner = f
        .str_arg("owner")
        .ok_or_else(|| GqlError::bad_request("repository(owner:) is required"))?;
    let name = f
        .str_arg("name")
        .ok_or_else(|| GqlError::bad_request("repository(name:) is required"))?;
    let id = ctx.repo_id(owner, name)?;
    // Refuse an unserved field before touching the bucket: an unimplemented
    // selection is a gap in the facade, not a missing repository, and the
    // message must say so whichever repository it was asked about.
    for child in &f.children {
        if !matches!(
            child.name.as_str(),
            "ref"
                | "object"
                | "refs"
                | "id"
                | "name"
                | "nameWithOwner"
                | "url"
                | "description"
                | "isFork"
                | "isPrivate"
                | "isArchived"
                | "isEmpty"
                | "owner"
                | "defaultBranchRef"
                | "pushedAt"
                | "updatedAt"
        ) {
            return Err(GqlError::not_implemented(format!(
                "{label}.repository.{}",
                child.name
            )));
        }
    }
    let needs_objects = f.has("ref") || f.has("object");
    let view = if needs_objects {
        ctx.objects(&id).await?
    } else {
        ctx.refs(&id).await?
    };
    let default_branch = view
        .index
        .head_target
        .strip_prefix("refs/heads/")
        .unwrap_or("main")
        .to_string();
    let mut node = repository_node(&ctx.urls, &id, &default_branch, &pushed_at(&view));
    for child in &f.children {
        let value = match child.name.as_str() {
            "ref" => ref_field(ctx, &view, child).await?,
            "object" => object_field(ctx, &view, child).await?,
            "refs" => refs_field(&view, child),
            _ => continue,
        };
        if let Some(obj) = node.as_object_mut() {
            obj.insert(child.name.clone(), value);
        }
    }
    Ok(node)
}

fn pushed_at(view: &View) -> String {
    view.handle.manifest().updated_at.as_ref().map_or_else(
        || crate::github::models::EPOCH.to_string(),
        |ts| {
            chrono::DateTime::<chrono::Utc>::from(walgit_proto::time::to_system(ts))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        },
    )
}

/// `ref(qualifiedName:)`. A branch name, a tag name or a full `refs/…`; a ref
/// that is not there is `null`, which every call site in the contract handles.
async fn ref_field(ctx: &Ctx, view: &View, f: &Field) -> Result<Value, GqlError> {
    let qualified = f
        .str_arg("qualifiedName")
        .ok_or_else(|| GqlError::bad_request("ref(qualifiedName:) is required"))?;
    let Some((full, oid)) = resolve_ref(view, qualified) else {
        return Ok(Value::Null);
    };
    let (prefix, short) = split_ref(&full);
    let facts = commit_facts(view, &oid).await?;
    let mut target = commit_node(&ctx.urls, &view.full_name, &facts);
    if let Some(t) = f.child("target") {
        for child in &t.children {
            let value = match child.name.as_str() {
                "history" => history(ctx, view, child, &oid).await?,
                "blame" => super::blame::blame(ctx, view, child, &oid).await?,
                _ => continue,
            };
            if let Some(obj) = target.as_object_mut() {
                obj.insert(child.name.clone(), value);
            }
        }
    }
    Ok(json!({
        "id": node_id("Ref", &format!("{}:{full}", view.full_name)),
        "name": short,
        "prefix": prefix,
        "target": target,
    }))
}

/// `qualifiedName` accepts what GitHub accepts: `main`, `refs/heads/main`,
/// `v1` (a tag), or a full ref of any namespace.
fn resolve_ref(view: &View, qualified: &str) -> Option<(String, String)> {
    if qualified.starts_with("refs/")
        && let Some((oid, _)) = view.index.by_name.get(qualified)
    {
        return Some((qualified.to_string(), oid.clone()));
    }
    if let Some(oid) = view.index.branch(qualified) {
        return Some((format!("refs/heads/{qualified}"), oid.to_string()));
    }
    if let Some(oid) = view.index.tag(qualified) {
        return Some((format!("refs/tags/{qualified}"), oid.to_string()));
    }
    None
}

fn split_ref(full: &str) -> (String, String) {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(short) = full.strip_prefix(prefix) {
            return (prefix.to_string(), short.to_string());
        }
    }
    (String::new(), full.to_string())
}

/// `history(first:)` on a commit — `git log` from the ref's tip.
async fn history(ctx: &Ctx, view: &View, f: &Field, tip: &str) -> Result<Value, GqlError> {
    let first = f.usize_arg("first").unwrap_or(1).clamp(1, MAX_HISTORY);
    let skip = decode_cursor(f.str_arg("after"));
    let mut args = vec![
        "log".to_string(),
        format!("--format={LOG_FORMAT}"),
        "--no-color".to_string(),
        format!("--skip={skip}"),
        format!("-{}", first.saturating_add(1)),
        "--end-of-options".to_string(),
        tip.to_string(),
    ];
    if let Some(path) = f.str_arg("path") {
        args.push("--".to_string());
        args.push(path.to_string());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut facts = parse_commits(&git(view, &argv).await?);
    let more = facts.len() > first;
    facts.truncate(first);
    let next = skip + facts.len();
    Ok(json!({
        "totalCount": next,
        "pageInfo": {
            "hasNextPage": more,
            "hasPreviousPage": skip > 0,
            "endCursor": encode_cursor(next),
            "startCursor": encode_cursor(skip),
        },
        "nodes": facts
            .iter()
            .map(|c| commit_node(&ctx.urls, &view.full_name, c))
            .collect::<Vec<_>>(),
    }))
}

/// `object(expression: "<rev>:<path>")` — the file-sha query. Anything that
/// does not resolve is `null`, never an error: the call site swallows errors
/// to `null` and a miss must not look like an outage.
async fn object_field(ctx: &Ctx, view: &View, f: &Field) -> Result<Value, GqlError> {
    let Some(expression) = f.str_arg("expression").filter(|e| !e.starts_with('-')) else {
        return Ok(Value::Null);
    };
    let Ok(out) = git(
        view,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", expression],
    )
    .await
    else {
        return Ok(Value::Null);
    };
    let oid = String::from_utf8_lossy(&out).trim().to_string();
    if oid.is_empty() {
        return Ok(Value::Null);
    }
    let kind = String::from_utf8_lossy(&git(view, &["cat-file", "-t", &oid]).await?)
        .trim()
        .to_string();
    match kind.as_str() {
        "blob" => blob(view, &oid).await,
        "tree" => Ok(json!({ "__typename": "Tree", "id": node_id("Tree", &oid), "oid": oid })),
        "commit" => {
            let facts = commit_facts(view, &oid).await?;
            let mut node = commit_node(&ctx.urls, &view.full_name, &facts);
            if let Some(obj) = node.as_object_mut() {
                obj.insert("__typename".to_string(), Value::from("Commit"));
            }
            Ok(node)
        }
        _ => Ok(Value::Null),
    }
}

async fn blob(view: &View, oid: &str) -> Result<Value, GqlError> {
    let size: u64 = String::from_utf8_lossy(&git(view, &["cat-file", "-s", oid]).await?)
        .trim()
        .parse()
        .unwrap_or(0);
    let mut is_binary = false;
    let mut text = Value::Null;
    if size <= MAX_BLOB_TEXT {
        let bytes = git(view, &["cat-file", "blob", oid]).await?;
        is_binary = bytes.iter().take(8000).any(|b| *b == 0);
        if !is_binary && let Ok(s) = String::from_utf8(bytes) {
            text = Value::from(s);
        }
    }
    Ok(json!({
        "__typename": "Blob",
        "id": node_id("Blob", oid),
        "oid": oid,
        "abbreviatedOid": abbreviated(oid),
        "byteSize": size,
        "isBinary": is_binary,
        "isTruncated": size > MAX_BLOB_TEXT,
        "text": text,
    }))
}

/// `refs(refPrefix:, first:, after:, query:)` — the branch listing.
fn refs_field(view: &View, f: &Field) -> Value {
    let prefix = f.str_arg("refPrefix").unwrap_or("refs/heads/");
    let filter = f.str_arg("query").unwrap_or("").to_lowercase();
    let first = f.usize_arg("first").unwrap_or(100).clamp(1, MAX_REPOS);
    let start = decode_cursor(f.str_arg("after"));
    let mut names: Vec<&String> = view
        .index
        .by_name
        .keys()
        .filter(|n| n.starts_with(prefix))
        .collect();
    names.sort();
    let matched: Vec<&&String> = names
        .iter()
        .filter(|n| {
            filter.is_empty()
                || n.strip_prefix(prefix)
                    .unwrap_or(n)
                    .to_lowercase()
                    .contains(&filter)
        })
        .collect();
    let total = matched.len();
    let nodes: Vec<Value> = matched
        .iter()
        .skip(start)
        .take(first)
        .filter_map(|n| {
            let (oid, _) = view.index.by_name.get(**n)?;
            let (p, short) = split_ref(n);
            Some(json!({
                "id": node_id("Ref", &format!("{}:{n}", view.full_name)),
                "name": short,
                "prefix": p,
                "target": { "oid": oid },
            }))
        })
        .collect();
    let next = start + nodes.len();
    json!({
        "totalCount": total,
        "pageInfo": page_info(next, total, start),
        "nodes": nodes,
    })
}

// ---- owners and search -------------------------------------------------------

/// Every repository of one owner, newest push first when that is the order
/// asked for. The bucket listing is `Registry::list`, the same one
/// `installation/repositories` pages; a facade's bucket is a developer's, so
/// resolving the whole owner before paging is cheap and keeps the order true.
async fn repositories_of(ctx: &Ctx, owner: &str) -> Result<Vec<(RepoId, String, String)>, GqlError> {
    let ids = ctx
        .st
        .registry
        .list()
        .await
        .map_err(|e| GqlError::internal(e.to_string()))?;
    let mut out = Vec::new();
    for id in ids.into_iter().filter(|i| i.owner() == owner) {
        let (branch, pushed) = match ctx.refs(&id).await {
            Ok(v) => (
                v.index
                    .head_target
                    .strip_prefix("refs/heads/")
                    .unwrap_or("main")
                    .to_string(),
                pushed_at(&v),
            ),
            Err(_) => (
                "main".to_string(),
                crate::github::models::EPOCH.to_string(),
            ),
        };
        out.push((id, branch, pushed));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `repositoryOwner(login:)`. An owner with nothing in the bucket is `null`,
/// which is how `getRepos` ends its pagination cleanly.
async fn repository_owner(ctx: &Ctx, f: &Field) -> Result<Value, GqlError> {
    let login = f
        .str_arg("login")
        .ok_or_else(|| GqlError::bad_request("repositoryOwner(login:) is required"))?;
    let mut repos = repositories_of(ctx, login).await?;
    if repos.is_empty() {
        return Ok(Value::Null);
    }
    let mut node = json!({
        "id": node_id("User", login),
        "login": login,
        "url": format!("{}/{login}", ctx.urls.html),
        "__typename": "User",
    });
    if let Some(child) = f.child("repositories") {
        if child
            .arg("orderBy")
            .and_then(|o| o.get("field"))
            .and_then(Value::as_str)
            == Some("PUSHED_AT")
        {
            repos.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        }
        let first = child.usize_arg("first").unwrap_or(100).clamp(1, MAX_REPOS);
        let start = decode_cursor(child.str_arg("after"));
        let total = repos.len();
        let nodes: Vec<Value> = repos
            .iter()
            .skip(start)
            .take(first)
            .map(|(id, branch, pushed)| repository_node(&ctx.urls, id, branch, pushed))
            .collect();
        let next = start + nodes.len();
        if let Some(obj) = node.as_object_mut() {
            obj.insert(
                "repositories".to_string(),
                json!({
                    "totalCount": total,
                    "pageInfo": page_info(next, total, start),
                    "nodes": nodes,
                }),
            );
        }
    }
    Ok(node)
}

/// `search(query:, type: REPOSITORY, first:)`. The query is GitHub's search
/// syntax; the facade reads the qualifiers the client actually sends —
/// `user:`/`org:`/`owner:` picks the owner, `in:name` and `fork:` are
/// accepted and ignored (every repository here is a non-fork) — and matches
/// the free terms against the name, case-insensitively.
async fn search(ctx: &Ctx, f: &Field) -> Result<Value, GqlError> {
    if let Some(kind) = f.arg("type").and_then(Value::as_str)
        && kind != "REPOSITORY"
    {
        return Err(GqlError::not_implemented(format!("search(type: {kind})")));
    }
    let q = f.str_arg("query").unwrap_or_default();
    let mut owner = None;
    let mut terms = Vec::new();
    for token in q.split_whitespace() {
        match token.split_once(':') {
            Some(("user" | "org" | "owner", who)) => owner = Some(who.to_string()),
            Some(("in" | "fork" | "is" | "archived" | "sort", _)) => {}
            _ => terms.push(token.to_lowercase()),
        }
    }
    let first = f.usize_arg("first").unwrap_or(50).clamp(1, MAX_REPOS);
    let candidates = if let Some(o) = &owner {
        repositories_of(ctx, o).await?
    } else {
        let ids = ctx
            .st
            .registry
            .list()
            .await
            .map_err(|e| GqlError::internal(e.to_string()))?;
        ids.into_iter()
            .map(|id| {
                (
                    id,
                    "main".to_string(),
                    crate::github::models::EPOCH.to_string(),
                )
            })
            .collect()
    };
    let matched: Vec<&(RepoId, String, String)> = candidates
        .iter()
        .filter(|(id, _, _)| {
            let name = id.name().to_lowercase();
            terms.iter().all(|t| name.contains(t.as_str()))
        })
        .collect();
    let total = matched.len();
    let nodes: Vec<Value> = matched
        .iter()
        .take(first)
        .map(|(id, branch, pushed)| repository_node(&ctx.urls, id, branch, pushed))
        .collect();
    let next = nodes.len();
    Ok(json!({
        "repositoryCount": total,
        "pageInfo": page_info(next, total, 0),
        "nodes": nodes,
    }))
}

#[cfg(test)]
mod tests {
    use super::{abbreviated, decode_cursor, encode_cursor, headline_and_body, split_ref};

    #[test]
    fn a_message_splits_into_headline_and_body() {
        let (h, b) = headline_and_body("subject\n\nbody line\n");
        assert_eq!(h, "subject");
        assert_eq!(b, "body line\n");
        let (h, b) = headline_and_body("only a subject");
        assert_eq!(h, "only a subject");
        assert!(b.is_empty());
    }

    #[test]
    fn refs_split_into_prefix_and_short_name() {
        assert_eq!(
            split_ref("refs/heads/feature/x"),
            ("refs/heads/".to_string(), "feature/x".to_string())
        );
        assert_eq!(
            split_ref("refs/pull/1/head"),
            (String::new(), "refs/pull/1/head".to_string())
        );
    }

    #[test]
    fn cursors_round_trip_and_a_junk_cursor_is_the_start() {
        assert_eq!(decode_cursor(Some(&encode_cursor(37))), 37);
        assert_eq!(decode_cursor(Some("nonsense")), 0);
        assert_eq!(decode_cursor(None), 0);
    }

    #[test]
    fn oids_abbreviate_without_slicing_a_short_one() {
        assert_eq!(abbreviated("0123456789abcdef"), "0123456");
        assert_eq!(abbreviated("abc"), "abc");
    }
}
