use std::{future::Future, pin::Pin, time::Duration};

use chrono::Utc;
use eyre::Result;
use reqwest::Client;
use tokio::time::timeout;
use tracing::warn;
use web_push::{
    request_builder, SubscriptionInfo, Urgency, VapidSignatureBuilder, WebPushMessageBuilder,
};

use crate::sync_state::{RevokedPushEndpoint, VapidKeys};

use super::state::AppState;

const PUSH_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
const PUSH_DELIVERY_TTL_SECS: u32 = 30;
const PUSH_TOPIC: &str = "branch-updated";

pub(super) type PushDeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = PushDeliveryResult> + Send + 'a>>;

pub(super) enum PushDeliveryResult {
    Delivered,
    Revoked,
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
        endpoint: &'a str,
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
        endpoint: &'a str,
        vapid: &'a VapidKeys,
    ) -> PushDeliveryFuture<'a> {
        Box::pin(async move {
            let subscription_info = SubscriptionInfo::new(endpoint, "", "");
            let signature =
                match VapidSignatureBuilder::from_base64(&vapid.private_key, &subscription_info) {
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

            let mut builder = WebPushMessageBuilder::new(&subscription_info);
            builder.set_ttl(PUSH_DELIVERY_TTL_SECS);
            builder.set_urgency(Urgency::Low);
            builder.set_topic(PUSH_TOPIC.to_string());
            builder.set_vapid_signature(signature);

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
                    if matches!(
                        response.status(),
                        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
                    ) =>
                {
                    PushDeliveryResult::Revoked
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
        endpoint: String,
    ) -> Result<()> {
        let store = self.push_state.lock().await;
        store.upsert_push_endpoint(client_id, endpoint, Utc::now())
    }

    pub(crate) async fn remove_push_endpoint(&self, client_id: &str) -> Result<()> {
        let store = self.push_state.lock().await;
        store.remove_push_endpoint(client_id, Utc::now())
    }

    pub(crate) async fn load_push_delivery_targets(&self) -> Result<Vec<(String, String)>> {
        let store = self.push_state.lock().await;
        let state = store.load()?;
        Ok(state
            .push_endpoints
            .into_iter()
            .map(|(client_id, entry)| (client_id, entry.endpoint))
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
        for (client_id, endpoint) in targets {
            match self
                .push_delivery
                .post_branch_updated(&endpoint, &self.vapid_keys)
                .await
            {
                PushDeliveryResult::Revoked => revoked.push(RevokedPushEndpoint {
                    client_id,
                    endpoint,
                }),
                PushDeliveryResult::Delivered => {}
                PushDeliveryResult::Failed(err) => {
                    warn!(
                        "push delivery to '{}' for client '{}' failed: {}",
                        endpoint, client_id, err
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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use chrono::TimeZone;
    use http::StatusCode;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use crate::{
        config::{Config, DatabaseCredentials},
        store::GitStore,
    };

    use super::*;

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
            endpoint: &'a str,
            _vapid: &'a VapidKeys,
        ) -> PushDeliveryFuture<'a> {
            Box::pin(async move {
                self.hits.lock().await.push(endpoint.to_string());
                match self
                    .statuses
                    .lock()
                    .await
                    .get(endpoint)
                    .copied()
                    .unwrap_or(StatusCode::OK)
                {
                    StatusCode::NOT_FOUND | StatusCode::GONE => PushDeliveryResult::Revoked,
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
            bind_addr: "127.0.0.1:0".into(),
            database: DatabaseCredentials {
                password: Some("test-password".into()),
                keyfile: None,
            },
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
        let now = Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap();

        {
            let push_state = state.push_state.lock().await;
            push_state
                .upsert_push_endpoint("alice", "https://push.example/alice".into(), now)
                .unwrap();
            push_state
                .upsert_push_endpoint("bob", "https://push.example/bob".into(), now)
                .unwrap();
            push_state
                .upsert_push_endpoint("carol", "https://push.example/carol".into(), now)
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
}
