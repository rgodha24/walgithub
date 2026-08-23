//! Repo-route parsing: extract `{owner}/{repo}[.git]` plus the sub-path from a
//! request path, and build a [`walgit_git::RepoId`].

use walgit_git::RepoId;

/// A parsed repo route.
#[derive(Debug)]
pub struct RepoRoute {
    pub id: RepoId,
    /// Sub-path after `/{owner}/{repo}[.git]/`, without leading slash. Empty for
    /// the repo root (PUT/DELETE).
    pub subpath: String,
    /// True when the request used the `.git` suffix.
    pub had_git_suffix: bool,
}

/// Parse `/owner/repo[.git][/sub...]`. Returns `None` when the path is not a
/// valid repo route (caller returns 404).
pub fn parse_repo_route(path: &str) -> Option<RepoRoute> {
    let path = path.strip_prefix('/').unwrap_or(path);
    // Split into the repo-identifier prefix and the sub-path.
    // owner/repo.git/info/refs  -> (owner, repo, .git, info/refs)
    let mut it = path.splitn(3, '/');
    let owner = it.next()?;
    let rest = it.next()?;
    let sub = it.next().unwrap_or("");
    // Strip optional .git from `rest`.
    let (name, had_git) = match rest.strip_suffix(".git") {
        Some(n) => (n, true),
        None => (rest, false),
    };
    let id: RepoId = format!("{owner}/{name}").parse().ok()?;
    Some(RepoRoute {
        id,
        subpath: sub.trim_start_matches('/').to_string(),
        had_git_suffix: had_git,
    })
}

/// True when `path` is a non-repo top-level route (`/`, `/healthz`, ...).
pub fn is_top_level(path: &str) -> bool {
    matches!(
        path.trim_matches('/'),
        "" | "healthz" | "readyz" | "metrics"
    )
}
