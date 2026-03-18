use std::path::{Path, PathBuf};

use clap::Parser;
use color_eyre::eyre::Result;

pub mod config;
pub mod sync;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "kdbx-git-sync-local", disable_help_subcommand = true)]
pub struct CliOptions {
    #[arg(long = "config", default_value = "config.toml")]
    pub config_path: PathBuf,
    #[arg(long)]
    pub once: bool,
    #[arg(long)]
    pub poll: bool,
    #[arg()]
    pub local_path: PathBuf,
}

pub fn init_observability() -> Result<()> {
    color_eyre::install()?;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    Ok(())
}

pub fn parse_cli_args<I>(args: I) -> Result<CliOptions>
where
    I: IntoIterator<Item = String>,
{
    Ok(CliOptions::try_parse_from(args)?)
}

pub async fn run_cli<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    init_observability()?;

    let options = parse_cli_args(args)?;
    let config = config::Config::from_file(&options.config_path)?;
    sync::sync_local(
        config.clone(),
        sync::SyncLocalOptions {
            client_id: config.client_id.clone(),
            local_path: options.local_path,
            once: options.once,
            poll: options.poll,
            server_url: Some(config.server_url.clone()),
        },
    )
    .await
}

pub async fn sync_local_from_config_path(config_path: &Path, local_path: PathBuf) -> Result<()> {
    let config = config::Config::from_file(config_path)?;
    sync::sync_local(
        config.clone(),
        sync::SyncLocalOptions {
            client_id: config.client_id.clone(),
            local_path,
            once: false,
            poll: false,
            server_url: Some(config.server_url.clone()),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{parse_cli_args, CliOptions};
    use std::path::PathBuf;

    #[test]
    fn parses_sync_local_with_defaults() {
        assert_eq!(
            parse_cli_args(["kdbx-git-sync-local".into(), "alice.kdbx".into()]).unwrap(),
            CliOptions {
                config_path: PathBuf::from("config.toml"),
                local_path: PathBuf::from("alice.kdbx"),
                once: false,
                poll: false,
            }
        );
    }

    #[test]
    fn parses_sync_local_flags() {
        assert_eq!(
            parse_cli_args([
                "kdbx-git-sync-local".into(),
                "--config".into(),
                "custom.toml".into(),
                "--once".into(),
                "--poll".into(),
                "alice.kdbx".into(),
            ])
            .unwrap(),
            CliOptions {
                config_path: PathBuf::from("custom.toml"),
                local_path: PathBuf::from("alice.kdbx"),
                once: true,
                poll: true,
            }
        );
    }
}
