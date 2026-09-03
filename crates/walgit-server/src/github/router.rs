//! The mount, plus the thin handlers that sit directly on [`super::write`].
//!
//! URL conventions the client fixes for us (`docs/GITHUB.md`):
//! - octokit with `baseUrl = <origin>/api/v3` emits REST under `/api/v3`.
//! - `@octokit/graphql` rewrites a `/api/v3` base to `/api/graphql`; a
//!   hand-rolled client concatenating `${baseUrl}/graphql` emits
//!   `/api/v3/graphql`. Both are mounted on one handler.
//! - `@octokit/oauth-methods` strips `/api/v3` from the base, so the web flow
//!   is `<origin>/login/oauth/{authorize,access_token}`.
//!
//! Everything under `/api/v3` that is not routed answers a GitHub-shaped 404
//! rather than falling through to walgit's repo-prefix dispatcher, which would
//! read `api/v3` as an owner and a repository.

#![allow(clippy::unused_async)]

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::auth;
use super::error::{FieldError, GhError, GhResult};
use super::models::{self, Urls};
use super::repo;
use super::write;
use crate::AppState;

/// Mount the facade. Called from `crate::router` only when `github.enabled`.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v3/app", get(auth::app))
        .route("/api/v3/app/installations", get(auth::installations))
        .route(
            "/api/v3/app/installations/{id}",
            get(auth::installation).delete(auth::delete_installation),
        )
        .route(
            "/api/v3/app/installations/{id}/access_tokens",
            post(auth::access_tokens),
        )
        .route(
            "/api/v3/installation/repositories",
            get(repo::installation_repositories),
        )
        .route("/api/v3/user", get(auth::user))
        .route("/api/v3/user/installations", get(auth::user_installations))
        .route("/api/v3/users/{login}", get(auth::user_by_login))
        .route("/api/v3/rate_limit", get(auth::rate_limit))
        .route("/api/v3/applications/{client_id}/grant", delete(auth::revoke))
        .route("/api/v3/applications/{client_id}/token", delete(auth::revoke))
        .route("/api/v3/repos/{owner}/{repo}", get(repo::get_repo))
        .route(
            "/api/v3/repos/{owner}/{repo}/collaborators/{username}/permission",
            get(auth::collaborator_permission),
        )
        .route("/api/v3/repos/{owner}/{repo}/commits", get(repo::list_commits))
        .route(
            "/api/v3/repos/{owner}/{repo}/commits/{*ref}",
            get(repo::get_commit),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/branches",
            get(repo::list_branches),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/branches/{*branch}",
            get(repo::get_branch),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/git/commits/{sha}",
            get(repo::get_git_commit),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/git/ref/{*ref}",
            get(repo::get_ref),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/git/matching-refs/{*ref}",
            get(repo::matching_refs),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/git/refs",
            get(repo::all_refs).post(create_ref),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/git/refs/{*ref}",
            get(repo::matching_refs)
                .patch(update_ref)
                .delete(delete_ref),
        )
        .route("/api/graphql", post(graphql))
        .route("/api/v3/graphql", post(graphql))
        .route("/login/oauth/authorize", get(auth::authorize))
        .route("/login/oauth/access_token", post(auth::access_token))
        .route(
            "/api/v3/{*rest}",
            get(unrouted)
                .post(unrouted)
                .patch(unrouted)
                .put(unrouted)
                .delete(unrouted),
        )
        .layer(axum::middleware::from_fn(rate_limit_headers))
        .with_state(state)
}

/// Every response carries the rate-limit headers a client budgets against
/// (they are read off responses rather than from `/rate_limit`) and the
/// request id a client puts in its own error logs. Nothing here is limited,
/// so the numbers never move.
async fn rate_limit_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    if let Ok(v) = axum::http::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()) {
        resp.headers_mut().insert(
            axum::http::HeaderName::from_static("x-github-request-id"),
            v,
        );
    }
    let h = resp.headers_mut();
    for (k, v) in [
        ("x-ratelimit-limit", "1000000"),
        ("x-ratelimit-remaining", "1000000"),
        ("x-ratelimit-used", "0"),
        ("x-ratelimit-resource", "core"),
    ] {
        h.insert(
            axum::http::HeaderName::from_static(k),
            axum::http::HeaderValue::from_static(v),
        );
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&(auth::now_secs() + 3600).to_string()) {
        h.insert(
            axum::http::HeaderName::from_static("x-ratelimit-reset"),
            v,
        );
    }
    resp
}

async fn unrouted(Path(rest): Path<String>) -> Response {
    GhError::not_found(format!("/api/v3/{rest}")).into_response()
}

// ---- ref writes --------------------------------------------------------------

#[derive(Deserialize)]
struct CreateRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

/// `POST /api/v3/repos/{o}/{r}/git/refs` — `{ref, sha}`, 201 on success.
async fn create_ref(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<CreateRef>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let written = write::create_ref(&st, &id, &body.ref_name, &body.sha).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(ref_body(&st, &headers, &id, &written)),
    )
        .into_response())
}

#[derive(Deserialize)]
struct UpdateRef {
    sha: String,
    #[serde(default)]
    force: bool,
}

/// `PATCH /api/v3/repos/{o}/{r}/git/refs/{ref}` — fast-forward unless `force`.
async fn update_ref(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, r)): Path<(String, String, String)>,
    Json(body): Json<UpdateRef>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let full = format!("refs/{}", r.trim_start_matches('/'));
    let written = write::update_ref(&st, &id, &full, &body.sha, body.force).await?;
    Ok(Json(ref_body(&st, &headers, &id, &written)).into_response())
}

/// `DELETE /api/v3/repos/{o}/{r}/git/refs/{ref}` — 204.
async fn delete_ref(
    State(st): State<Arc<AppState>>,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    let full = format!("refs/{}", r.trim_start_matches('/'));
    write::delete_ref(&st, &id, &full).await?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

fn ref_body(
    st: &AppState,
    headers: &HeaderMap,
    id: &walgit_git::RepoId,
    written: &write::RefWritten,
) -> serde_json::Value {
    let urls = Urls::from_request(st, headers);
    models::git_ref(
        &urls,
        &id.to_string(),
        &written.ref_name,
        &written.oid,
        "commit",
    )
}

// ---- graphql -----------------------------------------------------------------

#[derive(Deserialize)]
struct GraphQlRequest {
    query: String,
}

/// The GraphQL endpoint, parsed but not yet dispatched. The next phase fills
/// in arms keyed on the operation's top-level field; until then every request
/// is answered with the field it would have needed, so a client's failure
/// names the gap instead of a transport error.
async fn graphql(Json(body): Json<GraphQlRequest>) -> Response {
    let what = top_level_field(&body.query)
        .unwrap_or_else(|| "unparseable operation".to_string());
    Json(serde_json::json!({
        "data": serde_json::Value::Null,
        "errors": [{ "message": format!("not implemented: {what}") }],
    }))
    .into_response()
}

/// The operation's name plus its first selected field — enough for a dispatch
/// table and enough for a human reading the error.
fn top_level_field(query: &str) -> Option<String> {
    use graphql_parser::query::{Definition, OperationDefinition, Selection};
    let doc = graphql_parser::parse_query::<&str>(query).ok()?;
    for def in &doc.definitions {
        let Definition::Operation(op) = def else {
            continue;
        };
        let (name, set) = match op {
            OperationDefinition::Query(q) => (q.name, &q.selection_set),
            OperationDefinition::Mutation(m) => (m.name, &m.selection_set),
            OperationDefinition::Subscription(s) => (s.name, &s.selection_set),
            OperationDefinition::SelectionSet(s) => (None, s),
        };
        let field = set.items.iter().find_map(|i| match i {
            Selection::Field(f) => Some(f.name),
            _ => None,
        });
        return Some(match (name, field) {
            (Some(n), Some(f)) => format!("{n}.{f}"),
            (None, Some(f)) => f.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => "anonymous operation".to_string(),
        });
    }
    None
}

/// A body that is not JSON, or is missing `ref`/`sha`, is GitHub's 422.
pub fn invalid_body(what: &str) -> GhError {
    GhError::validation(
        "Validation Failed",
        FieldError::invalid("Reference", "ref", what.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::top_level_field;

    #[test]
    fn names_the_operation_and_its_first_field() {
        assert_eq!(
            top_level_field("query getLatestCommit($o:String!){ repository { ref { name } } }")
                .as_deref(),
            Some("getLatestCommit.repository")
        );
        assert_eq!(
            top_level_field("mutation CreateCommit($i:X!){ createCommitOnBranch(input:$i){ commit { oid } } }")
                .as_deref(),
            Some("CreateCommit.createCommitOnBranch")
        );
        assert_eq!(
            top_level_field("{ viewer { login } }").as_deref(),
            Some("viewer")
        );
        assert_eq!(top_level_field("not graphql at all {{{"), None);
    }
}
