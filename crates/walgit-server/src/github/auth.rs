//! The facade's identity: one hardcoded user, admin on everything, and an
//! OAuth web flow that agrees with the client immediately.
//!
//! There is nothing to authenticate against. Whatever a client presents — an
//! app JWT, an installation token, a user OAuth token, nothing at all — is the
//! same principal, so the endpoints below hand back credentials that are
//! constants and permissions that are always `admin`. That is the whole trust
//! boundary of the facade and the reason `Config::validate` refuses
//! `github.enabled` off a loopback bind (`docs/GITHUB.md`).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::error::GhResult;
use super::models::{self, Urls};
use crate::AppState;

/// The one user the facade knows. `mintlify-dev` because the client this
/// exists for is the Mintlify server's octokit.
pub const USER_LOGIN: &str = "mintlify-dev";
pub const USER_ID: u64 = 1;
/// The installation every `installationId` in the client's database resolves
/// to. Any id in the path is accepted; this is what is reported back.
pub const INSTALLATION_ID: u64 = 1;
/// The installation token handed out by `access_tokens`. Never checked.
pub const INSTALLATION_TOKEN: &str = "ghs_dev";
/// The user token handed out by the OAuth flow. Never checked.
pub const OAUTH_TOKEN: &str = "gho_dev";
/// The authorization code the authorize endpoint redirects with.
pub const OAUTH_CODE: &str = "dev";

/// `GET /api/v3/user` — `id`, `login` and the `x-oauth-scopes` header are what
/// the Mintlify server reads.
pub async fn user(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let urls = Urls::from_request(&st, &headers);
    let mut resp = axum::Json(models::user(&urls)).into_response();
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-oauth-scopes"),
        axum::http::HeaderValue::from_static("repo, read:org, workflow"),
    );
    resp
}

/// `GET /api/v3/users/{login}` — `name` and `email` are read by the Slack
/// mapping service; every login exists.
pub async fn user_by_login(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(login): Path<String>,
) -> Response {
    let urls = Urls::from_request(&st, &headers);
    axum::Json(models::named_user(&urls, &login)).into_response()
}

/// `GET /api/v3/app` — `html_url` is what `App.getInstallationUrl()` reads to
/// build the "install this app" link.
pub async fn app(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let urls = Urls::from_request(&st, &headers);
    axum::Json(models::app(&urls)).into_response()
}

/// `GET /api/v3/app/installations`.
pub async fn installations(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let urls = Urls::from_request(&st, &headers);
    axum::Json(vec![models::installation(&urls, INSTALLATION_ID)]).into_response()
}

/// `GET /api/v3/app/installations/{installation_id}`. Any id, including the
/// literal `{installationId}` the client sometimes fails to substitute.
pub async fn installation(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let urls = Urls::from_request(&st, &headers);
    let id = id.parse().unwrap_or(INSTALLATION_ID);
    axum::Json(models::installation(&urls, id)).into_response()
}

/// `DELETE /api/v3/app/installations/{installation_id}` — nothing to delete.
pub async fn delete_installation() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `POST /api/v3/app/installations/{id}/access_tokens`. `@octokit/auth-app`
/// reads `token` and `expires_at`; `permissions` and `repository_selection`
/// are read by the Mintlify server's install checks.
pub async fn access_tokens(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let urls = Urls::from_request(&st, &headers);
    (
        StatusCode::CREATED,
        axum::Json(models::access_token(&urls)),
    )
        .into_response()
}

/// `GET /api/v3/user/installations` — the paginated shape octokit expects.
pub async fn user_installations(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let urls = Urls::from_request(&st, &headers);
    axum::Json(serde_json::json!({
        "total_count": 1,
        "installations": [models::installation(&urls, INSTALLATION_ID)],
    }))
    .into_response()
}

/// `GET /api/v3/repos/{o}/{r}/collaborators/{u}/permission` — always `admin`.
pub async fn collaborator_permission(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((_owner, _repo, login)): Path<(String, String, String)>,
) -> Response {
    let urls = Urls::from_request(&st, &headers);
    axum::Json(serde_json::json!({
        "permission": "admin",
        "role_name": "admin",
        "user": models::named_user(&urls, &login),
    }))
    .into_response()
}

/// `GET /api/v3/rate_limit`. Nothing here is rate limited; the numbers are
/// large and constant so a client that budgets against them never waits.
pub async fn rate_limit() -> Response {
    let core = serde_json::json!({
        "limit": 1_000_000,
        "used": 0,
        "remaining": 1_000_000,
        "reset": reset_at(),
    });
    axum::Json(serde_json::json!({
        "resources": {
            "core": core,
            "search": core,
            "graphql": core,
            "integration_manifest": core,
            "code_scanning_upload": core,
        },
        "rate": core,
    }))
    .into_response()
}

fn reset_at() -> u64 {
    now_secs() + 3600
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[derive(serde::Deserialize)]
pub struct AuthorizeQuery {
    redirect_uri: Option<String>,
    state: Option<String>,
}

/// `GET /login/oauth/authorize` — 302 straight back to `redirect_uri` with a
/// code. There is no consent form because there is nothing to consent to.
pub async fn authorize(Query(q): Query<AuthorizeQuery>) -> Response {
    let Some(redirect) = q.redirect_uri.filter(|u| !u.is_empty()) else {
        return super::error::GhError::BadRequest(
            "redirect_uri is required by the walgit github facade".into(),
        )
        .into_response();
    };
    let sep = if redirect.contains('?') { '&' } else { '?' };
    let mut location = format!("{redirect}{sep}code={OAUTH_CODE}");
    if let Some(state) = q.state.filter(|s| !s.is_empty()) {
        location.push_str("&state=");
        location.push_str(&urlencode(&state));
    }
    match axum::http::HeaderValue::from_str(&location) {
        Ok(v) => (StatusCode::FOUND, [(header::LOCATION, v)]).into_response(),
        Err(e) => super::error::GhError::BadRequest(format!("redirect_uri: {e}")).into_response(),
    }
}

/// `POST /login/oauth/access_token`. GitHub answers form-encoded unless the
/// client asks for JSON; `@octokit/oauth-methods` always asks for JSON, the
/// hand-rolled clients do not, so both are honoured.
pub async fn access_token(headers: HeaderMap, body: String) -> GhResult<Response> {
    let _ = body;
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"));
    if wants_json {
        return Ok(axum::Json(serde_json::json!({
            "access_token": OAUTH_TOKEN,
            "token_type": "bearer",
            "scope": "repo",
        }))
        .into_response());
    }
    Ok((
        [(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8",
        )],
        format!("access_token={OAUTH_TOKEN}&token_type=bearer&scope=repo"),
    )
        .into_response())
}

/// `DELETE /api/v3/applications/{client_id}/{grant|token}` — the Mintlify
/// server revokes on sign-out; nothing is stored, so this always succeeds.
pub async fn revoke() -> StatusCode {
    StatusCode::NO_CONTENT
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16)
        .unwrap_or('0')
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn state_is_escaped() {
        assert_eq!(super::urlencode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(super::urlencode("plain-1_2.3~4"), "plain-1_2.3~4");
    }
}
