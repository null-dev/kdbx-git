use std::path::{Path, PathBuf};

use color_eyre::eyre::{bail, Result};
use tracing::info;

pub mod config;
pub mod init;
pub mod kdbx;
pub mod server;
pub mod storage;
pub mod store;
pub mod sync;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Serve {
        config_path: PathBuf,
    },
    Init {
        config_path: PathBuf,
    },
    SyncLocal {
        config_path: PathBuf,
        client_id: String,
        local_path: PathBuf,
        once: bool,
        interval_secs: u64,
    },
}

fn usage() -> &'static str {
    "usage: kdbx-git [config.toml]\n       kdbx-git --init [config.toml]\n       kdbx-git sync-local [--once] [--interval-secs N] [config.toml] <client-id> <local.kdbx>"
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
        Some("sync-local" | "--sync-local") => parse_sync_local_args(&args[1..]),
        Some("--help" | "-h") if args.len() == 1 => {
            bail!(usage());
        }
        Some(config_path) if args.len() == 1 => Ok(CliCommand::Serve {
            config_path: PathBuf::from(config_path),
        }),
        _ => bail!(usage()),
    }
}

fn parse_sync_local_args(args: &[String]) -> Result<CliCommand> {
    let mut once = false;
    let mut interval_secs = 2_u64;
    let mut positionals = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--once" => {
                once = true;
                i += 1;
            }
            "--interval-secs" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| eyre::eyre!("--interval-secs requires a value"))?;
                interval_secs = value
                    .parse()
                    .map_err(|_| eyre::eyre!("invalid --interval-secs value: {value}"))?;
                if interval_secs == 0 {
                    bail!("--interval-secs must be at least 1");
                }
                i += 2;
            }
            value if value.starts_with("--interval-secs=") => {
                let (_, raw) = value.split_once('=').unwrap();
                interval_secs = raw
                    .parse()
                    .map_err(|_| eyre::eyre!("invalid --interval-secs value: {raw}"))?;
                if interval_secs == 0 {
                    bail!("--interval-secs must be at least 1");
                }
                i += 1;
            }
            value if value.starts_with('-') => bail!("unknown sync-local flag: {value}"),
            _ => {
                positionals.push(args[i].clone());
                i += 1;
            }
        }
    }

    let (config_path, client_id, local_path) = match positionals.as_slice() {
        [client_id, local_path] => (
            PathBuf::from("config.toml"),
            client_id.clone(),
            PathBuf::from(local_path),
        ),
        [config_path, client_id, local_path] => (
            PathBuf::from(config_path),
            client_id.clone(),
            PathBuf::from(local_path),
        ),
        _ => bail!(usage()),
    };

    Ok(CliCommand::SyncLocal {
        config_path,
        client_id,
        local_path,
        once,
        interval_secs,
    })
}

pub async fn run_cli<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    init_observability()?;

    match parse_cli_args(args)? {
        CliCommand::Serve { config_path } => serve_from_config_path(&config_path).await,
        CliCommand::Init { config_path } => init::init_from_config_path(&config_path).await,
        CliCommand::SyncLocal {
            config_path,
            client_id,
            local_path,
            once,
            interval_secs,
        } => {
            sync::sync_local_from_config_path(
                &config_path,
                sync::SyncLocalOptions {
                    client_id,
                    local_path,
                    once,
                    interval_secs,
                },
            )
            .await
        }
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
    fn parses_sync_local_with_defaults() {
        assert_eq!(
            parse_cli_args([
                "kdbx-git".into(),
                "sync-local".into(),
                "alice".into(),
                "alice.kdbx".into(),
            ])
            .unwrap(),
            CliCommand::SyncLocal {
                config_path: PathBuf::from("config.toml"),
                client_id: "alice".into(),
                local_path: PathBuf::from("alice.kdbx"),
                once: false,
                interval_secs: 2,
            }
        );
    }

    #[test]
    fn parses_sync_local_flags() {
        assert_eq!(
            parse_cli_args([
                "kdbx-git".into(),
                "sync-local".into(),
                "--once".into(),
                "--interval-secs".into(),
                "5".into(),
                "custom.toml".into(),
                "alice".into(),
                "alice.kdbx".into(),
            ])
            .unwrap(),
            CliCommand::SyncLocal {
                config_path: PathBuf::from("custom.toml"),
                client_id: "alice".into(),
                local_path: PathBuf::from("alice.kdbx"),
                once: true,
                interval_secs: 5,
            }
        );
    }
}
