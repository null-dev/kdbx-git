use std::{convert::Infallible, sync::Arc};

use axum::{
    extract::{Json, Path, Query, Request, State},
    middleware::{self},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{any, get, post},
    Extension, Router,
};
use dav_server::{fakels::FakeLs, DavHandler};
use eyre::{Context, Result};
use futures_util::{stream, StreamExt};
use gix::ObjectId;
use http::{header, StatusCode};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::{
    config::Config,
    kdbx::build_kdbx_sync,
    store::{BranchConflictError, GitStore, MAIN_BRANCH},
};

use super::{
    auth::{auth_middleware, authed_client_id, AuthedClientId},
    dav::KdbxFs,
    state::AppState,
};

pub(super) async fn dav_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let client_id = match authed_client_id(&req) {
        Some(id) => id,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    {
        let store = state.store.lock().await;
        if let Err(e) = store.ensure_client_branch(client_id.clone()).await {
            warn!("Failed to ensure branch for '{}': {e:#}", client_id);
        }
    }

    let prefix = format!("/dav/{client_id}");
    let fs = KdbxFs::new(state, client_id);
    let dav = DavHandler::builder()
        .filesystem(fs)
        .locksystem(FakeLs::new())
        .autoindex(true)
        .strip_prefix(prefix)
        .build_handler();

    dav.handle(req).await.into_response()
}

pub(super) async fn sync_events_handler(
    State(state): State<AppState>,
    req: Request,
) -> impl IntoResponse {
    let client_id = match authed_client_id(&req) {
        Some(id) => id,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    {
        let store = state.store.lock().await;
        if let Err(e) = store.ensure_client_branch(client_id.clone()).await {
            warn!("Failed to ensure branch for '{}': {e:#}", client_id);
        }
    }

    let Some(receiver) = state.subscribe_branch_notifications(MAIN_BRANCH) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let initial =
        stream::once(async { Ok::<Event, Infallible>(Event::default().event("ready").data("0")) });
    let updates = stream::unfold(receiver, |mut receiver| async move {
        match receiver.changed().await {
            Ok(()) => {
                let version = *receiver.borrow_and_update();
                Some((
                    Ok::<Event, Infallible>(
                        Event::default()
                            .event("branch-updated")
                            .data(version.to_string()),
                    ),
                    receiver,
                ))
            }
            Err(_) => None,
        }
    });

    Sse::new(initial.chain(updates))
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Deserialize)]
pub(super) struct PromoteMergePathParams {
    #[allow(dead_code)]
    client_id: String,
    commit_id: String,
}

#[derive(Deserialize)]
pub(super) struct PromoteMergeQuery {
    #[serde(rename = "expected-tip")]
    expected_tip: String,
}

pub(super) async fn sync_merge_from_main_handler(
    State(state): State<AppState>,
    req: Request,
) -> impl IntoResponse {
    let client_id = match authed_client_id(&req) {
        Some(id) => id,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let result = {
        let store = state.store.lock().await;
        store.create_sync_merge_commit(client_id.clone()).await
    };

    match result {
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(merge_result)) => {
            let config = Arc::clone(&state.config);
            let commit_id = merge_result.commit_id.to_hex().to_string();
            let expected_tip = match merge_result.expected_branch_tip {
                Some(id) => id.to_hex().to_string(),
                None => "none".to_string(),
            };
            let storage = merge_result.storage;

            match spawn_blocking(move || build_kdbx_sync(&storage, &config.database)).await {
                Ok(Ok(bytes)) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header("X-Merge-Commit-Id", &commit_id)
                    .header("X-Expected-Branch-Tip", &expected_tip)
                    .body(axum::body::Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                Ok(Err(e)) => {
                    warn!(
                        "sync merge-from-main: failed to build KDBX for '{}': {e:#}",
                        client_id
                    );
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Err(e) => {
            warn!("sync merge-from-main: failed for '{}': {e:#}", client_id);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(super) async fn sync_promote_merge_handler(
    State(state): State<AppState>,
    Path(PromoteMergePathParams {
        commit_id: commit_id_str,
        ..
    }): Path<PromoteMergePathParams>,
    Query(query): Query<PromoteMergeQuery>,
    req: Request,
) -> impl IntoResponse {
    let client_id = match authed_client_id(&req) {
        Some(id) => id,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let commit_id = match ObjectId::from_hex(commit_id_str.as_bytes()) {
        Ok(id) => id,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let expected_branch_tip: Option<ObjectId> = if query.expected_tip == "none" {
        None
    } else {
        match ObjectId::from_hex(query.expected_tip.as_bytes()) {
            Ok(id) => Some(id),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    };

    let result = {
        let store = state.store.lock().await;
        store
            .promote_sync_merge_commit(client_id.clone(), commit_id, expected_branch_tip)
            .await
    };

    match result {
        Ok(()) => {
            state.notify_branches([&client_id]);
            StatusCode::OK.into_response()
        }
        Err(e) if e.downcast_ref::<BranchConflictError>().is_some() => {
            warn!(
                "sync promote-merge: branch conflict for '{}': {e:#}",
                client_id
            );
            StatusCode::CONFLICT.into_response()
        }
        Err(e) => {
            warn!("sync promote-merge: failed for '{}': {e:#}", client_id);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
pub(super) struct RegisterPushEndpointRequest {
    endpoint: String,
}

#[derive(Serialize)]
pub(super) struct VapidPublicKeyResponse {
    public_key: String,
}

pub(super) async fn get_vapid_public_key_handler(
    State(state): State<AppState>,
    req: Request,
) -> impl IntoResponse {
    if authed_client_id(&req).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(VapidPublicKeyResponse {
        public_key: state.vapid_public_key().to_string(),
    })
    .into_response()
}

pub(super) async fn register_push_endpoint_handler(
    State(state): State<AppState>,
    Extension(AuthedClientId(client_id)): Extension<AuthedClientId>,
    Json(payload): Json<RegisterPushEndpointRequest>,
) -> impl IntoResponse {
    match Url::parse(&payload.endpoint) {
        Ok(url) if url.scheme() == "https" => {}
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }

    match state
        .upsert_push_endpoint(&client_id, payload.endpoint)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            warn!(
                "push register endpoint: failed for '{}': {err:#}",
                client_id
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(super) async fn delete_push_endpoint_handler(
    State(state): State<AppState>,
    Extension(AuthedClientId(client_id)): Extension<AuthedClientId>,
) -> impl IntoResponse {
    match state.remove_push_endpoint(&client_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            warn!("push delete endpoint: failed for '{}': {err:#}", client_id);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/dav/{*path}", any(dav_handler))
        .route("/dav", any(dav_handler))
        .route("/dav/", any(dav_handler))
        .route("/sync/{client_id}/events", get(sync_events_handler))
        .route(
            "/sync/{client_id}/merge-from-main",
            post(sync_merge_from_main_handler),
        )
        .route(
            "/sync/{client_id}/promote-merge/{commit_id}",
            post(sync_promote_merge_handler),
        )
        .route(
            "/push/{client_id}/endpoint",
            post(register_push_endpoint_handler).delete(delete_push_endpoint_handler),
        )
        .route(
            "/push/{client_id}/vapid-public-key",
            get(get_vapid_public_key_handler),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

pub async fn serve_listener(listener: tokio::net::TcpListener, state: AppState) -> Result<()> {
    axum::serve(listener, build_app(state))
        .await
        .wrap_err("server error")?;

    Ok(())
}

pub async fn run_server(config: Config, store: GitStore) -> Result<()> {
    let state = AppState::new(config, store)?;
    let bind_addr = state.config.bind_addr.clone();

    info!("Listening on http://{bind_addr}");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .wrap_err_with(|| format!("failed to bind to {bind_addr}"))?;

    serve_listener(listener, state).await
}
