use http::StatusCode;
use kdbx_git_keegate_api::{
    EntryPayload, KeeGateApiErrorResponse, KeeGateInfoResponse, QueryEntriesRequest,
    QueryEntriesResponse,
};
use reqwest::{Client as HttpClient, Url};
use serde::{Deserialize, Serialize};

const INFO_PATH: &str = "api/v1/keegate/info";
const QUERY_PATH: &str = "api/v1/keegate/entries/query";
const RESOLVE_QUERY_PATH: &str = "api/v1/keegate/entries/resolve/query";
const RESOLVE_UUID_PATH_PREFIX: &str = "api/v1/keegate/entries/resolve/uuid";
const KEEGATE_SCHEME: &str = "kg";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeeGateClientConfig {
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QueryEntriesGetRequest {
    #[serde(default)]
    pub title_contains: Option<String>,
    #[serde(default)]
    pub title_regex: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct KeeGateClient {
    http: HttpClient,
    base_url: Url,
    username: String,
    password: String,
}

impl KeeGateClient {
    pub fn new(
        base_url: impl AsRef<str>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, KeeGateClientBuildError> {
        Self::with_http_client(HttpClient::new(), base_url, username, password)
    }

    pub fn from_config(config: &KeeGateClientConfig) -> Result<Self, KeeGateClientBuildError> {
        Self::with_http_client_from_config(HttpClient::new(), config)
    }

    pub fn from_connection_string(
        connection_string: impl AsRef<str>,
    ) -> Result<Self, KeeGateClientBuildError> {
        Self::with_http_client_from_connection_string(HttpClient::new(), connection_string)
    }

    pub fn with_http_client(
        http: HttpClient,
        base_url: impl AsRef<str>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, KeeGateClientBuildError> {
        Ok(Self {
            http,
            base_url: normalize_base_url(base_url.as_ref())?,
            username: username.into(),
            password: password.into(),
        })
    }

    pub fn with_http_client_from_config(
        http: HttpClient,
        config: &KeeGateClientConfig,
    ) -> Result<Self, KeeGateClientBuildError> {
        Self::with_http_client_from_connection_string(http, &config.url)
    }

    pub fn with_http_client_from_connection_string(
        http: HttpClient,
        connection_string: impl AsRef<str>,
    ) -> Result<Self, KeeGateClientBuildError> {
        let authority = parse_connection_authority(connection_string.as_ref())?;
        Ok(Self {
            http,
            base_url: authority.base_url,
            username: authority.username,
            password: authority.password,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub async fn info(&self) -> Result<KeeGateInfoResponse, KeeGateClientError> {
        let response = self
            .http
            .get(endpoint_from_base(&self.base_url, INFO_PATH)?)
            .send()
            .await
            .map_err(KeeGateClientError::Transport)?;
        parse_json_response(response).await
    }

    pub async fn query_entries(
        &self,
        request: &QueryEntriesRequest,
    ) -> Result<QueryEntriesResponse, KeeGateClientError> {
        let response = self
            .authed_request(
                self.http
                    .post(endpoint_from_base(&self.base_url, QUERY_PATH)?)
                    .json(request),
            )
            .send()
            .await
            .map_err(KeeGateClientError::Transport)?;
        parse_json_response(response).await
    }

    pub async fn query_entries_get(
        &self,
        request: &QueryEntriesGetRequest,
    ) -> Result<QueryEntriesResponse, KeeGateClientError> {
        let response = self
            .authed_request(
                self.http
                    .get(endpoint_from_base(&self.base_url, QUERY_PATH)?)
                    .query(request),
            )
            .send()
            .await
            .map_err(KeeGateClientError::Transport)?;
        parse_json_response(response).await
    }

    pub async fn resolve(
        &self,
        reference: impl AsRef<str>,
    ) -> Result<QueryEntriesResponse, KeeGateClientError> {
        let request = self.resolve_request(reference.as_ref())?;
        let response = self
            .http
            .get(request.url)
            .basic_auth(request.username, Some(request.password))
            .send()
            .await
            .map_err(KeeGateClientError::Transport)?;
        parse_json_response(response).await
    }

    pub async fn resolve_first(
        &self,
        reference: impl AsRef<str>,
    ) -> Result<Option<EntryPayload>, KeeGateClientError> {
        Ok(self.resolve(reference).await?.entries.into_iter().next())
    }

    fn authed_request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(&self.username, Some(&self.password))
    }

    fn resolve_request(&self, reference: &str) -> Result<ResolvedRequest, KeeGateClientError> {
        let reference = Url::parse(reference).map_err(|err| {
            KeeGateClientError::InvalidReference(reference.to_string(), err.to_string())
        })?;
        if reference.scheme() != KEEGATE_SCHEME {
            return Err(KeeGateClientError::InvalidReference(
                reference.to_string(),
                format!("unsupported scheme '{}'", reference.scheme()),
            ));
        }
        if reference.fragment().is_some() {
            return Err(KeeGateClientError::InvalidReference(
                reference.to_string(),
                "fragments are not supported in KeeGate references".to_string(),
            ));
        }

        let authority = if reference.host_str().is_some() {
            parse_reference_authority(&reference)?
        } else {
            if !reference.username().is_empty() || reference.password().is_some() {
                return Err(KeeGateClientError::InvalidReference(
                    reference.to_string(),
                    "relative KeeGate references cannot include credentials".to_string(),
                ));
            }
            ConnectionAuthority {
                base_url: self.base_url.clone(),
                username: self.username.clone(),
                password: self.password.clone(),
            }
        };

        let mut segments = reference.path_segments().ok_or_else(|| {
            KeeGateClientError::InvalidReference(
                reference.to_string(),
                "invalid KeeGate reference path".to_string(),
            )
        })?;

        let url = match (segments.next(), segments.next(), segments.next()) {
            (Some("uuid"), Some(uuid), None) => endpoint_with_uuid(&authority.base_url, uuid)?,
            (Some("query"), None, None) => {
                let mut url = endpoint_from_base(&authority.base_url, RESOLVE_QUERY_PATH)?;
                url.set_query(reference.query());
                url
            }
            _ => {
                return Err(KeeGateClientError::InvalidReference(
                    reference.to_string(),
                    "expected /uuid/<uuid> or /query?...".to_string(),
                ))
            }
        };

        Ok(ResolvedRequest {
            url,
            username: authority.username,
            password: authority.password,
        })
    }
}

#[derive(Debug)]
pub enum KeeGateClientBuildError {
    InvalidBaseUrl(String),
    InvalidConnectionString(String),
}

impl std::fmt::Display for KeeGateClientBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBaseUrl(err) => write!(f, "invalid KeeGate base URL: {err}"),
            Self::InvalidConnectionString(err) => {
                write!(f, "invalid KeeGate connection string: {err}")
            }
        }
    }
}

impl std::error::Error for KeeGateClientBuildError {}

#[derive(Debug)]
pub enum KeeGateClientError {
    InvalidEndpointUrl(String),
    InvalidReference(String, String),
    Transport(reqwest::Error),
    Decode(reqwest::Error),
    Api {
        status: StatusCode,
        error: Option<KeeGateApiErrorResponse>,
        body: String,
    },
}

impl KeeGateClientError {
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl std::fmt::Display for KeeGateClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpointUrl(err) => {
                write!(f, "failed to build KeeGate endpoint URL: {err}")
            }
            Self::InvalidReference(reference, err) => {
                write!(f, "invalid KeeGate reference '{reference}': {err}")
            }
            Self::Transport(err) => write!(f, "KeeGate request failed: {err}"),
            Self::Decode(err) => write!(f, "failed to decode KeeGate response: {err}"),
            Self::Api { status, error, .. } => {
                if let Some(error) = error {
                    write!(f, "KeeGate API returned {status}: {}", error.message)
                } else {
                    write!(f, "KeeGate API returned {status}")
                }
            }
        }
    }
}

impl std::error::Error for KeeGateClientError {}

#[derive(Debug, Clone)]
struct ConnectionAuthority {
    base_url: Url,
    username: String,
    password: String,
}

#[derive(Debug, Clone)]
struct ResolvedRequest {
    url: Url,
    username: String,
    password: String,
}

fn normalize_base_url(base_url: &str) -> Result<Url, KeeGateClientBuildError> {
    let mut base_url = Url::parse(base_url)
        .map_err(|err| KeeGateClientBuildError::InvalidBaseUrl(err.to_string()))?;
    if !base_url.path().ends_with('/') {
        let mut path = base_url.path().to_string();
        path.push('/');
        base_url.set_path(&path);
    }
    Ok(base_url)
}

fn parse_connection_authority(
    connection_string: &str,
) -> Result<ConnectionAuthority, KeeGateClientBuildError> {
    let connection = Url::parse(connection_string)
        .map_err(|err| KeeGateClientBuildError::InvalidConnectionString(err.to_string()))?;
    if connection.scheme() != KEEGATE_SCHEME {
        return Err(KeeGateClientBuildError::InvalidConnectionString(format!(
            "expected '{KEEGATE_SCHEME}' scheme, got '{}'",
            connection.scheme()
        )));
    }
    if connection.host_str().is_none() {
        return Err(KeeGateClientBuildError::InvalidConnectionString(
            "connection string must include a host".to_string(),
        ));
    }
    if connection.username().is_empty() {
        return Err(KeeGateClientBuildError::InvalidConnectionString(
            "connection string must include a username".to_string(),
        ));
    }
    let Some(password) = connection.password() else {
        return Err(KeeGateClientBuildError::InvalidConnectionString(
            "connection string must include a password".to_string(),
        ));
    };
    if !matches!(connection.path(), "" | "/")
        || connection.query().is_some()
        || connection.fragment().is_some()
    {
        return Err(KeeGateClientBuildError::InvalidConnectionString(
            "connection string must not include a path, query, or fragment".to_string(),
        ));
    }

    Ok(ConnectionAuthority {
        base_url: https_base_url_for(&connection)
            .map_err(KeeGateClientBuildError::InvalidConnectionString)?,
        username: connection.username().to_string(),
        password: password.to_string(),
    })
}

fn parse_reference_authority(reference: &Url) -> Result<ConnectionAuthority, KeeGateClientError> {
    if reference.host_str().is_none() {
        return Err(KeeGateClientError::InvalidReference(
            reference.to_string(),
            "absolute KeeGate references must include a host".to_string(),
        ));
    }
    if reference.username().is_empty() {
        return Err(KeeGateClientError::InvalidReference(
            reference.to_string(),
            "absolute KeeGate references must include a username".to_string(),
        ));
    }
    let Some(password) = reference.password() else {
        return Err(KeeGateClientError::InvalidReference(
            reference.to_string(),
            "absolute KeeGate references must include a password".to_string(),
        ));
    };

    Ok(ConnectionAuthority {
        base_url: https_base_url_for(reference)
            .map_err(|err| KeeGateClientError::InvalidReference(reference.to_string(), err))?,
        username: reference.username().to_string(),
        password: password.to_string(),
    })
}

fn https_base_url_for(url: &Url) -> Result<Url, String> {
    let mut base_url = Url::parse("https://example.invalid/")
        .map_err(|err| format!("failed to build HTTPS base URL: {err}"))?;
    base_url
        .set_host(url.host_str())
        .map_err(|err| format!("failed to set KeeGate host: {err}"))?;
    base_url
        .set_port(url.port())
        .map_err(|()| "failed to set KeeGate port".to_string())?;
    Ok(base_url)
}

fn endpoint_from_base(base_url: &Url, path: &str) -> Result<Url, KeeGateClientError> {
    base_url
        .join(path)
        .map_err(|err| KeeGateClientError::InvalidEndpointUrl(err.to_string()))
}

fn endpoint_with_uuid(base_url: &Url, uuid: &str) -> Result<Url, KeeGateClientError> {
    let mut url = endpoint_from_base(base_url, RESOLVE_UUID_PATH_PREFIX)?;
    {
        let mut segments = url.path_segments_mut().map_err(|()| {
            KeeGateClientError::InvalidEndpointUrl("base URL cannot be a base".to_string())
        })?;
        segments.push(uuid);
    }
    Ok(url)
}

async fn parse_json_response<T>(response: reqwest::Response) -> Result<T, KeeGateClientError>
where
    T: serde::de::DeserializeOwned,
{
    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(KeeGateClientError::Decode);
    }

    let body = response
        .text()
        .await
        .map_err(KeeGateClientError::Transport)?;
    let error = serde_json::from_str(&body).ok();
    Err(KeeGateClientError::Api {
        status,
        error,
        body,
    })
}

#[cfg(test)]
mod tests {
    use axum::{
        extract::{Path, Query, State},
        http::StatusCode,
        response::IntoResponse,
        routing::get,
        Json, Router,
    };
    use kdbx_git_keegate_api::{
        AndFilter, KeeGateApiErrorResponse, KeeGateInfoResponse, OrFilter, QueryEntriesRequest,
        QueryEntriesResponse, QueryFilterRequest, QueryMeta, QueryOptionsRequest, TagFilter,
        TitleContainsFilter,
    };
    use serde::Deserialize;
    use tokio::net::TcpListener;

    use super::{
        EntryPayload, KeeGateClient, KeeGateClientBuildError, KeeGateClientConfig,
        KeeGateClientError, QueryEntriesGetRequest,
    };

    #[derive(Clone)]
    struct MockState {
        expected_user: String,
        expected_password: String,
    }

    #[derive(Debug, Deserialize)]
    struct MockQueryParams {
        title_contains: Option<String>,
        title_regex: Option<String>,
        tag: Option<String>,
        uuid: Option<String>,
        limit: Option<usize>,
    }

    async fn spawn_mock_server() -> String {
        let state = MockState {
            expected_user: "app-user".into(),
            expected_password: "app-password".into(),
        };

        let app = Router::new()
            .route("/api/v1/keegate/info", get(mock_info))
            .route(
                "/api/v1/keegate/entries/query",
                get(mock_query_get).post(mock_query_post),
            )
            .route(
                "/api/v1/keegate/entries/resolve/uuid/{uuid}",
                get(mock_resolve_uuid),
            )
            .route(
                "/api/v1/keegate/entries/resolve/query",
                get(mock_resolve_query),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn mock_info() -> impl IntoResponse {
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

    async fn mock_query_post(
        State(state): State<MockState>,
        auth: Option<
            axum_extra::TypedHeader<
                axum_extra::headers::Authorization<axum_extra::headers::authorization::Basic>,
            >,
        >,
        Json(request): Json<QueryEntriesRequest>,
    ) -> impl IntoResponse {
        if let Some(response) = unauthorized_if_needed(&state, auth) {
            return response;
        }

        let expected_request = QueryEntriesRequest {
            filter: QueryFilterRequest::And(AndFilter {
                and: vec![
                    QueryFilterRequest::Tag(TagFilter { tag: "prod".into() }),
                    QueryFilterRequest::Or(OrFilter {
                        or: vec![QueryFilterRequest::TitleContains(TitleContainsFilter {
                            title_contains: "postgres".into(),
                        })],
                    }),
                ],
            }),
            options: QueryOptionsRequest { limit: Some(25) },
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::to_value(&expected_request).unwrap()
        );

        Json(QueryEntriesResponse {
            entries: vec![],
            meta: QueryMeta {
                count: 0,
                limit: 25,
            },
        })
        .into_response()
    }

    async fn mock_query_get(
        State(state): State<MockState>,
        auth: Option<
            axum_extra::TypedHeader<
                axum_extra::headers::Authorization<axum_extra::headers::authorization::Basic>,
            >,
        >,
        Query(query): Query<MockQueryParams>,
    ) -> impl IntoResponse {
        if let Some(response) = unauthorized_if_needed(&state, auth) {
            return response;
        }

        assert_eq!(query.tag.as_deref(), Some("shared"));
        assert_eq!(query.title_contains.as_deref(), Some("redis"));
        assert_eq!(query.limit, Some(5));
        assert_eq!(query.title_regex, None);
        assert_eq!(query.uuid, None);

        Json(QueryEntriesResponse {
            entries: vec![entry_payload(
                "11111111-1111-1111-1111-111111111111",
                "Shared Redis",
            )],
            meta: QueryMeta { count: 1, limit: 5 },
        })
        .into_response()
    }

    async fn mock_resolve_uuid(
        State(state): State<MockState>,
        auth: Option<
            axum_extra::TypedHeader<
                axum_extra::headers::Authorization<axum_extra::headers::authorization::Basic>,
            >,
        >,
        Path(uuid): Path<String>,
    ) -> impl IntoResponse {
        if let Some(response) = unauthorized_if_needed(&state, auth) {
            return response;
        }

        assert_eq!(uuid, "2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e");
        Json(QueryEntriesResponse {
            entries: vec![entry_payload(&uuid, "Prod Postgres")],
            meta: QueryMeta { count: 1, limit: 1 },
        })
        .into_response()
    }

    async fn mock_resolve_query(
        State(state): State<MockState>,
        auth: Option<
            axum_extra::TypedHeader<
                axum_extra::headers::Authorization<axum_extra::headers::authorization::Basic>,
            >,
        >,
        Query(query): Query<MockQueryParams>,
    ) -> impl IntoResponse {
        if let Some(response) = unauthorized_if_needed(&state, auth) {
            return response;
        }

        assert_eq!(query.tag.as_deref(), Some("prod"));
        assert_eq!(query.title_contains, None);
        assert_eq!(query.limit, None);

        Json(QueryEntriesResponse {
            entries: vec![
                entry_payload("2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e", "Prod Postgres"),
                entry_payload("11111111-1111-1111-1111-111111111111", "Shared Redis"),
            ],
            meta: QueryMeta {
                count: 2,
                limit: 100,
            },
        })
        .into_response()
    }

    fn unauthorized_if_needed(
        state: &MockState,
        auth: Option<
            axum_extra::TypedHeader<
                axum_extra::headers::Authorization<axum_extra::headers::authorization::Basic>,
            >,
        >,
    ) -> Option<axum::response::Response> {
        let Some(axum_extra::TypedHeader(auth)) = auth else {
            return Some(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(KeeGateApiErrorResponse {
                        error: "unauthorized".into(),
                        message: "missing auth".into(),
                    }),
                )
                    .into_response(),
            );
        };

        if auth.username() != state.expected_user || auth.password() != state.expected_password {
            return Some(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(KeeGateApiErrorResponse {
                        error: "unauthorized".into(),
                        message: "bad auth".into(),
                    }),
                )
                    .into_response(),
            );
        }

        None
    }

    fn entry_payload(uuid: &str, title: &str) -> EntryPayload {
        EntryPayload {
            uuid: uuid.into(),
            title: Some(title.into()),
            username: Some("demo-user".into()),
            password: Some("demo-password".into()),
            url: None,
            notes: None,
            tags: vec!["prod".into()],
            group_path: vec!["Applications".into()],
        }
    }

    #[tokio::test]
    async fn info_fetches_public_metadata() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "ignored", "ignored").unwrap();

        let info = client.info().await.unwrap();
        assert_eq!(info.name, "KeeGate API");
        assert_eq!(info.version, "v1");
        assert!(info.read_only);
    }

    #[tokio::test]
    async fn query_entries_sends_basic_auth_and_request_body() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "app-user", "app-password").unwrap();

        let response = client
            .query_entries(&QueryEntriesRequest {
                filter: QueryFilterRequest::And(AndFilter {
                    and: vec![
                        QueryFilterRequest::Tag(TagFilter { tag: "prod".into() }),
                        QueryFilterRequest::Or(OrFilter {
                            or: vec![QueryFilterRequest::TitleContains(TitleContainsFilter {
                                title_contains: "postgres".into(),
                            })],
                        }),
                    ],
                }),
                options: QueryOptionsRequest { limit: Some(25) },
            })
            .await
            .unwrap();

        assert_eq!(response.meta.limit, 25);
    }

    #[tokio::test]
    async fn query_entries_get_sends_basic_auth_and_query_string() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "app-user", "app-password").unwrap();

        let response = client
            .query_entries_get(&QueryEntriesGetRequest {
                tag: Some("shared".into()),
                title_contains: Some("redis".into()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(response.meta.count, 1);
        assert_eq!(response.entries[0].title.as_deref(), Some("Shared Redis"));
    }

    #[tokio::test]
    async fn resolve_uses_relative_uuid_reference() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "app-user", "app-password").unwrap();

        let response = client
            .resolve("kg:///uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e")
            .await
            .unwrap();

        assert_eq!(response.meta.count, 1);
        assert_eq!(response.entries[0].title.as_deref(), Some("Prod Postgres"));
    }

    #[tokio::test]
    async fn resolve_first_uses_relative_query_reference() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "app-user", "app-password").unwrap();

        let entry = client.resolve_first("kg:///query?tag=prod").await.unwrap();

        assert_eq!(entry.unwrap().title.as_deref(), Some("Prod Postgres"));
    }

    #[test]
    fn config_round_trips_via_json() {
        let config = KeeGateClientConfig {
            url: "kg://user:pass@example.com".into(),
        };

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "url": "kg://user:pass@example.com"
            })
        );
        let decoded: KeeGateClientConfig = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn from_config_parses_kg_connection_string() {
        let client = KeeGateClient::from_config(&KeeGateClientConfig {
            url: "kg://user:pass@example.com:8443".into(),
        })
        .unwrap();

        assert_eq!(client.base_url().as_str(), "https://example.com:8443/");
    }

    #[tokio::test]
    async fn query_entries_returns_structured_api_errors() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "wrong", "creds").unwrap();

        let error = client
            .query_entries(&QueryEntriesRequest {
                filter: QueryFilterRequest::Tag(TagFilter { tag: "prod".into() }),
                options: QueryOptionsRequest::default(),
            })
            .await
            .unwrap_err();

        match error {
            KeeGateClientError::Api { status, error, .. } => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(error.unwrap().message, "bad auth");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn new_rejects_invalid_base_url() {
        let error = KeeGateClient::new("://bad-url", "user", "pass").unwrap_err();
        match error {
            KeeGateClientBuildError::InvalidBaseUrl(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn from_connection_string_rejects_missing_password() {
        let error = KeeGateClient::from_connection_string("kg://user@example.com").unwrap_err();
        match error {
            KeeGateClientBuildError::InvalidConnectionString(message) => {
                assert!(message.contains("password"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_rejects_invalid_reference_scheme() {
        let base_url = spawn_mock_server().await;
        let client = KeeGateClient::new(&base_url, "app-user", "app-password").unwrap();

        let error = client
            .resolve("https://example.com/uuid/2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e")
            .await
            .unwrap_err();

        match error {
            KeeGateClientError::InvalidReference(reference, message) => {
                assert!(reference.starts_with("https://example.com"));
                assert!(message.contains("unsupported scheme"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
