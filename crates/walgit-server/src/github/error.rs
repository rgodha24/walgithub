//! GitHub-shaped JSON errors: `{message, documentation_url}` plus `errors[]`
//! on a 422, which is what octokit's `RequestError` reads and what the
//! Mintlify server logs. Never 401/403 — the facade has no auth
//! (`docs/GITHUB.md`); a credential problem cannot exist, so a client that
//! sees one would be debugging a fiction.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// One entry of a 422's `errors[]` (GitHub's validation shape).
#[derive(Debug, Serialize)]
pub struct FieldError {
    pub resource: String,
    pub field: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl FieldError {
    pub fn invalid(resource: &str, field: &str, message: impl Into<String>) -> Self {
        Self {
            resource: resource.to_string(),
            field: field.to_string(),
            code: "invalid".to_string(),
            message: Some(message.into()),
        }
    }
}

#[derive(Debug)]
pub enum GhError {
    /// Not Found (404). GitHub answers a missing repository, ref or object with this and
    /// nothing else — no distinction between "absent" and "not visible".
    NotFound(String),
    /// Conflict (409). A ref moved under a write (`sha` did not match) or the update was
    /// not a fast-forward and `force` was not set.
    Conflict(String),
    /// Validation Failed (422).
    Validation {
        message: String,
        errors: Vec<FieldError>,
    },
    /// Bad Request (400).
    BadRequest(String),
    /// Service Unavailable (503) — the repository's objects are not available on this host
    /// (placement, a too-large pack set). Carries `Retry-After`.
    Unavailable(String),
    /// Internal Server Error (500).
    Internal(String),
}

#[derive(Serialize)]
struct Body<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: &'a Vec<FieldError>,
    documentation_url: &'static str,
}

const DOCS: &str = "https://docs.github.com/rest";

impl GhError {
    pub fn status(&self) -> StatusCode {
        match self {
            GhError::NotFound(_) => StatusCode::NOT_FOUND,
            GhError::Conflict(_) => StatusCode::CONFLICT,
            GhError::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            GhError::BadRequest(_) => StatusCode::BAD_REQUEST,
            GhError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            GhError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            GhError::NotFound(_) => "Not Found".to_string(),
            GhError::Conflict(m)
            | GhError::BadRequest(m)
            | GhError::Unavailable(m)
            | GhError::Internal(m) => m.clone(),
            GhError::Validation { message, .. } => message.clone(),
        }
    }

    /// A 422 with one field error — the shape GitHub uses for a bad ref name
    /// or an update that would not fast-forward.
    pub fn validation(message: &str, err: FieldError) -> Self {
        GhError::Validation {
            message: message.to_string(),
            errors: vec![err],
        }
    }

    pub fn not_found(what: impl std::fmt::Display) -> Self {
        GhError::NotFound(what.to_string())
    }
}

impl IntoResponse for GhError {
    fn into_response(self) -> Response {
        let status = self.status();
        let message = self.message();
        if let GhError::NotFound(what) = &self {
            tracing::debug!(what = %what, "github facade: not found");
        }
        if status.is_server_error() {
            tracing::warn!(status = status.as_u16(), error = %message, "github facade request failed");
        }
        let empty = Vec::new();
        let errors = match &self {
            GhError::Validation { errors, .. } => errors,
            _ => &empty,
        };
        let body = axum::Json(Body {
            message: &message,
            errors,
            documentation_url: DOCS,
        });
        let mut resp = (status, body).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("15"),
            );
        }
        resp
    }
}

impl From<crate::error::ApiError> for GhError {
    fn from(e: crate::error::ApiError) -> Self {
        match e {
            crate::error::ApiError::NotFound(m) => GhError::NotFound(m),
            crate::error::ApiError::Conflict(m) => GhError::Conflict(m),
            crate::error::ApiError::BadRequest(m) => GhError::BadRequest(m),
            crate::error::ApiError::ServiceUnavailable(m) => GhError::Unavailable(m),
            other => GhError::Internal(other.message()),
        }
    }
}

impl From<walgit_wal::WalError> for GhError {
    fn from(e: walgit_wal::WalError) -> Self {
        match e {
            walgit_wal::WalError::NotFound => GhError::NotFound("repository".into()),
            other => GhError::Internal(other.to_string()),
        }
    }
}

pub type GhResult<T> = Result<T, GhError>;
