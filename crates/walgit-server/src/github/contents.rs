//! `GET /repos/{o}/{r}/contents/{path}` — the hottest read the facade serves
//! (1.58M/7d in `docs/GITHUB_SHAPES.md`), and the one with the most
//! representations.
//!
//! One route answers four shapes, chosen by the path's object type and the
//! `Accept` header:
//!
//! | Target | `Accept` | Body |
//! |---|---|---|
//! | file | default | an object with base64 `content` |
//! | file | `…raw` | the bytes, `Content-Type: application/vnd.github.raw` |
//! | directory | default | an **array** of entries |
//! | directory | `…object+json` | an object with `entries` |
//!
//! The array/object split is not cosmetic: `getFileBufferByPath` branches on
//! `Array.isArray(data)` and `getContentDirectorySha` refuses a non-array.

#![allow(clippy::unused_async)]

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use super::error::{GhError, GhResult};
use super::models::Urls;
use super::reads::{self, Entry};
use super::repo::{self, View};
use crate::AppState;

/// GitHub stops embedding `content` above 1 MiB and answers
/// `{"content": "", "encoding": "none"}`; the client then falls back to
/// `GET /git/blobs/{sha}` on `!content && size > 0`
/// (`github.service.ts:984-1018`), which is the path this keeps exercised.
const MAX_INLINE_BYTES: u64 = 1024 * 1024;

/// `file` / `dir` / `symlink` / `submodule` from the git mode, which is the
/// only place the distinction survives an `ls-tree`.
pub fn entry_type(mode: &str) -> &'static str {
    match mode {
        "040000" | "40000" => "dir",
        "120000" => "symlink",
        "160000" => "submodule",
        _ => "file",
    }
}

/// One element of the directory array. `sha` and `path` are the only fields
/// read by the server; the rest are what octokit's own types make non-optional.
pub fn entry_json(urls: &Urls, full_name: &str, git_ref: &str, e: &Entry) -> serde_json::Value {
    let name = e.path.rsplit('/').next().unwrap_or(&e.path).to_string();
    let kind = entry_type(&e.mode);
    let api = format!("{}/repos/{full_name}", urls.api);
    let download = if kind == "file" {
        serde_json::json!(format!("{}/{full_name}/raw/{git_ref}/{}", urls.html, e.path))
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "name": name,
        "path": e.path,
        "sha": e.sha,
        "size": e.size.unwrap_or(0),
        "type": kind,
        "url": format!("{api}/contents/{}?ref={git_ref}", e.path),
        "git_url": format!("{api}/git/{}s/{}", if kind == "dir" { "tree" } else { "blob" }, e.sha),
        "html_url": format!("{}/{full_name}/blob/{git_ref}/{}", urls.html, e.path),
        "download_url": download,
        "_links": {
            "self": format!("{api}/contents/{}?ref={git_ref}", e.path),
            "git": format!("{api}/git/{}s/{}", if kind == "dir" { "tree" } else { "blob" }, e.sha),
            "html": format!("{}/{full_name}/blob/{git_ref}/{}", urls.html, e.path),
        },
    })
}

/// The file shape, with `content` inlined unless the blob is over
/// [`MAX_INLINE_BYTES`].
pub async fn file_json(
    view: &View,
    urls: &Urls,
    git_ref: &str,
    e: &Entry,
) -> GhResult<serde_json::Value> {
    let mut body = entry_json(urls, &view.full_name, git_ref, e);
    let size = e.size.unwrap_or(0);
    let (content, encoding) = if size > MAX_INLINE_BYTES {
        (String::new(), "none")
    } else {
        let bytes = reads::git(&view.local, &["cat-file", "blob", &e.sha]).await?;
        (reads::base64_github(&bytes), "base64")
    };
    if let Some(map) = body.as_object_mut() {
        map.insert("type".into(), serde_json::json!(entry_type(&e.mode)));
        map.insert("content".into(), serde_json::json!(content));
        map.insert("encoding".into(), serde_json::json!(encoding));
    }
    Ok(body)
}

/// A file, as JSON or as raw bytes depending on `Accept`. Shared with
/// `reads::get_readme`, which serves the same two representations.
pub async fn file_response(
    view: &View,
    urls: &Urls,
    headers: &HeaderMap,
    git_ref: &str,
    e: &Entry,
) -> GhResult<Response> {
    if reads::wants_raw(headers) {
        let bytes = reads::git(&view.local, &["cat-file", "blob", &e.sha]).await?;
        return Ok(reads::raw_response(bytes));
    }
    Ok(axum::Json(file_json(view, urls, git_ref, e).await?).into_response())
}

/// `GET /api/v3/repos/{o}/{r}/contents/{path}?ref=`.
pub async fn get_contents(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, path)): Path<(String, String, String)>,
    Query(q): Query<reads::RefQuery>,
) -> GhResult<Response> {
    contents(&st, &headers, &owner, &name, &path, q.git_ref.as_deref()).await
}

/// `GET /api/v3/repos/{o}/{r}/contents` — the repository root, which octokit
/// emits for `path: ""`.
pub async fn get_contents_root(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<reads::RefQuery>,
) -> GhResult<Response> {
    contents(&st, &headers, &owner, &name, "", q.git_ref.as_deref()).await
}

async fn contents(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    path: &str,
    git_ref: Option<&str>,
) -> GhResult<Response> {
    let id = repo::repo_id(owner, name)?;
    let view = repo::objects_view(st, &id).await?;
    // An empty repository has no HEAD to resolve, and GitHub answers a 404
    // there rather than an empty listing.
    let commit = reads::resolve_ref(&view, git_ref).await?;
    let path = path.trim_matches('/').to_string();
    let urls = Urls::from_request(st, headers);
    let display_ref = git_ref.filter(|r| !r.is_empty()).unwrap_or(&commit);

    if path.is_empty() {
        let entries = reads::ls_tree(&view.local, &format!("{commit}^{{tree}}"), false).await?;
        return Ok(directory(&view, &urls, headers, display_ref, "", &entries));
    }

    let entry = lookup(&view, &commit, &path).await?;
    if entry_type(&entry.mode) == "dir" {
        let entries = reads::ls_tree(&view.local, &entry.sha, false).await?;
        let prefixed: Vec<Entry> = entries
            .into_iter()
            .map(|mut e| {
                e.path = format!("{path}/{}", e.path);
                e
            })
            .collect();
        return Ok(directory(&view, &urls, headers, display_ref, &path, &prefixed));
    }
    file_response(&view, &urls, headers, display_ref, &entry).await
}

/// The array shape, or the `object+json` envelope when the client asked for it.
fn directory(
    view: &View,
    urls: &Urls,
    headers: &HeaderMap,
    git_ref: &str,
    path: &str,
    entries: &[Entry],
) -> Response {
    let items: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| entry_json(urls, &view.full_name, git_ref, e))
        .collect();
    if reads::wants_object(headers) {
        let api = format!("{}/repos/{}", urls.api, view.full_name);
        return axum::Json(serde_json::json!({
            "type": "dir",
            "size": 0,
            "name": path.rsplit('/').next().unwrap_or(""),
            "path": path,
            "sha": "",
            "url": format!("{api}/contents/{path}?ref={git_ref}"),
            "git_url": format!("{api}/git/trees/{git_ref}"),
            "html_url": format!("{}/{}/tree/{git_ref}/{path}", urls.html, view.full_name),
            "download_url": serde_json::Value::Null,
            "entries": items,
            "_links": {
                "self": format!("{api}/contents/{path}?ref={git_ref}"),
                "git": format!("{api}/git/trees/{git_ref}"),
                "html": format!("{}/{}/tree/{git_ref}/{path}", urls.html, view.full_name),
            },
        }))
        .into_response();
    }
    axum::Json(items).into_response()
}

/// One path inside a commit's tree, as an [`Entry`]. `ls-tree` on the parent
/// is what carries the mode and the size; `<commit>:<path>` alone would not
/// distinguish a symlink from a file.
async fn lookup(view: &View, commit: &str, path: &str) -> GhResult<Entry> {
    let (parent, leaf) = match path.rsplit_once('/') {
        Some((p, l)) => (format!("{commit}:{p}"), l.to_string()),
        None => (format!("{commit}^{{tree}}"), path.to_string()),
    };
    let entries = reads::ls_tree(&view.local, &parent, false)
        .await
        .map_err(|_| GhError::not_found(path))?;
    let mut found = entries
        .into_iter()
        .find(|e| e.path == leaf)
        .ok_or_else(|| GhError::not_found(path))?;
    found.path = path.to_string();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::entry_type;

    #[test]
    fn modes_map_to_githubs_four_types() {
        assert_eq!(entry_type("100644"), "file");
        assert_eq!(entry_type("100755"), "file");
        assert_eq!(entry_type("040000"), "dir");
        assert_eq!(entry_type("40000"), "dir");
        assert_eq!(entry_type("120000"), "symlink");
        assert_eq!(entry_type("160000"), "submodule");
    }
}
