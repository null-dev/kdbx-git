use std::path::{Path, PathBuf};

use color_eyre::eyre::{bail, Result};
use tracing::info;

pub mod config;
pub mod init;
pub mod kdbx;
pub mod server;
pub mod storage;
pub mod store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Serve { config_path: PathBuf },
    Init { config_path: PathBuf },
}

pub fn init_observability() -> Result<()> {
    color_eyre::install()?;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kdbx_git=info".into()),
        )
        .try_init();

    Ok(())
}

pub fn parse_cli_args<I>(args: I) -> Result<CliCommand>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let _bin = args.next();

    let first = args.next();
    let second = args.next();
    let third = args.next();

    if third.is_some() {
        bail!("too many arguments");
    }

    match (first.as_deref(), second) {
        (None, None) => Ok(CliCommand::Serve {
            config_path: PathBuf::from("config.toml"),
        }),
        (Some("--init" | "init"), None) => Ok(CliCommand::Init {
            config_path: PathBuf::from("config.toml"),
        }),
        (Some("--init" | "init"), Some(config_path)) => Ok(CliCommand::Init {
            config_path: PathBuf::from(config_path),
        }),
        (Some("--help" | "-h"), None) => {
            bail!("usage: kdbx-git [config.toml]\n       kdbx-git --init [config.toml]");
        }
        (Some(config_path), None) => Ok(CliCommand::Serve {
            config_path: PathBuf::from(config_path),
        }),
        _ => bail!("usage: kdbx-git [config.toml]\n       kdbx-git --init [config.toml]"),
    }
}

pub async fn run_cli<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    init_observability()?;

    match parse_cli_args(args)? {
        CliCommand::Serve { config_path } => serve_from_config_path(&config_path).await,
        CliCommand::Init { config_path } => init::init_from_config_path(&config_path).await,
    }
}

pub async fn serve_from_config_path(config_path: &Path) -> Result<()> {
    info!("kdbx-git starting");

    let config = config::Config::from_file(config_path)?;
    let store = store::GitStore::open_or_init(&config.git_store)?;

    server::run_server(config, store).await
}

#[cfg(test)]
mod tests {
    use super::{parse_cli_args, CliCommand};
    use std::path::PathBuf;

    #[test]
    fn parses_default_serve_command() {
        assert_eq!(
            parse_cli_args(["kdbx-git".to_string()]).unwrap(),
            CliCommand::Serve {
                config_path: PathBuf::from("config.toml"),
            }
        );
    }

    #[test]
    fn parses_explicit_init_command() {
        assert_eq!(
            parse_cli_args(["kdbx-git".into(), "--init".into(), "custom.toml".into()]).unwrap(),
            CliCommand::Init {
                config_path: PathBuf::from("custom.toml"),
            }
        );
    }
}
