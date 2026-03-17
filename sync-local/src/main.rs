use color_eyre::eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    kdbx_git_sync_local::run_cli(std::env::args()).await
}
