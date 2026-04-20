use axum::{
    body::Bytes,
    extract::{rejection::QueryRejection, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use axum_extra::{
    headers::{authorization::Basic, Authorization},
    TypedHeader,
};
use eyre::{eyre, Result};
use kdbx_git_keegate_api::{
    authenticate, query_entries, startup_warnings, AndFilter, AuthError, AuthenticatedUser,
    KeeGateApiErrorResponse, KeeGateInfoResponse, QueryEntriesRequest, QueryEntriesResponse,
    QueryFilterRequest, QueryOptionsRequest, TagFilter, TitleContainsFilter, TitleRegexFilter,
    UuidFilter, ValidatedQuery, BASIC_AUTH_REALM,
};
use serde::Deserialize;
use tracing::warn;

use crate::store::MAIN_BRANCH;

use super::state::AppState;

pub(super) fn build_keegate_router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/keegate/info", get(info_handler))
        .route(
            "/api/v1/keegate/entries/resolve/uuid/{uuid}",
            get(resolve_uuid_handler),
        )
        .route(
            "/api/v1/keegate/entries/resolve/query",
            get(resolve_query_handler),
        )
        .route(
            "/api/v1/keegate/entries/query",
            get(query_entries_get_handler).post(query_entries_post_handler),
        )
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

async fn query_entries_post_handler(
    State(state): State<AppState>,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    body: Bytes,
) -> Response {
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

    match execute_query(&state, auth, validated_query).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

async fn query_entries_get_handler(
    State(state): State<AppState>,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    query: Result<Query<QueryRequestParams>, QueryRejection>,
) -> Response {
    execute_query_request(&state, auth, query).await
}

async fn resolve_uuid_handler(
    State(state): State<AppState>,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    Path(uuid): Path<String>,
) -> Response {
    let validated_query = match (QueryEntriesRequest {
        filter: QueryFilterRequest::Uuid(UuidFilter { uuid }),
        options: QueryOptionsRequest { limit: Some(1) },
    })
    .validate()
    {
        Ok(query) => query,
        Err(err) => return bad_request_response("invalid_request", err.message()),
    };

    let response = match execute_query(&state, auth, validated_query).await {
        Ok(response) => response,
        Err(response) => return response,
    };

    if response.entries.is_empty() {
        return not_found_response("no accessible KeeGate entry matched the requested UUID");
    }

    Json(response).into_response()
}

async fn resolve_query_handler(
    State(state): State<AppState>,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    query: Result<Query<QueryRequestParams>, QueryRejection>,
) -> Response {
    execute_query_request(&state, auth, query).await
}

async fn execute_query_request(
    state: &AppState,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    query: Result<Query<QueryRequestParams>, QueryRejection>,
) -> Response {
    let Query(params) = match query {
        Ok(params) => params,
        Err(err) => {
            return bad_request_response(
                "invalid_request",
                format!("invalid query parameters: {}", err.body_text()),
            );
        }
    };

    let request = match params.into_request() {
        Ok(request) => request,
        Err(message) => return bad_request_response("invalid_request", message),
    };

    let validated_query = match request.validate() {
        Ok(query) => query,
        Err(err) => return bad_request_response("invalid_request", err.message()),
    };

    match execute_query(&state, auth, validated_query).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRequestParams {
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    title_regex: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl QueryRequestParams {
    fn into_request(self) -> std::result::Result<QueryEntriesRequest, String> {
        let mut filters = Vec::new();

        if let Some(title_contains) = self.title_contains {
            filters.push(QueryFilterRequest::TitleContains(TitleContainsFilter {
                title_contains,
            }));
        }
        if let Some(title_regex) = self.title_regex {
            filters.push(QueryFilterRequest::TitleRegex(TitleRegexFilter {
                title_regex,
            }));
        }
        if let Some(tag) = self.tag {
            filters.push(QueryFilterRequest::Tag(TagFilter { tag }));
        }
        if let Some(uuid) = self.uuid {
            filters.push(QueryFilterRequest::Uuid(UuidFilter { uuid }));
        }

        let filter = match filters.len() {
            0 => {
                return Err(
                    "query requires at least one of: title_contains, title_regex, tag, uuid"
                        .to_string(),
                )
            }
            1 => filters.pop().expect("single filter must exist"),
            _ => QueryFilterRequest::And(AndFilter { and: filters }),
        };

        Ok(QueryEntriesRequest {
            filter,
            options: QueryOptionsRequest { limit: self.limit },
        })
    }
}

async fn execute_query(
    state: &AppState,
    auth: Option<TypedHeader<Authorization<Basic>>>,
    validated_query: ValidatedQuery,
) -> std::result::Result<QueryEntriesResponse, Response> {
    let (db, user) = authenticate_request(state, auth).await?;
    Ok(query_entries(&db, &user, &validated_query))
}

async fn authenticate_request(
    state: &AppState,
    auth: Option<TypedHeader<Authorization<Basic>>>,
) -> std::result::Result<
    (
        kdbx_git_common::storage::types::StorageDatabase,
        AuthenticatedUser,
    ),
    Response,
> {
    let Some(TypedHeader(auth)) = auth else {
        return Err(unauthorized_response());
    };
    let username = auth.username().to_owned();
    let password = auth.password().to_owned();

    let db = match load_main_database(state).await {
        Ok(db) => db,
        Err(err) => {
            warn!("KeeGate API failed to load main database: {err:#}");
            return Err(internal_error_response("failed to evaluate KeeGate query"));
        }
    };

    let user = match authenticate(&db, &username, &password) {
        Ok(user) => user,
        Err(AuthError::AmbiguousUsername) => {
            warn!("KeeGate API username '{username}' is ambiguous");
            return Err(unauthorized_response());
        }
        Err(AuthError::InvalidCredentials) => return Err(unauthorized_response()),
    };

    Ok((db, user))
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

fn not_found_response(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(KeeGateApiErrorResponse {
            error: "not_found".to_string(),
            message: message.to_string(),
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
