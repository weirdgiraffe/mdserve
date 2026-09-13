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

    /// Security boundary; nothing outside is ever served [default: git root, else current directory]
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

/// Resolves the base directory: the flag wins, then the nearest ancestor holding
/// a `.git` entry, then the start directory.
fn resolve_base_dir(flag: Option<PathBuf>, start: PathBuf) -> PathBuf {
    if let Some(dir) = flag {
        return dir;
    }
    let git_root = start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(|dir| dir.to_path_buf());
    git_root.unwrap_or(start)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let start_dir = std::env::current_dir().context("failed to determine current directory")?;
    let base_dir = resolve_base_dir(args.base_dir, start_dir);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_base_dir_prefers_the_flag() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let start = dir.path().join("nested/deeper");
        std::fs::create_dir_all(&start).unwrap();
        let flag = dir.path().join("nested");

        assert_eq!(resolve_base_dir(Some(flag.clone()), start), flag);
    }

    #[test]
    fn test_base_dir_finds_the_git_root() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let start = dir.path().join("nested/deeper");
        std::fs::create_dir_all(&start).unwrap();

        assert_eq!(resolve_base_dir(None, start), dir.path().to_path_buf());
    }

    #[test]
    fn test_base_dir_accepts_a_git_file() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere/worktree\n").unwrap();
        let start = dir.path().join("nested");
        std::fs::create_dir_all(&start).unwrap();

        assert_eq!(resolve_base_dir(None, start), dir.path().to_path_buf());
    }

    #[test]
    fn test_base_dir_falls_back_to_the_start() {
        let dir = tempdir().unwrap();
        let start = dir.path().join("nested/deeper");
        std::fs::create_dir_all(&start).unwrap();

        assert_eq!(resolve_base_dir(None, start.clone()), start);
    }
}
