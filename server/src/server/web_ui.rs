use std::path::{Component, Path, PathBuf};

use axum::{
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::{cookie::{Cookie, SameSite}, PrivateCookieJar};
use cookie::time::Duration as CookieDuration;
use kdbx_git_web_ui::{get_asset, index_asset};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::store::MAIN_BRANCH;

use super::state::AppState;

const SESSION_COOKIE_NAME: &str = "kdbx_git_web_ui_session";

pub(super) fn build_web_ui_api_router() -> Router<AppState> {
    Router::new()
        .route("/api/ui/v1/session", get(session_handler))
        .route("/api/ui/v1/session/login", post(login_handler))
        .route("/api/ui/v1/session/logout", post(logout_handler))
        .route("/api/ui/v1/status", get(status_handler))
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    authenticated: bool,
    username: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    authenticated_username: String,
    bind_addr: String,
    git_store: String,
    asset_delivery: &'static str,
    keegate_api_enabled: bool,
    client_count: usize,
    push_endpoint_count: usize,
    main_branch_exists: bool,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
    message: String,
}

async fn session_handler(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
) -> impl IntoResponse {
    Json(SessionResponse {
        authenticated: current_admin_username(&state, &jar).is_some(),
        username: current_admin_username(&state, &jar),
    })
}

async fn login_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: PrivateCookieJar,
    Json(payload): Json<LoginRequest>,
) -> Response {
    let Some(admin_user) = state
        .config
        .web_ui
        .admin_users
        .iter()
        .find(|user| user.username == payload.username)
    else {
        return unauthorized_response("invalid admin username or password");
    };

    if admin_user.password == payload.password {
        let cookie = build_session_cookie(
            &payload.username,
            state.config.web_ui.session_ttl_hours,
            request_is_secure(&headers),
        );
        let jar = jar.add(cookie);
        (
            jar,
            Json(SessionResponse {
                authenticated: true,
                username: Some(payload.username),
            }),
        )
            .into_response()
    } else {
        unauthorized_response("invalid admin username or password")
    }
}

async fn logout_handler(jar: PrivateCookieJar) -> impl IntoResponse {
    let jar = jar.remove(Cookie::build((SESSION_COOKIE_NAME, "")).path("/").build());
    (
        jar,
        Json(SessionResponse {
            authenticated: false,
            username: None,
        }),
    )
}

async fn status_handler(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
) -> Response {
    let Some(username) = current_admin_username(&state, &jar) else {
        return unauthorized_response("admin session required");
    };

    let push_endpoint_count = {
        let push_state = state.push_state.lock().await;
        match push_state.load() {
            Ok(sync_state) => sync_state.push_endpoints.len(),
            Err(err) => {
                warn!("web ui status: failed to load sync state: {err:#}");
                return internal_error_response("failed to load sync state");
            }
        }
    };

    let main_branch_exists = {
        let store = state.store.lock().await;
        match store.branch_tip_id(MAIN_BRANCH.to_string()).await {
            Ok(branch_tip) => branch_tip.is_some(),
            Err(err) => {
                warn!("web ui status: failed to read main branch tip: {err:#}");
                return internal_error_response("failed to read main branch status");
            }
        }
    };

    Json(StatusResponse {
        authenticated_username: username,
        bind_addr: state.config.bind_addr.clone(),
        git_store: state.config.git_store.display().to_string(),
        asset_delivery: "embedded",
        keegate_api_enabled: state.config.keegate_api.enabled,
        client_count: state.config.clients.len(),
        push_endpoint_count,
        main_branch_exists,
    })
    .into_response()
}

pub(super) async fn web_ui_index_handler(State(state): State<AppState>) -> Response {
    let _ = state;
    serve_embedded_asset(index_asset())
}

pub(super) async fn web_ui_asset_handler(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let _ = state;
    let Some(relative_path) = sanitize_relative_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let asset_path = if Path::new(&relative_path).extension().is_some() {
        relative_path.to_string_lossy().replace('\\', "/")
    } else {
        "index.html".to_string()
    };

    match get_asset(&asset_path) {
        Some(asset) => serve_embedded_asset(asset),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn current_admin_username(state: &AppState, jar: &PrivateCookieJar) -> Option<String> {
    let username = jar.get(SESSION_COOKIE_NAME)?.value().to_string();
    state
        .config
        .web_ui
        .admin_users
        .iter()
        .any(|user| user.username == username)
        .then_some(username)
}

fn build_session_cookie(username: &str, session_ttl_hours: u64, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE_NAME, username.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .max_age(CookieDuration::hours(
            i64::try_from(session_ttl_hours).unwrap_or(i64::MAX),
        ))
        .build()
}

fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

fn sanitize_relative_path(path: &str) -> Option<PathBuf> {
    let mut clean = PathBuf::new();

    for component in Path::new(path).components() {
        match component {
            Component::Normal(segment) => clean.push(segment),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    Some(clean)
}

fn serve_embedded_asset(asset: kdbx_git_web_ui::EmbeddedAsset) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type)
        .body(axum::body::Body::from(asset.bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn unauthorized_response(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: "unauthorized",
            message: message.to_string(),
        }),
    )
        .into_response()
}

fn internal_error_response(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "internal_error",
            message: message.to_string(),
        }),
    )
        .into_response()
}
