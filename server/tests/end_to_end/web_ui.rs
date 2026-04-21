use super::*;

use argon2::Config as Argon2Config;
use kdbx_git::config::WebUiAdminUser;

fn admin_password_hash(password: &str) -> String {
    argon2::hash_encoded(password.as_bytes(), b"web-ui-test-salt", &Argon2Config::default())
        .unwrap()
}

fn enable_web_ui(config: &mut kdbx_git::config::Config, root: &Path) {
    let _ = root;
    config.web_ui.enabled = true;
    config.web_ui.admin_users = vec![WebUiAdminUser {
        username: "admin".into(),
        password_hash: admin_password_hash("admin-pass"),
    }];
}

fn session_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn web_ui_login_sets_session_cookie_and_status_requires_auth() {
    let tempdir = TempDir::new().unwrap();
    let mut config = test_config(tempdir.path());
    enable_web_ui(&mut config, tempdir.path());

    let server = TestServer::start(config.clone(), tempdir).await.unwrap();
    let client = Client::new();

    let unauthorized = client
        .get(format!("{}/api/ui/v1/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let login = client
        .post(format!("{}/api/ui/v1/session/login", server.base_url))
        .json(&serde_json::json!({
            "username": "admin",
            "password": "admin-pass"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);

    let cookie = session_cookie(&login);

    let session = client
        .get(format!("{}/api/ui/v1/session", server.base_url))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let session_body: serde_json::Value = session.json().await.unwrap();
    assert_eq!(session_body["authenticated"], true);
    assert_eq!(session_body["username"], "admin");

    let status = client
        .get(format!("{}/api/ui/v1/status", server.base_url))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let status_body: serde_json::Value = status.json().await.unwrap();
    assert_eq!(status_body["authenticated_username"], "admin");
    assert_eq!(status_body["client_count"], 3);
    assert_eq!(status_body["asset_delivery"], "embedded");

    let logout = client
        .post(format!("{}/api/ui/v1/session/logout", server.base_url))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::OK);

    let status_after_logout = client
        .get(format!("{}/api/ui/v1/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(status_after_logout.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn web_ui_serves_spa_shell_and_static_assets() {
    let tempdir = TempDir::new().unwrap();
    let mut config = test_config(tempdir.path());
    enable_web_ui(&mut config, tempdir.path());

    let server = TestServer::start(config, tempdir).await.unwrap();
    let client = Client::new();

    let shell = client
        .get(format!("{}/ui", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(shell.status(), StatusCode::OK);
    let shell_text = shell.text().await.unwrap();
    assert!(shell_text.contains("__sveltekit_"));
    assert!(shell_text.contains("/ui/_app/immutable/entry/start"));

    let nested_route = client
        .get(format!("{}/ui/login", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(nested_route.status(), StatusCode::OK);
    let nested_text = nested_route.text().await.unwrap();
    assert!(nested_text.contains("__sveltekit_"));
    assert!(nested_text.contains("/ui/_app/immutable/entry/start"));

    let asset = client
        .get(format!("{}/ui/robots.txt", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    assert!(asset.text().await.unwrap().contains("User-agent"));
}
