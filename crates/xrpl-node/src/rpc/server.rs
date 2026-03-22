//! axum HTTP server for JSON-RPC.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::handlers::{handle_rpc, AppState};

/// Create the axum router for the JSON-RPC server.
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", post(rpc_handler))
        .with_state(state)
}

async fn rpc_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let method = body
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let params = body
        .get("params")
        .cloned()
        .unwrap_or(json!([]));

    let result = handle_rpc(method, &params, &state);

    Json(json!({ "result": result }))
}
