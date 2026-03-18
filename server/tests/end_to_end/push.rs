use super::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PushStateFile {
    vapid: Option<PushStateVapid>,
    #[serde(default)]
    push_endpoints: BTreeMap<String, PushEndpointFileEntry>,
}

#[derive(Debug, Deserialize)]
struct PushStateVapid {
    private_key: String,
    public_key: String,
}

#[derive(Debug, Deserialize)]
struct PushEndpointFileEntry {
    endpoint: String,
    last_seen_at: String,
}

fn read_push_state(path: &Path) -> PushStateFile {
    let contents = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(&contents).unwrap()
}

#[tokio::test]
async fn server_startup_creates_vapid_keypair_in_sync_state() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let sync_state_path = common::sync_state_path(&config);

    let _server = TestServer::start(config, tempdir).await.unwrap();

    let state = read_push_state(&sync_state_path);
    let vapid = state.vapid.expect("expected VAPID keys on startup");
    assert!(!vapid.private_key.is_empty());
    assert!(!vapid.public_key.is_empty());
    assert!(state.push_endpoints.is_empty());
}

#[tokio::test]
async fn register_push_endpoint_persists_endpoint_and_updates_timestamp() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let sync_state_path = common::sync_state_path(&config);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
        "alice-pass",
        reqwest::Method::POST,
        &format!("{}/push/alice/endpoint", server.base_url),
    )
    .json(&serde_json::json!({
        "endpoint": "https://push.example/alice-1"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let state = read_push_state(&sync_state_path);
    assert!(state.vapid.is_some());
    let alice = state.push_endpoints.get("alice").unwrap();
    assert_eq!(alice.endpoint, "https://push.example/alice-1");
    assert!(!alice.last_seen_at.is_empty());
}

#[tokio::test]
async fn register_push_endpoint_replaces_existing_endpoint() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let sync_state_path = common::sync_state_path(&config);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    for endpoint in [
        "https://push.example/alice-old",
        "https://push.example/alice-new",
    ] {
        let response = authed(
            &client,
            "alice",
            "alice-pass",
            reqwest::Method::POST,
            &format!("{}/push/alice/endpoint", server.base_url),
        )
        .json(&serde_json::json!({ "endpoint": endpoint }))
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let state = read_push_state(&sync_state_path);
    assert_eq!(state.push_endpoints.len(), 1);
    assert_eq!(
        state.push_endpoints["alice"].endpoint,
        "https://push.example/alice-new"
    );
    assert!(state.vapid.is_some());
}

#[tokio::test]
async fn delete_push_endpoint_is_idempotent() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let sync_state_path = common::sync_state_path(&config);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let register = authed(
        &client,
        "alice",
        "alice-pass",
        reqwest::Method::POST,
        &format!("{}/push/alice/endpoint", server.base_url),
    )
    .json(&serde_json::json!({
        "endpoint": "https://push.example/alice"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(register.status(), StatusCode::NO_CONTENT);

    for _ in 0..2 {
        let delete = authed(
            &client,
            "alice",
            "alice-pass",
            reqwest::Method::DELETE,
            &format!("{}/push/alice/endpoint", server.base_url),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    }

    let state = read_push_state(&sync_state_path);
    assert!(state.push_endpoints.is_empty());
    assert!(state.vapid.is_some());
}

#[tokio::test]
async fn register_push_endpoint_requires_matching_basic_auth_identity() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "bob",
        "bob-pass",
        reqwest::Method::POST,
        &format!("{}/push/alice/endpoint", server.base_url),
    )
    .json(&serde_json::json!({
        "endpoint": "https://push.example/alice"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn register_push_endpoint_rejects_non_https_urls() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let sync_state_path = common::sync_state_path(&config);
    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    for endpoint in [
        "http://push.example/alice",
        "not-a-url",
        "ftp://push.example/alice",
    ] {
        let response = authed(
            &client,
            "alice",
            "alice-pass",
            reqwest::Method::POST,
            &format!("{}/push/alice/endpoint", server.base_url),
        )
        .json(&serde_json::json!({ "endpoint": endpoint }))
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let state = read_push_state(&sync_state_path);
    assert!(state.vapid.is_some());
    assert!(state.push_endpoints.is_empty());
}

#[tokio::test]
async fn register_push_endpoint_prunes_expired_entries_on_write() {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let sync_state_path = common::sync_state_path(&config);
    std::fs::write(
        &sync_state_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "push_endpoints": {
                "stale-client": {
                    "endpoint": "https://push.example/stale",
                    "last_seen_at": "2026-03-01T00:00:00Z"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let response = authed(
        &client,
        "alice",
        "alice-pass",
        reqwest::Method::POST,
        &format!("{}/push/alice/endpoint", server.base_url),
    )
    .json(&serde_json::json!({
        "endpoint": "https://push.example/alice"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let state = read_push_state(&sync_state_path);
    assert_eq!(state.push_endpoints.len(), 1);
    assert!(state.push_endpoints.contains_key("alice"));
    assert!(!state.push_endpoints.contains_key("stale-client"));
    assert!(state.vapid.is_some());
}
