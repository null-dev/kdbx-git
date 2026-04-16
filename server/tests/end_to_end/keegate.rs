use super::*;

fn seed_keegate_db() -> kdbx_git::storage::types::StorageDatabase {
    let mut db = sample_db("KeeGate DB", "Seed Entry");

    let mut users = make_group("00000000-0000-0000-0000-000000000100", "KeeGate Users");

    let mut api_user = make_entry(
        "00000000-0000-0000-0000-000000000101",
        "API User",
        "app-client",
        "app-secret",
    );
    api_user.tags = vec!["prod".into(), "shared".into()];
    users.entries.push(api_user);

    let mut ignored_user = make_entry(
        "00000000-0000-0000-0000-000000000102",
        "Ignored User",
        "alice",
        "alice-pass",
    );
    ignored_user.tags = vec!["prod".into()];
    users.entries.push(ignored_user);
    db.root.groups.push(users);

    let mut apps = make_group("00000000-0000-0000-0000-000000000200", "Applications");

    let mut prod = make_entry(
        "2f8f6e1d-3f43-4d38-9e3c-3b8bdbf19c4e",
        "Prod Postgres",
        "db_admin",
        "prod-secret",
    );
    prod.tags = vec!["prod".into(), "database".into()];
    prod.fields.insert(
        "URL".into(),
        StorageValue {
            value: "https://db.example.com".into(),
            protected: false,
        },
    );
    prod.fields.insert(
        "Notes".into(),
        StorageValue {
            value: "primary production database".into(),
            protected: false,
        },
    );
    apps.entries.push(prod);

    let mut shared = make_entry(
        "11111111-1111-1111-1111-111111111111",
        "Shared Redis",
        "cache_admin",
        "shared-secret",
    );
    shared.tags = vec!["shared".into(), "cache".into()];
    apps.entries.push(shared);

    let mut staging = make_entry(
        "22222222-2222-2222-2222-222222222222",
        "Staging Postgres",
        "staging_admin",
        "staging-secret",
    );
    staging.tags = vec!["staging".into(), "database".into()];
    apps.entries.push(staging);

    db.root.groups.push(apps);

    db
}

async fn start_seeded_server() -> TestServer {
    let tempdir = TempDir::new().unwrap();
    let config = test_config(tempdir.path());
    let db = seed_keegate_db();
    let store = GitStore::open_or_init(&config.git_store).unwrap();
    store
        .bootstrap_main(db, "seed KeeGate API".into())
        .await
        .unwrap();

    TestServer::start(config, tempdir).await.unwrap()
}

#[tokio::test]
async fn keegate_info_endpoint_is_public() {
    let server = start_seeded_server().await;
    let client = Client::new();

    let response = client
        .get(format!("{}/api/v1/keegate/info", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["name"], "KeeGate API");
    assert_eq!(body["version"], "v1");
}

#[tokio::test]
async fn keegate_query_requires_keegate_basic_auth() {
    let server = start_seeded_server().await;
    let client = Client::new();
    let url = format!("{}/api/v1/keegate/entries/query", server.base_url);
    let body = serde_json::json!({
        "filter": { "tag": "prod" }
    });

    let missing = client.post(&url).json(&body).send().await.unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        missing
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap(),
        "Basic realm=\"KeeGate API\""
    );

    let wrong_domain = client
        .post(&url)
        .basic_auth("bob", Some("bob-pass"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_domain.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn keegate_query_filters_and_authorizes_entries() {
    let server = start_seeded_server().await;
    let client = Client::new();

    let response = client
        .post(format!("{}/api/v1/keegate/entries/query", server.base_url))
        .basic_auth("app-client", Some("app-secret"))
        .json(&serde_json::json!({
            "filter": {
                "and": [
                    {
                        "or": [
                            { "tag": "prod" },
                            { "uuid": "11111111-1111-1111-1111-111111111111" }
                        ]
                    },
                    {
                        "or": [
                            { "title_contains": "postgres" },
                            { "title_regex": "(?i)redis" }
                        ]
                    }
                ]
            },
            "options": {
                "limit": 10
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();

    assert_eq!(body["meta"]["count"], 2);
    let titles: Vec<String> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles, vec!["Prod Postgres", "Shared Redis"]);

    let prod = &body["entries"][0];
    assert_eq!(prod["username"], "db_admin");
    assert_eq!(prod["password"], "prod-secret");
    assert_eq!(prod["url"], "https://db.example.com");
    assert_eq!(prod["notes"], "primary production database");
    assert_eq!(prod["group_path"], serde_json::json!(["Applications"]));

    let serialized = serde_json::to_string(&body["entries"]).unwrap();
    assert!(!serialized.contains("Ignored User"));
    assert!(!serialized.contains("Staging Postgres"));
}

#[tokio::test]
async fn keegate_query_rejects_invalid_regex() {
    let server = start_seeded_server().await;
    let client = Client::new();

    let response = client
        .post(format!("{}/api/v1/keegate/entries/query", server.base_url))
        .basic_auth("app-client", Some("app-secret"))
        .json(&serde_json::json!({
            "filter": {
                "title_regex": "("
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], "invalid_request");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("invalid regex in filter.title_regex"));
}
