//! GraphQL errors, which are a 200 with a body.
//!
//! `githubApiClient.ts` turns any `200` whose body has a non-empty `errors[]`
//! into a `GraphqlResponseError` and reads exactly two fields of the first
//! entry: `type` and `message` (`docs/GITHUB_SHAPES.md`, "POST /graphql").
//! `createCommitOnBranch` then branches on the `type`, so the strings here are
//! GitHub's, not ours — `NOT_FOUND`, `UNPROCESSABLE`, `STALE_DATA`,
//! `FORBIDDEN`. `path` and `locations` are present because octokit's types say
//! they are, and read by nobody.

use serde_json::{Value, json};

use crate::github::error::GhError;

#[derive(Debug, Clone)]
pub struct GqlError {
    /// GitHub's `errors[0].type`.
    pub kind: &'static str,
    pub message: String,
    /// The field path the error is about, when there is one.
    pub path: Vec<String>,
}

impl GqlError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            path: Vec::new(),
        }
    }

    /// A repository, ref, object or pull request that is not there. The
    /// client's `createCommitOnBranch` maps this to a 404 and gives a message
    /// containing `Could not resolve to a Repository` its own summary, so
    /// resolution failures spell it GitHub's way.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("NOT_FOUND", message)
    }

    /// The input was understood and refused — GitHub's answer to an
    /// `expectedHeadOid` that is not where the branch is.
    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new("UNPROCESSABLE", message)
    }

    /// The branch moved under the write. The client turns this into a 412 and
    /// "Your branch is not up to date. Please try again".
    pub fn stale(message: impl Into<String>) -> Self {
        Self::new("STALE_DATA", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("INTERNAL", message)
    }

    /// A document the facade does not serve, named by `<operation>.<field>`
    /// so a client's failure names the gap instead of a transport error.
    pub fn not_implemented(what: impl std::fmt::Display) -> Self {
        Self::new("NOT_IMPLEMENTED", format!("not implemented: {what}"))
    }

    /// A malformed document or body: GraphQL's own parse failure, which
    /// GitHub also answers 200 with.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("BAD_REQUEST", message)
    }

    #[must_use]
    pub fn at(mut self, path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.path = path.into_iter().map(Into::into).collect();
        self
    }

    pub fn entry(&self) -> Value {
        json!({
            "type": self.kind,
            "message": self.message,
            "path": self.path,
            "locations": [{ "line": 1, "column": 1 }],
        })
    }

    /// The whole response body. GitHub answers errors with `200`.
    pub fn body(&self) -> Value {
        json!({ "data": Value::Null, "errors": [self.entry()] })
    }
}

impl From<GhError> for GqlError {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NotFound(what) => GqlError::not_found(format!("Could not resolve to {what}.")),
            // A ref that moved between the check and the CAS: the branch is
            // not where the caller thought, which is exactly STALE_DATA.
            GhError::Conflict(m) => GqlError::stale(m),
            GhError::Validation { message, errors } => {
                let detail = errors
                    .iter()
                    .filter_map(|e| e.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                if detail.is_empty() {
                    GqlError::unprocessable(message)
                } else {
                    GqlError::unprocessable(format!("{message}: {detail}"))
                }
            }
            GhError::BadRequest(m) => GqlError::bad_request(m),
            GhError::Unavailable(m) | GhError::Internal(m) => GqlError::internal(m),
        }
    }
}
