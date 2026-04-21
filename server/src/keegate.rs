use color_eyre::eyre::{Result, WrapErr};
use kdbx_git_keegate_client::KeeGateClient;

pub async fn resolve(reference: &str) -> Result<()> {
    // For absolute kg:// references the credentials are embedded in the URL,
    // so the client's own base_url/credentials are unused.
    let client = KeeGateClient::new("http://localhost", "unused", "unused")
        .wrap_err("failed to build KeeGate client")?;

    let response = client
        .resolve(reference)
        .await
        .wrap_err_with(|| format!("failed to resolve '{reference}'"))?;

    println!("{}", serde_json::to_string_pretty(&response).unwrap());
    Ok(())
}
