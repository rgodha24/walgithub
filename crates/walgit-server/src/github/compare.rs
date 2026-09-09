//! `GET /repos/{o}/{r}/compare/{base}...{head}` — 457k calls/7d, and the one
//! read whose absence is a hard error rather than a degraded answer: a missing
//! `files` on the diff path throws before anything else can run
//! (`docs/GITHUB_SHAPES.md`).
//!
//! GitHub's compare is three-dot: everything is measured from the **merge
//! base**, not from `base` itself. `ahead_by`/`behind_by`, `commits[]` and
//! `files[]` all follow from that, which is why `merge_base_commit` is a full
//! commit object here — the server dereferences `merge_base_commit.sha` with
//! no null guard.

#![allow(clippy::unused_async)]

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use super::error::{GhError, GhResult};
use super::models::{self, Urls};
use super::repo::{self, View};
use super::{diff, error};
use crate::AppState;

/// GitHub returns at most 250 commits and 300 files from one compare, whatever
/// `per_page` says; `compareRef` sends no pagination at all and still expects
/// the whole file list, so these are the defaults rather than 30.
const MAX_COMMITS_PER_PAGE: usize = 250;
const MAX_FILES_PER_PAGE: usize = 300;

#[derive(serde::Deserialize, Default)]
pub struct CompareQuery {
    page: Option<usize>,
    per_page: Option<usize>,
}

/// `owner:branch` is how a cross-fork compare is spelled. There are no forks
/// here — every repository is its own truth — so the owner is stripped and the
/// rest resolved locally.
fn strip_owner(spec: &str) -> &str {
    match spec.split_once(':') {
        Some((_, rest)) if !rest.is_empty() => rest,
        _ => spec,
    }
}

/// `base...head`, with `base..head` accepted too because a hand-rolled client
/// eventually emits one.
fn split_basehead(spec: &str) -> GhResult<(&str, &str)> {
    let (base, head) = spec
        .split_once("...")
        .or_else(|| spec.split_once(".."))
        .ok_or_else(|| {
            GhError::validation(
                "Validation Failed",
                error::FieldError::invalid(
                    "Comparison",
                    "basehead",
                    format!("{spec} is not a <base>...<head> pair"),
                ),
            )
        })?;
    if base.is_empty() || head.is_empty() {
        return Err(GhError::not_found(spec));
    }
    Ok((strip_owner(base), strip_owner(head)))
}

/// `GET /api/v3/repos/{o}/{r}/compare/{basehead}`.
pub async fn compare(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, basehead)): Path<(String, String, String)>,
    Query(q): Query<CompareQuery>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let view = repo::objects_view(&st, &id).await?;
    let (base_spec, head_spec) = split_basehead(&basehead)?;
    let base = repo::resolve_commitish(&view, base_spec).await?;
    let head = repo::resolve_commitish(&view, head_spec).await?;
    let merge_base = merge_base(&view, &base, &head).await?;

    let ahead_by = count(&view, &merge_base, &head).await?;
    let behind_by = count(&view, &merge_base, &base).await?;
    let status = if base == head {
        "identical"
    } else if behind_by == 0 {
        "ahead"
    } else if ahead_by == 0 {
        "behind"
    } else {
        "diverged"
    };

    let commits_per = q.per_page.unwrap_or(MAX_COMMITS_PER_PAGE).clamp(1, MAX_COMMITS_PER_PAGE);
    let files_per = q.per_page.unwrap_or(MAX_FILES_PER_PAGE).clamp(1, MAX_FILES_PER_PAGE);
    let page = q.page.unwrap_or(1).max(1);
    let skip = page.saturating_sub(1);

    // Oldest first, which is the order `commits.at(-1)` is read as the head.
    let log = repo::git(
        &view.local,
        &[
            "log",
            "--reverse",
            &format!("--format={}", repo::LOG_FORMAT),
            "--no-color",
            &format!("--skip={}", skip.saturating_mul(commits_per)),
            &format!("-{commits_per}"),
            "--end-of-options",
            &format!("{merge_base}..{head}"),
        ],
    )
    .await?;
    let urls = Urls::from_request(&st, &headers);
    let commits: Vec<serde_json::Value> = repo::parse_commits(&log)
        .iter()
        .map(|c| models::commit(&urls, &view.full_name, c))
        .collect();

    let all_files = diff::changed_files(&view.local, &merge_base, &head).await?;
    let files: Vec<serde_json::Value> = all_files
        .iter()
        .skip(skip.saturating_mul(files_per))
        .take(files_per)
        .map(|f| diff::file_json(&urls, &view.full_name, &head, f))
        .collect();

    let base_commit = repo::commit_facts(&view.local, &base).await?;
    let merge_base_commit = repo::commit_facts(&view.local, &merge_base).await?;
    let html = format!(
        "{}/{}/compare/{base}...{head}",
        urls.html, view.full_name
    );
    Ok(axum::Json(serde_json::json!({
        "url": format!("{}/repos/{}/compare/{base}...{head}", urls.api, view.full_name),
        "html_url": html,
        "permalink_url": html,
        "diff_url": format!("{html}.diff"),
        "patch_url": format!("{html}.patch"),
        "base_commit": models::commit(&urls, &view.full_name, &base_commit),
        "merge_base_commit": models::commit(&urls, &view.full_name, &merge_base_commit),
        "status": status,
        "ahead_by": ahead_by,
        "behind_by": behind_by,
        "total_commits": ahead_by,
        "commits": commits,
        "files": files,
    }))
    .into_response())
}

/// The merge base, falling back to `base` for unrelated histories — GitHub
/// still answers a comparison there, and `merge_base_commit.sha` is read with
/// no null guard.
async fn merge_base(view: &View, base: &str, head: &str) -> GhResult<String> {
    Ok(repo::merge_base(&view.local, base, head)
        .await?
        .unwrap_or_else(|| base.to_string()))
}

async fn count(view: &View, from: &str, to: &str) -> GhResult<u64> {
    if from == to {
        return Ok(0);
    }
    repo::commit_count(&view.local, from, to).await
}

#[cfg(test)]
mod tests {
    use super::{split_basehead, strip_owner};

    #[test]
    fn three_dots_and_two_dots_both_split() {
        assert_eq!(split_basehead("main...topic").ok(), Some(("main", "topic")));
        assert_eq!(split_basehead("main..topic").ok(), Some(("main", "topic")));
        assert!(split_basehead("main").is_err());
        assert!(split_basehead("...topic").is_err());
    }

    #[test]
    fn a_fork_qualified_side_loses_its_owner() {
        assert_eq!(strip_owner("acme:main"), "main");
        assert_eq!(strip_owner("main"), "main");
        assert_eq!(
            split_basehead("acme:main...contrib:topic").ok(),
            Some(("main", "topic"))
        );
    }

    #[test]
    fn a_branch_with_slashes_survives() {
        assert_eq!(
            split_basehead("main...editor/quickstart").ok(),
            Some(("main", "editor/quickstart"))
        );
    }
}
