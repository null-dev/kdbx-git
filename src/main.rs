use color_eyre::eyre::Result;
use tracing::info;

mod config;
mod storage;
mod store;

fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kdbx_git=info".into()),
        )
        .init();

    info!("kdbx-git starting");

    Ok(())
}
