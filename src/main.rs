//! shell-mcp binary entry point.
//!
//! Parses CLI arguments, resolves the launch root from
//! `--root` / `SHELL_MCP_ROOT` / launch cwd (in that precedence), and
//! serves the MCP protocol over stdio.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

use shell_mcp::root::resolve_root;
use shell_mcp::tools::{Engine, ShellServer};

const ROOT_ENV: &str = "SHELL_MCP_ROOT";

#[derive(Debug, Parser)]
#[command(
    name = "shell-mcp",
    version,
    about = "Scoped, allowlisted shell access over MCP",
    long_about = "Scoped, allowlisted shell access over MCP.\n\n\
                  The launch root is the directory every executed command is pinned into. \
                  It is resolved from these sources, highest precedence first:\n\
                  \x20 1. --root <PATH> CLI flag\n\
                  \x20 2. SHELL_MCP_ROOT environment variable\n\
                  \x20 3. The process's current working directory at launch\n\n\
                  Claude Desktop launches MCP servers from an undefined cwd, so the cwd \
                  fallback is unsafe under Desktop — always pass --root or set \
                  SHELL_MCP_ROOT in the Desktop config."
)]
struct Cli {
    /// Launch root (must be an absolute path to an existing directory).
    /// Overrides SHELL_MCP_ROOT and the launch cwd.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so they don't pollute the stdio MCP transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SHELL_MCP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let env_root = std::env::var(ROOT_ENV).ok();
    let launch_cwd = std::env::current_dir().context("could not read current working directory")?;

    let resolved = resolve_root(cli.root.as_deref(), env_root.as_deref(), &launch_cwd)
        .context("could not resolve launch root")?;

    tracing::info!(
        root = %resolved.path.display(),
        source = resolved.source.as_str(),
        "starting shell-mcp",
    );

    let engine = Arc::new(Engine::new(resolved.path));
    let service = ShellServer::new(engine).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
