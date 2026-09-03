//! `POST /api/graphql` and `POST /api/v3/graphql`.
//!
//! **There is no GraphQL engine here, on purpose.** Every document the client
//! sends is a string literal in its own source — `docs/GITHUB_SHAPES.md`
//! ("POST /graphql") lists all eleven of them, their variables and the exact
//! fields destructured off the response — so the facade parses the document
//! with `graphql-parser`, takes its single operation, resolves the arguments
//! against the JSON `variables`, and dispatches on field names. A schema, a
//! resolver graph and an executor would be a large amount of machinery to
//! answer eleven known questions.
//!
//! Shape of the answers:
//!
//! - Each arm returns the **full** documented shape of its node, not only the
//!   fields the document selected. The documents are literals that get edited;
//!   a response that carries only today's selection breaks on the next edit.
//!   Selection still decides *work* (`history` and `blame` are computed only
//!   when asked for).
//! - A field the facade does not serve is
//!   `{"data":null,"errors":[{"type":"NOT_IMPLEMENTED","message":"not
//!   implemented: <op>.<field>"}]}` with **HTTP 200** — GitHub answers every
//!   GraphQL error 200, and the client turns a non-empty `errors[]` into a
//!   `GraphqlResponseError` reading only `type` and `message`.
//! - Error `type`s are GitHub's, because `createCommitOnBranch` branches on
//!   them: `NOT_FOUND` → 404, `STALE_DATA` → 412 "not up to date",
//!   `UNPROCESSABLE` and anything else → 500.
//!
//! Missing-thing conventions follow the call sites: a repository that does not
//! resolve is a `NOT_FOUND` error, a `ref` that does not exist is `null` (the
//! caller falls back to REST), and `object` is `null` for every miss (the
//! caller swallows errors to `null`, so a miss must not look like an outage).

pub mod blame;
pub mod error;
pub mod mutate;
pub mod ops;
pub mod parse;

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use error::GqlError;
use ops::Ctx;
use parse::Kind;

use crate::AppState;
use crate::github::models::Urls;

/// `{query, variables, operationName}` — what both of the client's GraphQL
/// clients POST.
#[derive(Deserialize)]
pub struct Request {
    query: String,
    #[serde(default)]
    variables: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    operation_name: Option<String>,
    /// The camelCase spelling `@octokit/graphql` actually sends.
    #[serde(default, rename = "operationName")]
    operation_name_camel: Option<String>,
}

/// The endpoint. Always 200: a GraphQL failure is a body, not a status.
pub async fn handler(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<axum::Json<Request>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let axum::Json(req) = match body {
        Ok(b) => b,
        Err(e) => {
            return axum::Json(GqlError::bad_request(format!("invalid request body: {e}")).body())
                .into_response();
        }
    };
    let vars = req.variables.unwrap_or_default();
    let name = req
        .operation_name
        .or(req.operation_name_camel);
    let op = match parse::parse(&req.query, &vars, name.as_deref()) {
        Ok(op) => op,
        Err(e) => return axum::Json(GqlError::bad_request(e).body()).into_response(),
    };
    let ctx = Ctx {
        urls: Urls::from_request(&st, &headers),
        st,
    };
    let label = op.label();
    let result = match op.kind {
        Kind::Query => ops::query(&ctx, &label, &op.fields).await,
        Kind::Mutation => mutate::mutation(&ctx, &label, &op.fields).await,
    };
    match result {
        Ok(data) => axum::Json(json!({ "data": data })).into_response(),
        Err(e) => {
            tracing::debug!(kind = e.kind, error = %e.message, operation = %label, "github graphql");
            axum::Json(e.body()).into_response()
        }
    }
}
