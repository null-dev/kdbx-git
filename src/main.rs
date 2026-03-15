use color_eyre::eyre::Result;
use tracing::info;

mod config;
mod kdbx;
mod server;
mod storage;
mod store;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kdbx_git=info".into()),
        )
        .init();

    info!("kdbx-git starting");

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let config = config::Config::from_file(std::path::Path::new(&config_path))?;
    let store = store::GitStore::open_or_init(&config.git_store)?;

    server::run_server(config, store).await
}
