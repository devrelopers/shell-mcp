//! shell-mcp binary entry point.
//!
//! Wires up tracing, parses CLI arguments, constructs an [`Engine`] anchored
//! at the launch root, and serves the MCP protocol over stdio.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

use shell_mcp::tools::{Engine, ShellServer};

#[derive(Debug, Parser)]
#[command(
    name = "shell-mcp",
    version,
    about = "Scoped, allowlisted shell access over MCP"
)]
struct Cli {
    /// Launch root. Every command is forced to run inside this directory.
    /// Defaults to the current working directory.
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
    let root = match cli.root {
        Some(p) => p,
        None => std::env::current_dir().context("could not read current working directory")?,
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("launch root does not exist: {}", root.display()))?;

    tracing::info!(root = %root.display(), "starting shell-mcp");

    let engine = Arc::new(Engine::new(root));
    let service = ShellServer::new(engine).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
