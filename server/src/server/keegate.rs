use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_extra::{
    headers::{authorization::Basic, Authorization},
    TypedHeader,
};
use eyre::{eyre, Result};
use kdbx_git_keegate_api::{
    authenticate, query_entries, startup_warnings, AuthError, KeeGateApiErrorResponse,
    KeeGateInfoResponse, QueryEntriesRequest, BASIC_AUTH_REALM,
};
use tracing::warn;

use crate::store::MAIN_BRANCH;

use super::state::AppState;

pub(super) fn build_keegate_router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/keegate/info", get(info_handler))
        .route("/api/v1/keegate/entries/query", post(query_entries_handler))
}

pub(super) async fn log_startup_warnings(state: &AppState) {
    let read_result = {
        let store = state.store.lock().await;
        store.read_branch(MAIN_BRANCH.to_string()).await
    };

    match read_result {
        Ok(Some(db)) => {
            for message in startup_warnings(&db) {
                warn!("{message}");
            }
        }
        Ok(None) => {}
        Err(err) => warn!("Failed to validate KeeGate API users at startup: {err:#}"),
    }
}

async fn info_handler() -> impl IntoResponse {
    Json(KeeGateInfoResponse {
        name: "KeeGate API".into(),
        version: "v1".into(),
        read_only: true,
        authentication: "basic".into(),
        query_features: vec![
            "title_contains".into(),
            "title_regex".into(),
            "tag".into(),
            "uuid".into(),
            "and".into(),
            "or".into(),
        ],
    })
}

async fn query_entries_handler(
    State(state): State<AppState>,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    body: Bytes,
) -> Response {
    let Some(TypedHeader(auth)) = auth else {
        return unauthorized_response();
    };
    let username = auth.username().to_owned();
    let password = auth.password().to_owned();

    let request: QueryEntriesRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return bad_request_response("invalid_request", format!("invalid JSON body: {err}"));
        }
    };

    let validated_query = match request.validate() {
        Ok(query) => query,
        Err(err) => return bad_request_response("invalid_request", err.message()),
    };

    let db = match load_main_database(&state).await {
        Ok(db) => db,
        Err(err) => {
            warn!("KeeGate API failed to load main database: {err:#}");
            return internal_error_response("failed to evaluate KeeGate query");
        }
    };

    let user = match authenticate(&db, &username, &password) {
        Ok(user) => user,
        Err(AuthError::AmbiguousUsername) => {
            warn!("KeeGate API username '{username}' is ambiguous");
            return unauthorized_response();
        }
        Err(AuthError::InvalidCredentials) => return unauthorized_response(),
    };

    Json(query_entries(&db, &user, &validated_query)).into_response()
}

async fn load_main_database(
    state: &AppState,
) -> Result<kdbx_git_common::storage::types::StorageDatabase> {
    let store = state.store.lock().await;
    let db = store
        .read_branch(MAIN_BRANCH.to_string())
        .await?
        .ok_or_else(|| eyre!("main branch does not exist"))?;
    Ok(db)
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, basic_auth_challenge())],
    )
        .into_response()
}

fn basic_auth_challenge() -> String {
    format!("Basic realm=\"{BASIC_AUTH_REALM}\"")
}

fn bad_request_response(error: &'static str, message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(KeeGateApiErrorResponse {
            error: error.to_string(),
            message,
        }),
    )
        .into_response()
}

fn internal_error_response(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(KeeGateApiErrorResponse {
            error: "internal_error".to_string(),
            message: message.to_string(),
        }),
    )
        .into_response()
}
