use std::path::{Path, PathBuf};

use color_eyre::eyre::{bail, Result};
use tracing::info;

pub mod config;
pub mod init;
pub mod server;

pub use kdbx_git_common::{kdbx, storage, store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Serve { config_path: PathBuf },
    Init { config_path: PathBuf },
}

fn usage() -> &'static str {
    "usage: kdbx-git [config.toml]\n       kdbx-git --init [config.toml]"
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
    let mut args: Vec<String> = args.into_iter().collect();
    if !args.is_empty() {
        args.remove(0);
    }

    match args.first().map(String::as_str) {
        None => Ok(CliCommand::Serve {
            config_path: PathBuf::from("config.toml"),
        }),
        Some("--init" | "init") => match args.get(1..) {
            Some([]) => Ok(CliCommand::Init {
                config_path: PathBuf::from("config.toml"),
            }),
            Some([config_path]) => Ok(CliCommand::Init {
                config_path: PathBuf::from(config_path),
            }),
            _ => bail!(usage()),
        },
        Some("sync-local" | "--sync-local") => {
            bail!("sync-local moved to the dedicated `kdbx-git-sync-local` binary")
        }
        Some("--help" | "-h") if args.len() == 1 => {
            bail!(usage());
        }
        Some(config_path) if args.len() == 1 => Ok(CliCommand::Serve {
            config_path: PathBuf::from(config_path),
        }),
        _ => bail!(usage()),
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

    #[test]
    fn sync_local_command_points_to_new_binary() {
        let err = parse_cli_args(["kdbx-git".into(), "sync-local".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("kdbx-git-sync-local"));
    }
}
