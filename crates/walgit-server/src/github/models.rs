//! GitHub JSON shapes.
//!
//! Only the fields a client actually reads are modelled, plus the ones
//! octokit's own types make non-optional. Numeric `id`s are derived from the
//! name (sha1, 48 bits, so they stay exact in JavaScript) — stable across
//! restarts and across instances, which is all anything needs them for.

use serde::Serialize;

use super::auth::{INSTALLATION_ID, INSTALLATION_TOKEN, USER_ID, USER_LOGIN};

/// The origins a response's URLs are built from: `api` is what octokit's
/// `baseUrl` points at (`…/api/v3`), `html` is the browser origin.
pub struct Urls {
    pub html: String,
    pub api: String,
}

impl Urls {
    pub fn from_request(st: &crate::AppState, headers: &axum::http::HeaderMap) -> Self {
        let html = crate::smart::request_base_url(st, headers);
        let api = format!("{html}/api/v3");
        Self { html, api }
    }
}

/// A stable 48-bit id for a name. GitHub ids are integers and clients key
/// caches on them, so they must not move between restarts.
pub fn id_for(name: &str) -> u64 {
    use sha1::Digest;
    let d = sha1::Sha1::digest(name.as_bytes());
    d.iter()
        .take(6)
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
        .max(1)
}

/// GitHub node ids are opaque base64 to every client; a deterministic one
/// keeps GraphQL mutations addressable in a later phase.
pub fn node_id(kind: &str, name: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("{kind}:{name}"))
}

#[derive(Serialize, Clone)]
pub struct User {
    pub login: String,
    pub id: u64,
    pub node_id: String,
    pub avatar_url: String,
    pub gravatar_id: String,
    pub url: String,
    pub html_url: String,
    pub followers_url: String,
    pub organizations_url: String,
    pub repos_url: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub site_admin: bool,
    pub name: String,
    pub email: String,
}

pub fn named_user(urls: &Urls, login: &str) -> User {
    User {
        login: login.to_string(),
        id: if login == USER_LOGIN {
            USER_ID
        } else {
            id_for(login)
        },
        node_id: node_id("User", login),
        avatar_url: format!("{}/_ui/avatar/{login}.png", urls.html),
        gravatar_id: String::new(),
        url: format!("{}/users/{login}", urls.api),
        html_url: format!("{}/{login}", urls.html),
        followers_url: format!("{}/users/{login}/followers", urls.api),
        organizations_url: format!("{}/users/{login}/orgs", urls.api),
        repos_url: format!("{}/users/{login}/repos", urls.api),
        kind: "User",
        site_admin: true,
        name: login.to_string(),
        email: format!("{login}@walgit.localhost"),
    }
}

pub fn user(urls: &Urls) -> User {
    named_user(urls, USER_LOGIN)
}

pub fn app(urls: &Urls) -> serde_json::Value {
    serde_json::json!({
        "id": 1,
        "slug": "walgit-github-facade",
        "node_id": node_id("Integration", "walgit"),
        "client_id": "Iv1.walgitdev",
        "owner": user(urls),
        "name": "walgit github facade",
        "description": "Local development facade served by walgit (docs/GITHUB.md).",
        "external_url": urls.html,
        "html_url": format!("{}/apps/walgit-github-facade", urls.html),
        "created_at": EPOCH,
        "updated_at": EPOCH,
        "permissions": permissions(),
        "events": [],
        "installations_count": 1,
    })
}

pub fn installation(urls: &Urls, id: u64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "node_id": node_id("Installation", &id.to_string()),
        "account": user(urls),
        "repository_selection": "all",
        "access_tokens_url": format!("{}/app/installations/{id}/access_tokens", urls.api),
        "repositories_url": format!("{}/installation/repositories", urls.api),
        "html_url": format!("{}/settings/installations/{id}", urls.html),
        "app_id": 1,
        "app_slug": "walgit-github-facade",
        "target_id": USER_ID,
        "target_type": "User",
        "permissions": permissions(),
        "events": [],
        "created_at": EPOCH,
        "updated_at": EPOCH,
        "suspended_by": serde_json::Value::Null,
        "suspended_at": serde_json::Value::Null,
    })
}

pub fn access_token(urls: &Urls) -> serde_json::Value {
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    serde_json::json!({
        "token": INSTALLATION_TOKEN,
        "expires_at": expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "permissions": permissions(),
        "repository_selection": "all",
        "installation": {
            "id": INSTALLATION_ID,
            "repositories_url": format!("{}/installation/repositories", urls.api),
        },
    })
}

/// Every permission at `write`: the facade is admin on everything.
pub fn permissions() -> serde_json::Value {
    serde_json::json!({
        "administration": "write",
        "checks": "write",
        "contents": "write",
        "deployments": "write",
        "issues": "write",
        "members": "read",
        "metadata": "read",
        "pull_requests": "write",
        "statuses": "write",
        "workflows": "write",
    })
}

/// A fixed timestamp for everything the facade has no real time for. Real
/// commit times come from the objects; a repository's `created_at` does not
/// exist in the WAL, and inventing "now" would make every response uncacheable
/// and every diff of two responses noisy.
pub const EPOCH: &str = "2020-01-01T00:00:00Z";

#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize)]
pub struct Permissions {
    pub admin: bool,
    pub maintain: bool,
    pub push: bool,
    pub triage: bool,
    pub pull: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize)]
pub struct Repository {
    pub id: u64,
    pub node_id: String,
    pub name: String,
    pub full_name: String,
    pub owner: User,
    pub private: bool,
    pub html_url: String,
    pub description: Option<String>,
    pub fork: bool,
    pub url: String,
    pub git_url: String,
    pub ssh_url: String,
    pub clone_url: String,
    pub homepage: Option<String>,
    pub size: u64,
    pub stargazers_count: u64,
    pub watchers_count: u64,
    pub forks_count: u64,
    pub open_issues_count: u64,
    pub language: Option<String>,
    pub archived: bool,
    pub disabled: bool,
    pub visibility: &'static str,
    pub default_branch: String,
    pub topics: Vec<String>,
    pub license: Option<serde_json::Value>,
    pub allow_squash_merge: bool,
    pub allow_merge_commit: bool,
    pub allow_rebase_merge: bool,
    pub permissions: Permissions,
    pub created_at: String,
    pub updated_at: String,
    pub pushed_at: String,
}

/// `default_branch` comes from the repository's HEAD; `pushed_at` from the WAL
/// manifest's `updated_at` when we have it.
pub fn repository(
    urls: &Urls,
    owner: &str,
    name: &str,
    default_branch: &str,
    pushed_at: &str,
) -> Repository {
    let full_name = format!("{owner}/{name}");
    Repository {
        id: id_for(&full_name),
        node_id: node_id("Repository", &full_name),
        name: name.to_string(),
        full_name: full_name.clone(),
        owner: named_user(urls, owner),
        private: false,
        html_url: format!("{}/{full_name}", urls.html),
        description: None,
        fork: false,
        url: format!("{}/repos/{full_name}", urls.api),
        git_url: format!("{}/{full_name}.git", urls.html),
        ssh_url: format!("{}/{full_name}.git", urls.html),
        clone_url: format!("{}/{full_name}.git", urls.html),
        homepage: None,
        size: 0,
        stargazers_count: 0,
        watchers_count: 0,
        forks_count: 0,
        open_issues_count: 0,
        language: None,
        archived: false,
        disabled: false,
        visibility: "private",
        default_branch: default_branch.to_string(),
        topics: Vec::new(),
        license: None,
        allow_squash_merge: true,
        allow_merge_commit: true,
        allow_rebase_merge: true,
        permissions: Permissions {
            admin: true,
            maintain: true,
            push: true,
            triage: true,
            pull: true,
        },
        created_at: EPOCH.to_string(),
        updated_at: pushed_at.to_string(),
        pushed_at: pushed_at.to_string(),
    }
}

/// A commit as parsed out of the object database.
#[derive(Clone, Debug)]
pub struct CommitFacts {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_date: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_date: String,
    pub message: String,
}

fn signature(name: &str, email: &str, date: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "email": email, "date": date })
}

/// `GET /repos/{o}/{r}/commits/{ref}` and the entries of the commits list.
/// `files`/`stats` are omitted — a diff needs a scratch checkout, which is a
/// later phase (`docs/GITHUB.md`).
pub fn commit(urls: &Urls, full_name: &str, c: &CommitFacts) -> serde_json::Value {
    let base = format!("{}/repos/{full_name}", urls.api);
    serde_json::json!({
        "sha": c.sha,
        "node_id": node_id("Commit", &format!("{full_name}:{}", c.sha)),
        "url": format!("{base}/commits/{}", c.sha),
        "html_url": format!("{}/{full_name}/commit/{}", urls.html, c.sha),
        "comments_url": format!("{base}/commits/{}/comments", c.sha),
        "commit": {
            "url": format!("{base}/git/commits/{}", c.sha),
            "author": signature(&c.author_name, &c.author_email, &c.author_date),
            "committer": signature(&c.committer_name, &c.committer_email, &c.committer_date),
            "message": c.message,
            "tree": {
                "sha": c.tree,
                "url": format!("{base}/git/trees/{}", c.tree),
            },
            "comment_count": 0,
            "verification": verification(),
        },
        "author": commit_user(urls, &c.author_name, &c.author_email),
        "committer": commit_user(urls, &c.committer_name, &c.committer_email),
        "parents": parents(urls, full_name, &c.parents),
    })
}

/// `GET /repos/{o}/{r}/git/commits/{sha}` — the git-data shape, which is the
/// commit object and nothing else.
pub fn git_commit(urls: &Urls, full_name: &str, c: &CommitFacts) -> serde_json::Value {
    let base = format!("{}/repos/{full_name}", urls.api);
    serde_json::json!({
        "sha": c.sha,
        "node_id": node_id("Commit", &format!("{full_name}:{}", c.sha)),
        "url": format!("{base}/git/commits/{}", c.sha),
        "html_url": format!("{}/{full_name}/commit/{}", urls.html, c.sha),
        "author": signature(&c.author_name, &c.author_email, &c.author_date),
        "committer": signature(&c.committer_name, &c.committer_email, &c.committer_date),
        "message": c.message,
        "tree": {
            "sha": c.tree,
            "url": format!("{base}/git/trees/{}", c.tree),
        },
        "parents": parents(urls, full_name, &c.parents),
        "verification": verification(),
    })
}

fn parents(urls: &Urls, full_name: &str, shas: &[String]) -> serde_json::Value {
    let base = format!("{}/repos/{full_name}", urls.api);
    serde_json::Value::Array(
        shas.iter()
            .map(|p| {
                serde_json::json!({
                    "sha": p,
                    "url": format!("{base}/commits/{p}"),
                    "html_url": format!("{}/{full_name}/commit/{p}", urls.html),
                })
            })
            .collect(),
    )
}

/// Commit authorship maps to a user stub keyed on the email's local part: the
/// facade has no account table, and every client only reads `login`.
fn commit_user(urls: &Urls, name: &str, email: &str) -> serde_json::Value {
    let login = email
        .split('@')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    serde_json::to_value(named_user(urls, login)).unwrap_or(serde_json::Value::Null)
}

fn verification() -> serde_json::Value {
    serde_json::json!({
        "verified": false,
        "reason": "unsigned",
        "signature": serde_json::Value::Null,
        "payload": serde_json::Value::Null,
    })
}

/// `GET /repos/{o}/{r}/branches[/{branch}]`.
pub fn branch(urls: &Urls, full_name: &str, name: &str, c: &CommitFacts) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "commit": commit(urls, full_name, c),
        "protected": false,
        "protection": {
            "enabled": false,
            "required_status_checks": { "enforcement_level": "off", "contexts": [] },
        },
        "protection_url": format!("{}/repos/{full_name}/branches/{name}/protection", urls.api),
    })
}

/// `GET|POST|PATCH /repos/{o}/{r}/git/ref[s]/…`. `object.type` is `tag` only
/// for an annotated tag, which is what a client peeling a release relies on.
pub fn git_ref(
    urls: &Urls,
    full_name: &str,
    name: &str,
    oid: &str,
    object_type: &str,
) -> serde_json::Value {
    let base = format!("{}/repos/{full_name}", urls.api);
    serde_json::json!({
        "ref": name,
        "node_id": node_id("Ref", &format!("{full_name}:{name}")),
        "url": format!("{base}/git/{name}"),
        "object": {
            "sha": oid,
            "type": object_type,
            "url": format!("{base}/git/{}s/{oid}", object_type),
        },
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_are_stable_and_js_safe() {
        let a = super::id_for("acme/docs");
        assert_eq!(a, super::id_for("acme/docs"));
        assert_ne!(a, super::id_for("acme/other"));
        assert!(a < (1u64 << 53));
        assert!(a > 0);
    }
}
