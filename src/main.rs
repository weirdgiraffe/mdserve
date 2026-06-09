use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

mod app;

#[derive(Parser)]
#[command(name = "mdserve")]
#[command(about = "A markdown preview server for AI coding agents")]
#[command(version)]
struct Args {
    /// Initial file or directory to view, resolved under --base-dir [default: base dir]
    path: Option<PathBuf>,

    /// Security boundary; nothing outside is ever served [default: current directory]
    #[arg(long)]
    base_dir: Option<PathBuf>,

    /// Hostname (domain or IP address) to listen on
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    hostname: String,

    /// Port to serve on
    #[arg(short, long, default_value = "3000")]
    port: u16,

    /// Open the initial path in the default browser
    #[arg(short, long)]
    open: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let base_dir = match args.base_dir {
        Some(dir) => dir,
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let base_dir = base_dir
        .canonicalize()
        .with_context(|| format!("base-dir does not exist: {}", base_dir.display()))?;
    if !base_dir.is_dir() {
        anyhow::bail!("base-dir must be a directory: {}", base_dir.display());
    }

    let path = args.path.unwrap_or_else(|| base_dir.clone());
    let url_path = app::initial_url_path(&base_dir, &path)?;

    app::serve(base_dir, url_path, args.hostname, args.port, args.open).await
}
