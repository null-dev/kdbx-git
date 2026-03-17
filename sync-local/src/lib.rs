use std::path::{Path, PathBuf};

use color_eyre::eyre::{bail, Result};

pub mod config;
pub mod sync;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    SyncLocal {
        config_path: PathBuf,
        local_path: PathBuf,
        once: bool,
        poll: bool,
    },
}

fn usage() -> &'static str {
    "usage: kdbx-git-sync-local [--once] [--poll] [config.toml] <local.kdbx>"
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

pub fn parse_cli_args<I>(args: I) -> Result<CliCommand>
where
    I: IntoIterator<Item = String>,
{
    let mut args: Vec<String> = args.into_iter().collect();
    if !args.is_empty() {
        args.remove(0);
    }

    let mut once = false;
    let mut poll = false;
    let mut positionals = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--once" => {
                once = true;
                i += 1;
            }
            "--poll" => {
                poll = true;
                i += 1;
            }
            "--help" | "-h" if args.len() == 1 => bail!(usage()),
            value if value.starts_with('-') => bail!("unknown sync-local flag: {value}"),
            _ => {
                positionals.push(args[i].clone());
                i += 1;
            }
        }
    }

    let (config_path, local_path) = match positionals.as_slice() {
        [local_path] => (PathBuf::from("config.toml"), PathBuf::from(local_path)),
        [config_path, local_path] => (PathBuf::from(config_path), PathBuf::from(local_path)),
        _ => bail!(usage()),
    };

    Ok(CliCommand::SyncLocal {
        config_path,
        local_path,
        once,
        poll,
    })
}

pub async fn run_cli<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    init_observability()?;

    match parse_cli_args(args)? {
        CliCommand::SyncLocal {
            config_path,
            local_path,
            once,
            poll,
        } => {
            let config = config::Config::from_file(&config_path)?;
            sync::sync_local(
                config.clone(),
                sync::SyncLocalOptions {
                    client_id: config.client_id.clone(),
                    local_path,
                    once,
                    poll,
                    server_url: Some(config.server_url.clone()),
                },
            )
            .await
        }
    }
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
    use super::{parse_cli_args, CliCommand};
    use std::path::PathBuf;

    #[test]
    fn parses_sync_local_with_defaults() {
        assert_eq!(
            parse_cli_args(["kdbx-git-sync-local".into(), "alice.kdbx".into()]).unwrap(),
            CliCommand::SyncLocal {
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
                "--once".into(),
                "--poll".into(),
                "custom.toml".into(),
                "alice.kdbx".into(),
            ])
            .unwrap(),
            CliCommand::SyncLocal {
                config_path: PathBuf::from("custom.toml"),
                local_path: PathBuf::from("alice.kdbx"),
                once: true,
                poll: true,
            }
        );
    }
}
