use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine};
use http::{header, StatusCode};

use super::state::AppState;

/// Validated client ID, injected into request extensions by the auth middleware.
#[derive(Clone)]
pub(super) struct AuthedClientId(pub(super) String);

pub(super) fn authed_client_id(req: &Request) -> Option<String> {
    req.extensions()
        .get::<AuthedClientId>()
        .map(|id| id.0.clone())
}

pub(super) async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let unauthorized = || -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"kdbx-git\"")],
        )
            .into_response()
    };

    let path = req.uri().path().to_owned();
    let client_id = extract_client_id_from_path(&path);

    let client_id = match client_id {
        Some(id) => id,
        None => return unauthorized(),
    };

    let creds = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| B64_STANDARD.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());

    let (username, password) = match creds.as_deref().and_then(|s| s.split_once(':')) {
        Some(pair) => pair,
        None => return unauthorized(),
    };

    let found = state
        .config
        .clients
        .iter()
        .any(|c| c.id == client_id && c.id == username && c.password == password);

    if found {
        req.extensions_mut().insert(AuthedClientId(client_id));
        next.run(req).await
    } else {
        unauthorized()
    }
}

pub(super) fn extract_client_id_from_path(path: &str) -> Option<String> {
    ["/dav/", "/sync/", "/push/"]
        .into_iter()
        .find_map(|prefix| {
            path.strip_prefix(prefix)
                .and_then(|s| s.split('/').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::extract_client_id_from_path;

    #[test]
    fn extracts_client_id_from_supported_paths() {
        assert_eq!(
            extract_client_id_from_path("/dav/alice/database.kdbx"),
            Some("alice".to_string())
        );
        assert_eq!(
            extract_client_id_from_path("/sync/bob/events"),
            Some("bob".to_string())
        );
        assert_eq!(
            extract_client_id_from_path("/push/carol/endpoint"),
            Some("carol".to_string())
        );
        assert_eq!(extract_client_id_from_path("/unknown/alice"), None);
    }
}
