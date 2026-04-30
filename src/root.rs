//! Resolution of the launch root.
//!
//! The launch root is the directory shell-mcp pins every executed command
//! into. v0.1.0 derived it from the process working directory, which broke
//! under Claude Desktop: Desktop launches MCP servers from an undefined
//! cwd (often `/` on macOS), so the safety boundary collapsed to the whole
//! filesystem.
//!
//! v0.1.1 takes the root from three sources, in this precedence order:
//!
//! 1. `--root <PATH>` CLI flag
//! 2. `SHELL_MCP_ROOT` environment variable
//! 3. The process's launch cwd (legacy behaviour, kept as a fallback for
//!    direct shell invocations).
//!
//! Whichever source wins, the path must already be **absolute**, must
//! exist, and must be a directory. We then canonicalize so symlinks are
//! resolved up front (otherwise the lexical containment check in
//! [`crate::safety::resolve_cwd`] would compare against an unresolved
//! prefix and a request for the symlink target would falsely escape).

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RootError {
    #[error("launch root must be an absolute path; got `{path}` (set --root or SHELL_MCP_ROOT)")]
    NotAbsolute { path: String },

    #[error("launch root does not exist: `{path}`")]
    DoesNotExist { path: String },

    #[error("launch root is not a directory: `{path}`")]
    NotDirectory { path: String },

    #[error("could not canonicalize launch root `{path}`: {source}")]
    Canonicalize {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read launch root `{path}`: {source}")]
    Stat {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Where the resolved root came from. Surfaced in logs so the operator can
/// tell which input was honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    Flag,
    Env,
    LaunchCwd,
}

impl RootSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RootSource::Flag => "--root flag",
            RootSource::Env => "SHELL_MCP_ROOT env var",
            RootSource::LaunchCwd => "launch cwd",
        }
    }
}

/// The chosen root plus the source it came from.
#[derive(Debug, Clone)]
pub struct ResolvedRoot {
    pub path: PathBuf,
    pub source: RootSource,
}

/// Pure resolution function so unit tests can drive every case without
/// touching the process environment.
///
/// `cli` is the value of `--root`; `env` is the value of `SHELL_MCP_ROOT`
/// (an empty string is treated as unset to match shell ergonomics);
/// `fallback_cwd` is what the process believes its cwd to be.
///
/// The launch-cwd fallback is intentionally permissive — it accepts
/// whatever the OS reports — because direct shell invocations
/// (`cd ~/proj && shell-mcp`) should still work without ceremony. The
/// strict absolute-path requirement only applies when the user explicitly
/// supplied a flag or env value.
pub fn resolve_root(
    cli: Option<&Path>,
    env: Option<&str>,
    fallback_cwd: &Path,
) -> Result<ResolvedRoot, RootError> {
    let (raw, source) = if let Some(p) = cli {
        (p.to_path_buf(), RootSource::Flag)
    } else if let Some(s) = env.filter(|s| !s.is_empty()) {
        (PathBuf::from(s), RootSource::Env)
    } else {
        (fallback_cwd.to_path_buf(), RootSource::LaunchCwd)
    };

    // Strict checks apply to user-supplied paths only.
    if matches!(source, RootSource::Flag | RootSource::Env) && !raw.is_absolute() {
        return Err(RootError::NotAbsolute {
            path: raw.display().to_string(),
        });
    }

    let metadata = match std::fs::metadata(&raw) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(RootError::DoesNotExist {
                path: raw.display().to_string(),
            });
        }
        Err(e) => {
            return Err(RootError::Stat {
                path: raw.display().to_string(),
                source: e,
            });
        }
    };
    if !metadata.is_dir() {
        return Err(RootError::NotDirectory {
            path: raw.display().to_string(),
        });
    }

    let path = raw.canonicalize().map_err(|e| RootError::Canonicalize {
        path: raw.display().to_string(),
        source: e,
    })?;

    Ok(ResolvedRoot { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn flag_overrides_env() {
        let flag_dir = tempdir().unwrap();
        let env_dir = tempdir().unwrap();
        let cwd_dir = tempdir().unwrap();
        let r = resolve_root(
            Some(flag_dir.path()),
            Some(env_dir.path().to_str().unwrap()),
            cwd_dir.path(),
        )
        .unwrap();
        assert_eq!(r.source, RootSource::Flag);
        assert_eq!(r.path, flag_dir.path().canonicalize().unwrap());
    }

    #[test]
    fn env_overrides_launch_cwd() {
        let env_dir = tempdir().unwrap();
        let cwd_dir = tempdir().unwrap();
        let r = resolve_root(None, Some(env_dir.path().to_str().unwrap()), cwd_dir.path()).unwrap();
        assert_eq!(r.source, RootSource::Env);
        assert_eq!(r.path, env_dir.path().canonicalize().unwrap());
    }

    #[test]
    fn empty_env_falls_through_to_launch_cwd() {
        let cwd_dir = tempdir().unwrap();
        let r = resolve_root(None, Some(""), cwd_dir.path()).unwrap();
        assert_eq!(r.source, RootSource::LaunchCwd);
    }

    #[test]
    fn launch_cwd_used_when_nothing_else_set() {
        let cwd_dir = tempdir().unwrap();
        let r = resolve_root(None, None, cwd_dir.path()).unwrap();
        assert_eq!(r.source, RootSource::LaunchCwd);
        assert_eq!(r.path, cwd_dir.path().canonicalize().unwrap());
    }

    #[test]
    fn nonexistent_path_rejected() {
        let parent = tempdir().unwrap();
        let missing = parent.path().join("does-not-exist");
        let cwd_dir = tempdir().unwrap();
        let err = resolve_root(Some(&missing), None, cwd_dir.path()).unwrap_err();
        assert!(matches!(err, RootError::DoesNotExist { .. }), "got {err:?}");
    }

    #[test]
    fn file_not_directory_rejected() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"hi").unwrap();
        let cwd_dir = tempdir().unwrap();
        let err = resolve_root(Some(&file), None, cwd_dir.path()).unwrap_err();
        assert!(matches!(err, RootError::NotDirectory { .. }), "got {err:?}");
    }

    #[test]
    fn relative_flag_path_rejected() {
        let cwd_dir = tempdir().unwrap();
        let rel = Path::new("relative/path");
        let err = resolve_root(Some(rel), None, cwd_dir.path()).unwrap_err();
        assert!(matches!(err, RootError::NotAbsolute { .. }), "got {err:?}");
    }

    #[test]
    fn relative_env_path_rejected() {
        let cwd_dir = tempdir().unwrap();
        let err = resolve_root(None, Some("also/relative"), cwd_dir.path()).unwrap_err();
        assert!(matches!(err, RootError::NotAbsolute { .. }), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_resolved() {
        use std::os::unix::fs::symlink;
        let real = tempdir().unwrap();
        let link_parent = tempdir().unwrap();
        let link = link_parent.path().join("alias");
        symlink(real.path(), &link).unwrap();

        let cwd_dir = tempdir().unwrap();
        let r = resolve_root(Some(&link), None, cwd_dir.path()).unwrap();
        assert_eq!(r.source, RootSource::Flag);
        // The resolved path must point at the real directory, not the link.
        assert_eq!(r.path, real.path().canonicalize().unwrap());
        assert_ne!(r.path, link);
    }
}
