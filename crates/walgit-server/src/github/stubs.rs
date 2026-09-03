//! Accept-and-forget endpoints (`docs/GITHUB_SHAPES.md`, Tier 4).
//!
//! Check runs, deployments and commit statuses exist so a client's happy path
//! does not fault. Nothing here is durable: state lives in a bounded
//! in-memory map that a restart forgets, because every one of these is a
//! write whose response is read once (for an `id`) and then only written
//! back to. Persisting them would put deploy chatter in the bucket for no
//! reader.
//!
//! The map is bounded and evicts oldest-first: a long-running facade takes
//! thousands of check-run `PATCH`es and must not grow without limit.

#![allow(clippy::unused_async)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::error::GhResult;
use super::models::{self, Urls};
use crate::AppState;

/// Room for a busy afternoon of deploys; the oldest entries fall off.
const MAX_ENTRIES: usize = 4096;

/// Every check run and deployment this process has seen, newest last.
static STATE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::default()));

#[derive(Default)]
struct Store {
    next_id: u64,
    /// Check runs by id, plus the head sha they were created against.
    check_runs: HashMap<u64, Value>,
    order: Vec<u64>,
    deployments: HashMap<u64, Value>,
    deployment_order: Vec<u64>,
}

impl Store {
    fn id(&mut self) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        1_000_000_000 + self.next_id
    }

    fn insert_check(&mut self, id: u64, value: Value) {
        if self.check_runs.insert(id, value).is_none() {
            self.order.push(id);
        }
        while self.order.len() > MAX_ENTRIES {
            if let Some(old) = self.order.first().copied() {
                self.order.remove(0);
                self.check_runs.remove(&old);
            }
        }
    }

    fn insert_deployment(&mut self, id: u64, value: Value) {
        self.deployments.insert(id, value);
        self.deployment_order.push(id);
        while self.deployment_order.len() > MAX_ENTRIES {
            if let Some(old) = self.deployment_order.first().copied() {
                self.deployment_order.remove(0);
                self.deployments.remove(&old);
            }
        }
    }
}

/// A poisoned mutex here means a panic inside a handler that only touches
/// `serde_json::Value`s; recovering the guard is strictly better than
/// propagating a 500 to every later deploy.
fn state() -> std::sync::MutexGuard<'static, Store> {
    STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Every route this module owns.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v3/repos/{owner}/{repo}/check-runs",
            post(create_check_run),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/check-runs/{check_run_id}",
            get(get_check_run).patch(update_check_run),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/statuses/{sha}",
            post(create_status),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/deployments",
            get(list_deployments).post(create_deployment),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/deployments/{deployment_id}/statuses",
            get(list_deployment_statuses).post(create_deployment_status),
        )
        .route(
            "/api/v3/repos/{owner}/{repo}/rules/branches/{branch}",
            get(branch_rules),
        )
}

fn now() -> String {
    super::pr_store::now()
}

// ---- check runs --------------------------------------------------------------

/// `POST /repos/{o}/{r}/check-runs` — 201 with the echoed fields and an id.
async fn create_check_run(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let full_name = format!("{owner}/{name}");
    let mut store = state();
    let id = store.id();
    let run = check_run_json(&urls, &full_name, id, &body, None);
    store.insert_check(id, run.clone());
    drop(store);
    Ok((StatusCode::CREATED, Json(run)).into_response())
}

/// `PATCH /repos/{o}/{r}/check-runs/{id}` — 1.7M calls a week in production,
/// concurrent by construction. Each PATCH merges into whatever is there; a run
/// this process never created is accepted and remembered anyway, because the
/// client is retrying against a facade that restarted.
async fn update_check_run(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Json(body): Json<Value>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let full_name = format!("{owner}/{name}");
    let mut store = state();
    let existing = store.check_runs.get(&id).cloned();
    let run = check_run_json(&urls, &full_name, id, &body, existing.as_ref());
    store.insert_check(id, run.clone());
    drop(store);
    Ok(Json(run).into_response())
}

async fn get_check_run(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let full_name = format!("{owner}/{name}");
    let store = state();
    let existing = store.check_runs.get(&id).cloned();
    drop(store);
    let run = existing
        .unwrap_or_else(|| check_run_json(&urls, &full_name, id, &Value::Null, None));
    Ok(Json(run).into_response())
}

/// `GET /repos/{o}/{r}/commits/{ref}/check-runs` — `octokit.paginate` drives
/// this, so the envelope is `{total_count, check_runs}` and a `Link` header
/// would only ever say "no next page".
///
/// Reached through [`super::prs::commit_or_subroute`]: a route with a path
/// parameter here cannot be registered beside the `commits/{*ref}` wildcard.
pub fn list_check_runs(sha: &str) -> Response {
    let store = state();
    let runs: Vec<Value> = store
        .order
        .iter()
        .filter_map(|id| store.check_runs.get(id))
        .filter(|r| r.get("head_sha").and_then(Value::as_str) == Some(sha))
        .cloned()
        .collect();
    drop(store);
    Json(json!({ "total_count": runs.len(), "check_runs": runs })).into_response()
}

fn check_run_json(
    urls: &Urls,
    full_name: &str,
    id: u64,
    body: &Value,
    previous: Option<&Value>,
) -> Value {
    let take = |key: &str| -> Option<Value> {
        body.get(key)
            .cloned()
            .or_else(|| previous.and_then(|p| p.get(key).cloned()))
    };
    let status = take("status")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "queued".to_string());
    let head_sha = take("head_sha")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    json!({
        "id": id,
        "node_id": models::node_id("CheckRun", &format!("{full_name}:{id}")),
        "name": take("name").unwrap_or_else(|| json!("walgit")),
        "head_sha": head_sha,
        "external_id": take("external_id").unwrap_or(Value::Null),
        "status": status,
        "conclusion": take("conclusion").unwrap_or(Value::Null),
        "started_at": take("started_at").unwrap_or_else(|| json!(now())),
        "completed_at": take("completed_at").unwrap_or(Value::Null),
        "details_url": take("details_url").unwrap_or(Value::Null),
        "output": take("output").unwrap_or_else(|| json!({
            "title": Value::Null, "summary": Value::Null, "text": Value::Null,
            "annotations_count": 0,
        })),
        "url": format!("{}/repos/{full_name}/check-runs/{id}", urls.api),
        "html_url": format!("{}/{full_name}/runs/{id}", urls.html),
        "pull_requests": [],
        "app": { "id": 1, "slug": "walgit-github-facade", "name": "walgit github facade" },
        "check_suite": { "id": id },
    })
}

// ---- deployments -------------------------------------------------------------

/// `POST /repos/{o}/{r}/deployments`. The client throws unless `id` is
/// present, so the 202 "auto-merge" body GitHub sometimes sends is never it.
async fn create_deployment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let full_name = format!("{owner}/{name}");
    let mut store = state();
    let id = store.id();
    let field = |key: &str| body.get(key).cloned().unwrap_or(Value::Null);
    let deployment = json!({
        "id": id,
        "node_id": models::node_id("Deployment", &format!("{full_name}:{id}")),
        "sha": field("ref"),
        "ref": field("ref"),
        "task": body.get("task").cloned().unwrap_or_else(|| json!("deploy")),
        "environment": field("environment"),
        "description": field("description"),
        "transient_environment": body
            .get("transient_environment")
            .cloned()
            .unwrap_or(Value::Bool(false)),
        "production_environment": body
            .get("production_environment")
            .cloned()
            .unwrap_or(Value::Bool(false)),
        "payload": {},
        "creator": models::user(&urls),
        "created_at": now(),
        "updated_at": now(),
        "url": format!("{}/repos/{full_name}/deployments/{id}", urls.api),
        "statuses_url": format!("{}/repos/{full_name}/deployments/{id}/statuses", urls.api),
        "repository_url": format!("{}/repos/{full_name}", urls.api),
    });
    store.insert_deployment(id, deployment.clone());
    drop(store);
    Ok((StatusCode::CREATED, Json(deployment)).into_response())
}

#[derive(serde::Deserialize, Default)]
struct DeploymentQuery {
    environment: Option<String>,
    #[serde(rename = "ref")]
    ref_name: Option<String>,
}

async fn list_deployments(
    Path((_owner, _name)): Path<(String, String)>,
    Query(q): Query<DeploymentQuery>,
) -> GhResult<Response> {
    let store = state();
    let out: Vec<Value> = store
        .deployment_order
        .iter()
        .rev()
        .filter_map(|id| store.deployments.get(id))
        .filter(|d| {
            q.environment
                .as_deref()
                .is_none_or(|e| d.get("environment").and_then(Value::as_str) == Some(e))
        })
        .filter(|d| {
            q.ref_name
                .as_deref()
                .is_none_or(|r| d.get("ref").and_then(Value::as_str) == Some(r))
        })
        .cloned()
        .collect();
    drop(store);
    Ok(Json(out).into_response())
}

/// `POST /repos/{o}/{r}/deployments/{id}/statuses` — no field is read back.
async fn create_deployment_status(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, deployment_id)): Path<(String, String, u64)>,
    Json(body): Json<Value>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let full_name = format!("{owner}/{name}");
    let mut store = state();
    let id = store.id();
    drop(store);
    let field = |key: &str| body.get(key).cloned().unwrap_or(Value::Null);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "node_id": models::node_id("DeploymentStatus", &format!("{full_name}:{id}")),
            "state": body.get("state").cloned().unwrap_or_else(|| json!("success")),
            "description": field("description"),
            "environment": field("environment"),
            "environment_url": field("environment_url"),
            "log_url": field("log_url"),
            "target_url": field("log_url"),
            "creator": models::user(&urls),
            "created_at": now(),
            "updated_at": now(),
            "deployment_url": format!("{}/repos/{full_name}/deployments/{deployment_id}", urls.api),
            "repository_url": format!("{}/repos/{full_name}", urls.api),
        })),
    )
        .into_response())
}

async fn list_deployment_statuses(
    Path((_owner, _name, _id)): Path<(String, String, u64)>,
) -> GhResult<Response> {
    Ok(Json(Value::Array(Vec::new())).into_response())
}

// ---- commit statuses ---------------------------------------------------------

/// `POST /repos/{o}/{r}/statuses/{sha}`.
async fn create_status(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> GhResult<Response> {
    let urls = Urls::from_request(&st, &headers);
    let full_name = format!("{owner}/{name}");
    let mut store = state();
    let id = store.id();
    drop(store);
    let field = |key: &str| body.get(key).cloned().unwrap_or(Value::Null);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "node_id": models::node_id("Status", &format!("{full_name}:{id}")),
            "state": body.get("state").cloned().unwrap_or_else(|| json!("success")),
            "context": body.get("context").cloned().unwrap_or_else(|| json!("default")),
            "description": field("description"),
            "target_url": field("target_url"),
            "created_at": now(),
            "updated_at": now(),
            "creator": models::user(&urls),
            "url": format!("{}/repos/{full_name}/statuses/{sha}", urls.api),
        })),
    )
        .into_response())
}

/// `GET /repos/{o}/{r}/commits/{ref}/status` — nothing here ever fails.
pub fn combined_status(urls: &Urls, full_name: &str, sha: &str) -> Response {
    Json(json!({
        "state": "success",
        "sha": sha,
        "total_count": 0,
        "statuses": [],
        "repository": { "full_name": full_name },
        "url": format!("{}/repos/{full_name}/commits/{sha}/status", urls.api),
    }))
    .into_response()
}

/// `GET /repos/{o}/{r}/commits/{ref}/statuses` — the facade runs no checks.
pub fn commit_statuses() -> Response {
    Json(Value::Array(Vec::new())).into_response()
}

/// `GET /repos/{o}/{r}/rules/branches/{branch}` — no rulesets exist, and an
/// empty array is only read as "legacy protection" when the branch also
/// reports `protected: true`, which the facade never does.
async fn branch_rules(
    Path((_owner, _name, _branch)): Path<(String, String, String)>,
) -> GhResult<Response> {
    Ok(Json(Value::Array(Vec::new())).into_response())
}

#[cfg(test)]
mod tests {
    use super::{MAX_ENTRIES, Store};

    #[test]
    fn ids_are_unique_and_js_safe() {
        let mut s = Store::default();
        let a = s.id();
        let b = s.id();
        assert_ne!(a, b);
        assert!(b < (1u64 << 53));
    }

    #[test]
    fn the_check_run_map_is_bounded() {
        let mut s = Store::default();
        for _ in 0..(MAX_ENTRIES + 16) {
            let id = s.id();
            s.insert_check(id, serde_json::json!({}));
        }
        assert_eq!(s.order.len(), MAX_ENTRIES);
        assert_eq!(s.check_runs.len(), MAX_ENTRIES);
    }

    #[test]
    fn a_patch_of_a_known_run_replaces_it_in_place() {
        let mut s = Store::default();
        let id = s.id();
        s.insert_check(id, serde_json::json!({"status": "queued"}));
        s.insert_check(id, serde_json::json!({"status": "completed"}));
        assert_eq!(s.order.len(), 1);
        assert_eq!(
            s.check_runs.get(&id).and_then(|v| v.get("status")),
            Some(&serde_json::json!("completed"))
        );
    }
}
