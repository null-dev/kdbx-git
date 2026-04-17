use http::StatusCode;
use kdbx_git_keegate_api::{
    KeeGateApiErrorResponse, KeeGateInfoResponse, QueryEntriesRequest, QueryEntriesResponse,
};
use reqwest::{Client as HttpClient, Url};

const INFO_PATH: &str = "api/v1/keegate/info";
const QUERY_PATH: &str = "api/v1/keegate/entries/query";

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

    pub fn with_http_client(
        http: HttpClient,
        base_url: impl AsRef<str>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, KeeGateClientBuildError> {
        let mut base_url = Url::parse(base_url.as_ref())
            .map_err(|err| KeeGateClientBuildError::InvalidBaseUrl(err.to_string()))?;
        if !base_url.path().ends_with('/') {
            let mut path = base_url.path().to_string();
            path.push('/');
            base_url.set_path(&path);
        }

        Ok(Self {
            http,
            base_url,
            username: username.into(),
            password: password.into(),
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub async fn info(&self) -> Result<KeeGateInfoResponse, KeeGateClientError> {
        let url = self.endpoint(INFO_PATH)?;
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(KeeGateClientError::Transport)?;
        parse_json_response(response).await
    }

    pub async fn query_entries(
        &self,
        request: &QueryEntriesRequest,
    ) -> Result<QueryEntriesResponse, KeeGateClientError> {
        let url = self.endpoint(QUERY_PATH)?;
        let response = self
            .http
            .post(url)
            .basic_auth(&self.username, Some(&self.password))
            .json(request)
            .send()
            .await
            .map_err(KeeGateClientError::Transport)?;
        parse_json_response(response).await
    }

    fn endpoint(&self, path: &str) -> Result<Url, KeeGateClientError> {
        self.base_url
            .join(path)
            .map_err(|err| KeeGateClientError::InvalidEndpointUrl(err.to_string()))
    }
}

#[derive(Debug)]
pub enum KeeGateClientBuildError {
    InvalidBaseUrl(String),
}

impl std::fmt::Display for KeeGateClientBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBaseUrl(err) => write!(f, "invalid KeeGate base URL: {err}"),
        }
    }
}

impl std::error::Error for KeeGateClientBuildError {}

#[derive(Debug)]
pub enum KeeGateClientError {
    InvalidEndpointUrl(String),
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
        extract::State,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use kdbx_git_keegate_api::{
        AndFilter, KeeGateApiErrorResponse, KeeGateInfoResponse, OrFilter, QueryEntriesRequest,
        QueryEntriesResponse, QueryFilterRequest, QueryMeta, QueryOptionsRequest, TagFilter,
        TitleContainsFilter,
    };
    use tokio::net::TcpListener;

    use super::{KeeGateClient, KeeGateClientBuildError, KeeGateClientError};

    #[derive(Clone)]
    struct MockState {
        expected_user: String,
        expected_password: String,
    }

    async fn spawn_mock_server() -> String {
        let state = MockState {
            expected_user: "app-user".into(),
            expected_password: "app-password".into(),
        };

        let app = Router::new()
            .route("/api/v1/keegate/info", get(mock_info))
            .route("/api/v1/keegate/entries/query", post(mock_query))
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

    async fn mock_query(
        State(state): State<MockState>,
        auth: Option<
            axum_extra::TypedHeader<
                axum_extra::headers::Authorization<axum_extra::headers::authorization::Basic>,
            >,
        >,
        Json(request): Json<QueryEntriesRequest>,
    ) -> impl IntoResponse {
        let Some(axum_extra::TypedHeader(auth)) = auth else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(KeeGateApiErrorResponse {
                    error: "unauthorized".into(),
                    message: "missing auth".into(),
                }),
            )
                .into_response();
        };

        if auth.username() != state.expected_user || auth.password() != state.expected_password {
            return (
                StatusCode::UNAUTHORIZED,
                Json(KeeGateApiErrorResponse {
                    error: "unauthorized".into(),
                    message: "bad auth".into(),
                }),
            )
                .into_response();
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
    fn rejects_invalid_base_url() {
        let error = KeeGateClient::new("://bad-url", "user", "pass").unwrap_err();
        match error {
            KeeGateClientBuildError::InvalidBaseUrl(_) => {}
        }
    }
}
