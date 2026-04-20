use std::{future::Future, pin::Pin, time::Duration};

use chrono::Utc;
use eyre::Result;
use reqwest::{Client, StatusCode};
use tokio::time::timeout;
use tracing::{debug, warn};
use web_push::{
    request_builder, ContentEncoding, SubscriptionInfo, Urgency, VapidSignatureBuilder,
    WebPushMessageBuilder,
};

use crate::sync_state::{RevokedPushEndpoint, VapidKeys};

use super::state::AppState;

const PUSH_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
const PUSH_DELIVERY_TTL_SECS: u32 = 30;
const PUSH_TOPIC: &str = "branch-updated";
const PUSH_NOTIFICATION_PAYLOAD: &[u8] = br#"{"event":"branch-updated"}"#;
const PUSH_ERROR_BODY_LOG_LIMIT: usize = 512;

pub(super) type PushDeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = PushDeliveryResult> + Send + 'a>>;

pub(super) enum PushDeliveryResult {
    Delivered,
    Revoked {
        status: StatusCode,
        body: Option<String>,
    },
    Failed(PushDeliveryError),
}

#[derive(Debug)]
pub(super) struct PushDeliveryError(String);

impl std::fmt::Display for PushDeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PushDeliveryError {}

pub(super) trait PushDelivery: Send + Sync {
    fn post_branch_updated<'a>(
        &'a self,
        subscription: &'a SubscriptionInfo,
        vapid: &'a VapidKeys,
    ) -> PushDeliveryFuture<'a>;
}

#[derive(Clone)]
pub(super) struct ReqwestPushDelivery {
    client: Client,
}

impl ReqwestPushDelivery {
    pub(super) fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(PUSH_DELIVERY_TIMEOUT)
                .build()
                .expect("failed to build push delivery HTTP client"),
        }
    }
}

impl PushDelivery for ReqwestPushDelivery {
    fn post_branch_updated<'a>(
        &'a self,
        subscription: &'a SubscriptionInfo,
        vapid: &'a VapidKeys,
    ) -> PushDeliveryFuture<'a> {
        Box::pin(async move {
            let signature =
                match VapidSignatureBuilder::from_base64(&vapid.private_key, subscription) {
                    Ok(builder) => match builder.build() {
                        Ok(signature) => signature,
                        Err(err) => {
                            return PushDeliveryResult::Failed(PushDeliveryError(err.to_string()));
                        }
                    },
                    Err(err) => {
                        return PushDeliveryResult::Failed(PushDeliveryError(err.to_string()))
                    }
                };

            let mut builder = WebPushMessageBuilder::new(subscription);
            builder.set_ttl(PUSH_DELIVERY_TTL_SECS);
            builder.set_urgency(Urgency::Low);
            builder.set_topic(PUSH_TOPIC.to_string());
            builder.set_vapid_signature(signature);
            builder.set_payload(ContentEncoding::Aes128Gcm, PUSH_NOTIFICATION_PAYLOAD);

            let message = match builder.build() {
                Ok(message) => message,
                Err(err) => return PushDeliveryResult::Failed(PushDeliveryError(err.to_string())),
            };

            let request = request_builder::build_request::<Vec<u8>>(message);
            let uri = request.uri().to_string();
            let mut reqwest_request = self.client.post(uri);
            for (name, value) in request.headers() {
                reqwest_request = reqwest_request.header(name.as_str(), value.as_bytes());
            }

            match timeout(
                PUSH_DELIVERY_TIMEOUT,
                reqwest_request.body(request.into_body()).send(),
            )
            .await
            {
                Ok(Ok(response)) if response.status().is_success() => PushDeliveryResult::Delivered,
                Ok(Ok(response))
                    if matches!(response.status(), StatusCode::NOT_FOUND | StatusCode::GONE) =>
                {
                    let status = response.status();
                    let body = match response.text().await {
                        Ok(text) => trim_log_body(text),
                        Err(err) => Some(format!("<failed to read response body: {err}>")),
                    };
                    PushDeliveryResult::Revoked { status, body }
                }
                Ok(Ok(response)) => PushDeliveryResult::Failed(PushDeliveryError(format!(
                    "unexpected status {}",
                    response.status()
                ))),
                Ok(Err(err)) => PushDeliveryResult::Failed(PushDeliveryError(err.to_string())),
                Err(_) => PushDeliveryResult::Failed(PushDeliveryError("timed out".into())),
            }
        })
    }
}

impl AppState {
    pub(crate) async fn upsert_push_endpoint(
        &self,
        client_id: &str,
        subscription: SubscriptionInfo,
    ) -> Result<()> {
        let store = self.push_state.lock().await;
        store.upsert_push_endpoint(client_id, subscription, Utc::now())
    }

    pub(crate) async fn remove_push_endpoint(&self, client_id: &str) -> Result<()> {
        let store = self.push_state.lock().await;
        store.remove_push_endpoint(client_id, Utc::now())
    }

    pub(crate) async fn load_push_delivery_targets(
        &self,
    ) -> Result<Vec<(String, SubscriptionInfo)>> {
        let store = self.push_state.lock().await;
        let state = store.load()?;
        Ok(state
            .push_endpoints
            .into_iter()
            .map(|(client_id, entry)| (client_id, entry.subscription_info()))
            .collect())
    }

    pub(crate) async fn remove_revoked_push_endpoints(
        &self,
        revoked: &[RevokedPushEndpoint],
    ) -> Result<()> {
        let store = self.push_state.lock().await;
        store.remove_revoked_push_endpoints(revoked, Utc::now())
    }

    pub(crate) async fn dispatch_push_notifications(&self) -> Result<()> {
        let targets = self.load_push_delivery_targets().await?;
        if targets.is_empty() {
            return Ok(());
        }

        let mut revoked = Vec::new();
        for (client_id, subscription) in targets {
            match self
                .push_delivery
                .post_branch_updated(&subscription, &self.vapid_keys)
                .await
            {
                PushDeliveryResult::Revoked { status, body } => {
                    let body_suffix = body
                        .as_deref()
                        .map(|body| format!("; response body: {body}"))
                        .unwrap_or_default();
                    warn!(
                        "push delivery to '{}' for client '{}' returned {}; removing revoked subscription{}",
                        subscription.endpoint, client_id, status, body_suffix
                    );
                    revoked.push(RevokedPushEndpoint {
                        client_id,
                        subscription,
                    });
                }
                PushDeliveryResult::Delivered => {
                    debug!(
                        "push delivery to '{}' for client '{}' succeeded",
                        subscription.endpoint, client_id
                    );
                }
                PushDeliveryResult::Failed(err) => {
                    warn!(
                        "push delivery to '{}' for client '{}' failed: {}",
                        subscription.endpoint, client_id, err
                    );
                }
            }
        }

        if !revoked.is_empty() {
            self.remove_revoked_push_endpoints(&revoked).await?;
        }

        Ok(())
    }
}

fn trim_log_body(body: String) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut chars = trimmed.chars();
    let mut truncated = String::new();
    for _ in 0..PUSH_ERROR_BODY_LOG_LIMIT {
        let Some(ch) = chars.next() else {
            return Some(trimmed.to_string());
        };
        truncated.push(ch);
    }

    if chars.next().is_some() {
        truncated.push_str("...");
    }

    Some(truncated)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use http::StatusCode;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };
    use web_push::SubscriptionKeys;

    use crate::{
        config::{Config, DatabaseCredentials, KeeGateApiConfig},
        store::GitStore,
        sync_state::SyncStateStore,
    };

    use super::*;

    fn sample_subscription(endpoint: &str) -> SubscriptionInfo {
        SubscriptionInfo {
            endpoint: endpoint.into(),
            keys: SubscriptionKeys {
                p256dh:
                    "BGa4N1PI79lboMR_YrwCiCsgp35DRvedt7opHcf0yM3iOBTSoQYqQLwWxAfRKE6tsDnReWmhsImkhDF_DBdkNSU"
                        .into(),
                auth: "EvcWjEgzr4rbvhfi3yds0A".into(),
            },
        }
    }

    #[derive(Clone)]
    struct FakePushDelivery {
        statuses: Arc<Mutex<HashMap<String, StatusCode>>>,
        hits: Arc<Mutex<Vec<String>>>,
    }

    impl FakePushDelivery {
        fn new(statuses: impl IntoIterator<Item = (String, StatusCode)>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(statuses.into_iter().collect())),
                hits: Arc::new(Mutex::new(Vec::new())),
            }
        }

        async fn hits(&self) -> Vec<String> {
            self.hits.lock().await.clone()
        }
    }

    impl PushDelivery for FakePushDelivery {
        fn post_branch_updated<'a>(
            &'a self,
            subscription: &'a SubscriptionInfo,
            _vapid: &'a VapidKeys,
        ) -> PushDeliveryFuture<'a> {
            Box::pin(async move {
                self.hits.lock().await.push(subscription.endpoint.clone());
                match self
                    .statuses
                    .lock()
                    .await
                    .get(&subscription.endpoint)
                    .copied()
                    .unwrap_or(StatusCode::OK)
                {
                    status @ (StatusCode::NOT_FOUND | StatusCode::GONE) => {
                        PushDeliveryResult::Revoked { status, body: None }
                    }
                    status if status.is_success() => PushDeliveryResult::Delivered,
                    status => PushDeliveryResult::Failed(PushDeliveryError(format!(
                        "unexpected status {status}"
                    ))),
                }
            })
        }
    }

    #[tokio::test]
    async fn dispatch_push_notifications_removes_404_and_410_endpoints() {
        let tempdir = TempDir::new().unwrap();
        let config = Config {
            git_store: tempdir.path().join("store.git"),
            sync_state_path: None,
            bind_addr: "127.0.0.1:0".into(),
            database: DatabaseCredentials {
                password: Some("test-password".into()),
                keyfile: None,
            },
            keegate_api: KeeGateApiConfig::default(),
            web_ui: crate::config::WebUiConfig::default(),
            clients: vec![],
        };
        let store = GitStore::open_or_init(&config.git_store).unwrap();
        let delivery = Arc::new(FakePushDelivery::new([
            ("https://push.example/alice".into(), StatusCode::OK),
            ("https://push.example/bob".into(), StatusCode::NOT_FOUND),
            ("https://push.example/carol".into(), StatusCode::GONE),
        ]));
        let state =
            AppState::new_with_push_delivery(config.clone(), store, delivery.clone()).unwrap();
        let now = Utc::now();

        {
            let push_state = state.push_state.lock().await;
            push_state
                .upsert_push_endpoint(
                    "alice",
                    sample_subscription("https://push.example/alice"),
                    now,
                )
                .unwrap();
            push_state
                .upsert_push_endpoint("bob", sample_subscription("https://push.example/bob"), now)
                .unwrap();
            push_state
                .upsert_push_endpoint(
                    "carol",
                    sample_subscription("https://push.example/carol"),
                    now,
                )
                .unwrap();
        }

        state.dispatch_push_notifications().await.unwrap();

        let hits = delivery.hits().await;
        assert_eq!(
            hits,
            vec![
                "https://push.example/alice".to_string(),
                "https://push.example/bob".to_string(),
                "https://push.example/carol".to_string()
            ]
        );

        let loaded = {
            let push_state = state.push_state.lock().await;
            push_state.load().unwrap()
        };
        assert_eq!(loaded.push_endpoints.len(), 1);
        assert!(loaded.push_endpoints.contains_key("alice"));
    }

    #[tokio::test]
    async fn push_request_sends_encrypted_json_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let mut bytes_read = 0;
            let header_end;

            loop {
                let read = stream.read(&mut buffer[bytes_read..]).await.unwrap();
                bytes_read += read;
                let request = &buffer[..bytes_read];
                if let Some(offset) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = offset + 4;
                    break;
                }
            }

            let headers = String::from_utf8(buffer[..header_end].to_vec()).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        Some(value.trim().parse::<usize>().unwrap())
                    } else {
                        None
                    }
                })
                .unwrap();

            while bytes_read - header_end < content_length {
                let read = stream.read(&mut buffer[bytes_read..]).await.unwrap();
                bytes_read += read;
            }

            stream
                .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();

            buffer[..bytes_read].to_vec()
        });

        let tempdir = TempDir::new().unwrap();
        let vapid = SyncStateStore::new(tempdir.path().join("sync-state.json"))
            .ensure_vapid_keys()
            .unwrap();

        let delivery = ReqwestPushDelivery::new();
        let subscription = sample_subscription(&format!("http://{address}/push"));
        let result = delivery.post_branch_updated(&subscription, &vapid).await;

        assert!(matches!(result, PushDeliveryResult::Delivered));

        let request = server.await.unwrap();
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let lower = headers.to_ascii_lowercase();
        let body = &request[header_end..];

        assert!(headers.starts_with("POST /push HTTP/1.1\r\n"));
        assert!(lower.contains("\r\ncontent-type: application/octet-stream\r\n"));
        assert!(lower.contains("\r\ncontent-encoding: aes128gcm\r\n"));
        assert!(!lower.contains("\r\ncontent-length: 0\r\n"));
        assert!(!body.is_empty());
    }
}
