//! Health/readiness endpoints.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::AppState;

/// Build identity of this binary (commit short sha; `WALGIT_BUILD_SHA` at build time).
/// Exposed by both health endpoints so operators can verify the running artifact.
pub const BUILD_SHA: &str = env!("WALGIT_BUILD_SHA");

pub async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "version": BUILD_SHA}))
}

/// 200 once startup prewarm (`cache.prewarm`) finished or
/// `cache.prewarm_ready_timeout` elapsed; 503 (with what is pending) before.
pub async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let r = &state.readiness;
    let pending = r.pending.load(std::sync::atomic::Ordering::Acquire);
    // Draining after SIGTERM: tell the edge/LB to stop routing here at once
    // (in-flight work finishes; new object work is refused with Retry-After).
    if walgit_wal::tasks::shutting_down() {
        return (StatusCode::SERVICE_UNAVAILABLE, [(axum::http::header::RETRY_AFTER, "15")], Json(json!({"status": "draining", "version": BUILD_SHA, "running": state.registry.tasks().running_all().len(), "instance": crate::instance::info(&state.cfg)}))).into_response();
    }
    if r.ready(state.cfg.cache.prewarm_ready_timeout) {
        // Placement is a liveness fact: deployment verification should assert
        // that each important repository is served by at least one ready host.
        // Return rules, not repository lists; /readyz remains open for probes.
        let p = &state.cfg.placement;
        return Json(json!({"status": "ready", "version": BUILD_SHA, "prewarm_pending": pending, "instance": crate::instance::info(&state.cfg),
            "placement": {"serve": p.serve, "serve_exclude": p.serve_exclude, "maintain": p.maintain, "maintain_exclude": p.maintain_exclude}})).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        // Unauthenticated (startup probe): counts only, no repo names.
        Json(json!({"status": "warming", "version": BUILD_SHA, "prewarm_pending": pending, "running": state.registry.tasks().running_all().len(), "instance": crate::instance::info(&state.cfg)})),
    )
        .into_response()
}
