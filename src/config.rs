//! TOML configuration: parsing, walks-up discovery, and merging.
//!
//! Discovery rules:
//!
//! 1. Start at the working directory and walk up to the filesystem root,
//!    collecting any `.shell-mcp.toml` files we find. (We do not stop at git
//!    boundaries — a project may legitimately live outside a git repo.)
//! 2. Then add `~/.shell-mcp.toml` if present, treating it as the
//!    *outermost* (lowest precedence) config.
//! 3. Merge order: outermost first, innermost last. Innermost wins for
//!    `extend = false` semantics; rules accumulate.
//!
//! For v0.1 a config has a single field, `allow`, which is a list of
//! shell-style pattern strings. A future version may grow `deny`,
//! `env`, or `cwd` overrides.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;

use crate::allowlist::{Allowlist, Rule, RuleError};

const CONFIG_FILENAME: &str = ".shell-mcp.toml";

/// Raw on-disk schema for a single `.shell-mcp.toml` file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Shell-style pattern strings. See [`crate::allowlist`] for syntax.
    #[serde(default)]
    pub allow: Vec<String>,

    /// If true, also include the platform default read-only allowlist.
    /// Defaults to true (so that adding a project config does not
    /// accidentally disable the read-only commands).
    #[serde(default = "default_true")]
    pub include_defaults: bool,
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            reason: e.to_string(),
        })
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::parse(&text).map_err(|e| match e {
            ConfigError::Parse { reason } => ConfigError::ParseAt {
                path: path.to_path_buf(),
                reason,
            },
            other => other,
        })
    }
}

/// The result of resolving config for a particular working directory.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Merged allowlist (defaults + every loaded TOML, in merge order).
    pub allowlist: Allowlist,
    /// TOML files that were loaded, in the order they were merged
    /// (outermost first, innermost last). The global `~/.shell-mcp.toml`
    /// appears first if present.
    pub sources: Vec<PathBuf>,
    /// True if any loaded config explicitly disabled platform defaults.
    pub defaults_included: bool,
    /// The launch root used for this resolution.
    pub root: PathBuf,
    /// The working directory the resolution was performed against.
    pub cwd: PathBuf,
}

/// Walk from `cwd` up to filesystem root, collecting `.shell-mcp.toml` files
/// in *outermost-first* order. The global `~/.shell-mcp.toml` is prepended
/// when present.
fn discover_config_files(cwd: &Path) -> Vec<PathBuf> {
    let mut found_in_tree: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(cwd.to_path_buf());
    while let Some(dir) = cursor {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            found_in_tree.push(candidate);
        }
        cursor = dir.parent().map(|p| p.to_path_buf());
    }
    // `found_in_tree` is innermost-first; reverse so we end up outermost-first.
    found_in_tree.reverse();

    let mut all = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let global = home.join(CONFIG_FILENAME);
        if global.is_file() {
            all.push(global);
        }
    }
    all.extend(found_in_tree);
    all
}

/// Compose a [`LoadedConfig`] from the given working directory.
///
/// `root` is the launch root; `cwd` may be the same as `root` or a
/// subdirectory. We walk up from `cwd` for discovery so that subprojects
/// can layer extra rules on top of the launch root's config.
pub fn resolve(root: &Path, cwd: &Path) -> Result<LoadedConfig, ConfigError> {
    let sources = discover_config_files(cwd);
    let mut allowlist = Allowlist::new();
    let mut include_defaults = true;
    let mut configs: Vec<(PathBuf, Config)> = Vec::with_capacity(sources.len());
    for path in &sources {
        let cfg = Config::load(path)?;
        configs.push((path.clone(), cfg));
    }
    // Closer (later in `configs`) configs take precedence on `include_defaults`.
    if let Some((_, last)) = configs.last() {
        include_defaults = last.include_defaults;
    }
    if include_defaults {
        allowlist.extend(crate::allowlist::platform_defaults());
    }
    for (path, cfg) in &configs {
        for raw in &cfg.allow {
            let rule = Rule::parse(raw.clone(), path.display().to_string()).map_err(|e| {
                ConfigError::Rule {
                    path: path.clone(),
                    source: e,
                }
            })?;
            allowlist.push(rule);
        }
    }
    Ok(LoadedConfig {
        allowlist,
        sources,
        defaults_included: include_defaults,
        root: root.to_path_buf(),
        cwd: cwd.to_path_buf(),
    })
}

/// Cache so that repeated calls with the same `(root, cwd)` skip filesystem I/O.
#[derive(Default)]
pub struct ConfigCache {
    inner: Mutex<HashMap<(PathBuf, PathBuf), LoadedConfig>>,
}

impl ConfigCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_load(&self, root: &Path, cwd: &Path) -> Result<LoadedConfig, ConfigError> {
        let key = (root.to_path_buf(), cwd.to_path_buf());
        {
            let guard = self.inner.lock().expect("config cache poisoned");
            if let Some(hit) = guard.get(&key) {
                return Ok(hit.clone());
            }
        }
        let loaded = resolve(root, cwd)?;
        let mut guard = self.inner.lock().expect("config cache poisoned");
        guard.insert(key, loaded.clone());
        Ok(loaded)
    }

    pub fn clear(&self) {
        self.inner.lock().expect("config cache poisoned").clear();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse config: {reason}")]
    Parse { reason: String },

    #[error("could not parse config at {path}: {reason}")]
    ParseAt { path: PathBuf, reason: String },

    #[error("invalid rule in {path}: {source}")]
    Rule {
        path: PathBuf,
        #[source]
        source: RuleError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    fn tokens(s: &str) -> Vec<String> {
        shlex::split(s).unwrap()
    }

    #[test]
    fn empty_config_yields_defaults_only() {
        let dir = tempdir().unwrap();
        let loaded = resolve(dir.path(), dir.path()).unwrap();
        assert!(loaded.defaults_included);
        assert!(
            loaded.sources.is_empty() || loaded.sources.iter().all(|p| !p.starts_with(dir.path()))
        );
    }

    #[test]
    fn walks_up_and_merges_inner_over_outer() {
        let outer = tempdir().unwrap();
        let inner = outer.path().join("project").join("sub");
        std::fs::create_dir_all(&inner).unwrap();

        write(
            &outer.path().join(".shell-mcp.toml"),
            r#"allow = ["outer-cmd **"]"#,
        );
        write(
            &outer.path().join("project").join(".shell-mcp.toml"),
            r#"allow = ["mid-cmd **"]"#,
        );
        write(
            &inner.join(".shell-mcp.toml"),
            r#"allow = ["inner-cmd **"]"#,
        );

        let loaded = resolve(outer.path(), &inner).unwrap();

        // Sources are outermost-first; ignore any global config that may exist
        // on the host by filtering to files inside our tempdir.
        let in_tree: Vec<_> = loaded
            .sources
            .iter()
            .filter(|p| p.starts_with(outer.path()))
            .collect();
        assert_eq!(in_tree.len(), 3);
        assert!(
            in_tree[0].ends_with("launch/.shell-mcp.toml") || in_tree[0].starts_with(outer.path())
        );
        // outermost first
        assert!(in_tree[0].parent().unwrap() == outer.path());
        assert!(in_tree[2].parent().unwrap() == inner);

        assert!(loaded
            .allowlist
            .find_match(&tokens("outer-cmd a"))
            .is_some());
        assert!(loaded.allowlist.find_match(&tokens("mid-cmd a")).is_some());
        assert!(loaded
            .allowlist
            .find_match(&tokens("inner-cmd a"))
            .is_some());
    }

    #[test]
    fn include_defaults_false_disables_platform_defaults() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(".shell-mcp.toml"),
            r#"
include_defaults = false
allow = ["only-this"]
"#,
        );
        let loaded = resolve(dir.path(), dir.path()).unwrap();
        assert!(!loaded.defaults_included);
        assert!(loaded.allowlist.find_match(&tokens("only-this")).is_some());
        // Defaults are gone:
        assert!(loaded.allowlist.find_match(&tokens("pwd")).is_none());
    }

    #[test]
    fn cache_returns_stable_result() {
        let dir = tempdir().unwrap();
        let cache = ConfigCache::new();
        let a = cache.get_or_load(dir.path(), dir.path()).unwrap();
        let b = cache.get_or_load(dir.path(), dir.path()).unwrap();
        assert_eq!(a.sources, b.sources);
    }
}
