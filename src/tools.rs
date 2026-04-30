//! MCP tool definitions and the underlying [`Engine`] that performs the
//! reject/allow/execute pipeline.
//!
//! The engine is exposed publicly so integration tests can drive the same
//! code path the MCP server uses without wiring up stdio.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::config::{ConfigCache, ConfigError, LoadedConfig};
use crate::exec::{execute, ExecError, ExecOptions, ExecOutcome};
use crate::safety::{check_hard_denylist, check_metacharacters, resolve_cwd, tokenize, Rejection};

/// The pure-Rust core: takes a command string + optional subdir, returns a
/// structured result. No MCP types here — the [`ShellServer`] adapter wraps
/// this in MCP responses.
pub struct Engine {
    root: PathBuf,
    cache: ConfigCache,
}

impl Engine {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: into_normal(root.into()),
            cache: ConfigCache::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve and return the merged config for `subdir` (or the root).
    pub fn describe(&self, subdir: Option<&str>) -> Result<DescribeResult, EngineError> {
        let cwd = resolve_cwd(&self.root, subdir).map_err(EngineError::Rejection)?;
        let loaded = self.cache.get_or_load(&self.root, &cwd)?;
        Ok(DescribeResult::from_loaded(loaded))
    }

    /// Run the full pipeline: metacharacter check → tokenize → hard deny →
    /// resolve cwd → load config → allowlist → execute.
    pub async fn exec(
        &self,
        command: &str,
        subdir: Option<&str>,
    ) -> Result<ExecResult, EngineError> {
        check_metacharacters(command).map_err(EngineError::Rejection)?;
        let tokens = tokenize(command).map_err(EngineError::Rejection)?;
        check_hard_denylist(&tokens).map_err(EngineError::Rejection)?;
        let cwd = resolve_cwd(&self.root, subdir).map_err(EngineError::Rejection)?;
        let loaded = self.cache.get_or_load(&self.root, &cwd)?;
        let matched =
            loaded
                .allowlist
                .find_match(&tokens)
                .ok_or_else(|| EngineError::NotAllowed {
                    command: command.to_string(),
                    sources: loaded.sources.clone(),
                })?;
        let matched_rule = matched.raw().to_string();
        let matched_source = matched.source().to_string();
        let outcome = execute(&tokens, &ExecOptions::new(cwd.clone())).await?;
        Ok(ExecResult {
            outcome,
            cwd,
            matched_rule,
            matched_source,
        })
    }
}

/// Strip `.` and `..` lexically so the launch root is a stable string.
fn into_normal(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug)]
pub struct ExecResult {
    pub outcome: ExecOutcome,
    pub cwd: PathBuf,
    pub matched_rule: String,
    pub matched_source: String,
}

#[derive(Debug)]
pub struct DescribeResult {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub platform: &'static str,
    pub defaults_included: bool,
    pub rules: Vec<DescribedRule>,
    pub sources: Vec<PathBuf>,
}

impl DescribeResult {
    fn from_loaded(loaded: LoadedConfig) -> Self {
        Self {
            root: loaded.root,
            cwd: loaded.cwd,
            platform: platform_label(),
            defaults_included: loaded.defaults_included,
            rules: loaded
                .allowlist
                .rules()
                .iter()
                .map(|r| DescribedRule {
                    pattern: r.raw().to_string(),
                    source: r.source().to_string(),
                })
                .collect(),
            sources: loaded.sources,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DescribedRule {
    pub pattern: String,
    pub source: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Rejection(Rejection),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Exec(#[from] ExecError),

    #[error("command not in allowlist: `{command}`. Loaded config files: {sources:?}. Use `shell_describe` to inspect the active rules.")]
    NotAllowed {
        command: String,
        sources: Vec<PathBuf>,
    },
}

fn platform_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

// --------------------------------------------------------------------
// MCP wire types
// --------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShellExecRequest {
    /// Shell command to execute. Pipelines and redirections are not
    /// permitted in v0.1 — write a script and allowlist it instead.
    pub command: String,
    /// Optional subdirectory under the launch root in which to run the
    /// command. Must remain inside the launch root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShellDescribeRequest {
    /// Optional subdirectory under the launch root to introspect. Defaults
    /// to the launch root itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExecResponse<'a> {
    ok: bool,
    cwd: String,
    matched_rule: &'a str,
    matched_rule_source: &'a str,
    exit_code: Option<i32>,
    truncated: bool,
    timed_out: bool,
    stdout: &'a str,
    stderr: &'a str,
}

#[derive(Debug, Serialize)]
struct RejectionResponse<'a> {
    ok: bool,
    rejection: RejectionPayload<'a>,
}

#[derive(Debug, Serialize)]
struct RejectionPayload<'a> {
    kind: &'a str,
    message: String,
}

#[derive(Debug, Serialize)]
struct DescribeResponse<'a> {
    root: String,
    cwd: String,
    platform: &'a str,
    defaults_included: bool,
    rules: &'a [DescribedRule],
    config_files_loaded: Vec<String>,
}

// --------------------------------------------------------------------
// MCP server
// --------------------------------------------------------------------

#[derive(Clone)]
pub struct ShellServer {
    engine: Arc<Engine>,
    #[allow(dead_code)] // read by `#[tool_handler]`-generated code
    tool_router: ToolRouter<Self>,
}

impl ShellServer {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl ShellServer {
    #[tool(
        description = "Execute a shell command from the merged allowlist and return stdout, stderr, exit code, and a truncation flag. Rejects shell metacharacters; rejects sudo and other hard-denied commands; rejects commands not in the allowlist. Always run shell_describe first to see the active rules."
    )]
    async fn shell_exec(
        &self,
        Parameters(req): Parameters<ShellExecRequest>,
    ) -> Result<CallToolResult, McpError> {
        match self.engine.exec(&req.command, req.cwd.as_deref()).await {
            Ok(result) => {
                let body = ExecResponse {
                    ok: true,
                    cwd: result.cwd.display().to_string(),
                    matched_rule: &result.matched_rule,
                    matched_rule_source: &result.matched_source,
                    exit_code: result.outcome.exit_code,
                    truncated: result.outcome.truncated,
                    timed_out: result.outcome.timed_out,
                    stdout: &result.outcome.stdout,
                    stderr: &result.outcome.stderr,
                };
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&body).unwrap_or_else(|e| {
                        format!("{{\"ok\":false,\"serialization_error\":\"{e}\"}}")
                    }),
                )]))
            }
            Err(EngineError::Rejection(r)) => {
                let body = RejectionResponse {
                    ok: false,
                    rejection: RejectionPayload {
                        kind: r.kind().as_str(),
                        message: r.to_string(),
                    },
                };
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )]))
            }
            Err(EngineError::NotAllowed { .. }) => {
                let body = RejectionResponse {
                    ok: false,
                    rejection: RejectionPayload {
                        kind: "not_allowlisted",
                        message: format!(
                            "{}",
                            EngineError::NotAllowed {
                                command: req.command.clone(),
                                sources: vec![],
                            }
                        ),
                    },
                };
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )]))
            }
            Err(other) => Err(McpError::internal_error(other.to_string(), None)),
        }
    }

    #[tool(
        description = "Return the merged allowlist for the given subdirectory (or the launch root), the resolved working directory, platform, and the list of TOML files that were loaded in merge order. Call this first in any new session."
    )]
    async fn shell_describe(
        &self,
        Parameters(req): Parameters<ShellDescribeRequest>,
    ) -> Result<CallToolResult, McpError> {
        match self.engine.describe(req.cwd.as_deref()) {
            Ok(d) => {
                let body = DescribeResponse {
                    root: d.root.display().to_string(),
                    cwd: d.cwd.display().to_string(),
                    platform: d.platform,
                    defaults_included: d.defaults_included,
                    rules: &d.rules,
                    config_files_loaded: d
                        .sources
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect(),
                };
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )]))
            }
            Err(EngineError::Rejection(r)) => Err(McpError::invalid_params(r.to_string(), None)),
            Err(other) => Err(McpError::internal_error(other.to_string(), None)),
        }
    }
}

#[tool_handler]
impl ServerHandler for ShellServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "shell-mcp provides scoped, allowlisted shell access. Call `shell_describe` \
             first to see the active rules and the resolved working directory, then \
             `shell_exec` to run commands. Pipelines, redirections, and `sudo` are always \
             rejected; write commands require an explicit per-directory `.shell-mcp.toml` \
             allowlist.",
        )
    }
}
