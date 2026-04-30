//! Hard safety checks that the user's TOML configuration cannot override.
//!
//! Two layers live here:
//!
//! 1. Syntactic rejection of shell metacharacters (`;`, `&&`, `||`, `|`,
//!    backticks, `$()`, `>`, `<`, `>>`). v0.1 takes the position that
//!    composite shell pipelines must be expressed as scripts and the script
//!    itself allowlisted.
//! 2. A hard denylist of token patterns that no allowlist can re-enable
//!    (`sudo`, `rm -rf /`, classic fork bombs).
//!
//! Working-directory containment lives here as well: every command must
//! resolve to a path inside the launch root.
//!
//! These checks run *before* allowlist matching so that the user can never
//! accidentally write a TOML rule that lets a dangerous command through.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Substrings that immediately disqualify a command in v0.1.
///
/// Order matters only for diagnostic messages — the first hit wins.
const METACHARACTERS: &[&str] = &[
    "&&", "||", ">>", // multi-char first so we report the most specific match
    ";", "|", "`", "$(", ">", "<",
];

/// Token sequences that are always rejected, regardless of allowlist.
///
/// Each entry is a sequence of glob-free, exact-match tokens. The matcher
/// looks for these sequences anywhere in the parsed command tokens.
const HARD_DENY: &[&[&str]] = &[
    &["sudo"],
    &["doas"],
    &["su"],
    &["rm", "-rf", "/"],
    &["rm", "-rf", "/*"],
    &["rm", "-fr", "/"],
    &["rm", "--recursive", "--force", "/"],
    &[":(){", ":|:&", "};:"], // classic fork bomb tokenization
    &["mkfs"],
    &["mkfs.ext4"],
    &["dd", "if=/dev/zero"],
    &["dd", "if=/dev/random"],
    &["chmod", "-R", "777", "/"],
    &["chown", "-R", "root", "/"],
];

/// Why a command was refused.
#[derive(Debug, Error)]
pub enum Rejection {
    #[error("command rejected: shell metacharacter `{token}` is not allowed in v0.1 (write a script and allowlist it instead)")]
    Metacharacter { token: String },

    #[error("command rejected by hard denylist (rule: `{rule}`); this rule cannot be overridden by .shell-mcp.toml")]
    HardDeny { rule: String },

    #[error(
        "command rejected: requested working directory `{requested}` escapes launch root `{root}`"
    )]
    EscapesRoot { requested: String, root: String },

    #[error("command rejected: empty command")]
    Empty,

    #[error("command rejected: could not parse command tokens ({reason})")]
    ParseError { reason: String },
}

impl Rejection {
    pub fn kind(&self) -> RejectionKind {
        match self {
            Rejection::Metacharacter { .. } => RejectionKind::Metacharacter,
            Rejection::HardDeny { .. } => RejectionKind::HardDeny,
            Rejection::EscapesRoot { .. } => RejectionKind::EscapesRoot,
            Rejection::Empty => RejectionKind::Empty,
            Rejection::ParseError { .. } => RejectionKind::ParseError,
        }
    }
}

/// Stable categorisation suitable for serialising into MCP tool responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    Metacharacter,
    HardDeny,
    EscapesRoot,
    Empty,
    ParseError,
}

impl RejectionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectionKind::Metacharacter => "metacharacter",
            RejectionKind::HardDeny => "hard_deny",
            RejectionKind::EscapesRoot => "escapes_root",
            RejectionKind::Empty => "empty",
            RejectionKind::ParseError => "parse_error",
        }
    }
}

/// Reject any command containing the v0.1 metacharacter set.
pub fn check_metacharacters(raw: &str) -> Result<(), Rejection> {
    for token in METACHARACTERS {
        if raw.contains(token) {
            return Err(Rejection::Metacharacter {
                token: (*token).to_string(),
            });
        }
    }
    Ok(())
}

/// Tokenize the command using POSIX shell quoting rules so that quoted
/// arguments survive (`git commit -m "fix: thing"` becomes four tokens).
///
/// Metacharacter rejection runs first, so any pipeline syntax never reaches
/// this function in normal operation.
pub fn tokenize(raw: &str) -> Result<Vec<String>, Rejection> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Rejection::Empty);
    }
    shlex::split(trimmed).ok_or_else(|| Rejection::ParseError {
        reason: "unbalanced quotes".to_string(),
    })
}

/// Walk the parsed tokens looking for any hard-denied subsequence.
pub fn check_hard_denylist(tokens: &[String]) -> Result<(), Rejection> {
    for rule in HARD_DENY {
        if contains_subsequence(tokens, rule) {
            return Err(Rejection::HardDeny {
                rule: rule.join(" "),
            });
        }
    }
    Ok(())
}

/// True if `needle` appears as a contiguous subsequence of `haystack`.
fn contains_subsequence(haystack: &[String], needle: &[&str]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(h, n)| h == n))
}

/// Resolve `requested` against `root`, ensuring the result stays inside `root`.
///
/// `requested` may be `None` (use the root itself), a relative path (joined to
/// the root), or an absolute path (must already be inside the root). `..`
/// components are normalised lexically before the containment check so that
/// `subdir/../..` cannot escape.
pub fn resolve_cwd(root: &Path, requested: Option<&str>) -> Result<PathBuf, Rejection> {
    let root = normalize(root);
    let candidate = match requested {
        None | Some("") => root.clone(),
        Some(p) => {
            let p = Path::new(p);
            if p.is_absolute() {
                normalize(p)
            } else {
                normalize(&root.join(p))
            }
        }
    };
    if !candidate.starts_with(&root) {
        return Err(Rejection::EscapesRoot {
            requested: candidate.display().to_string(),
            root: root.display().to_string(),
        });
    }
    Ok(candidate)
}

/// Lexical path normalisation that collapses `.` and `..` without touching
/// the filesystem. We deliberately avoid `canonicalize` because it requires
/// the path to exist and resolves symlinks — neither is appropriate for the
/// cwd containment check.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metacharacters_are_rejected() {
        for bad in [
            "ls; rm -rf foo",
            "ls && rm foo",
            "ls || true",
            "ls | grep foo",
            "ls > out",
            "ls < in",
            "ls >> out",
            "echo `whoami`",
            "echo $(whoami)",
        ] {
            assert!(check_metacharacters(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn plain_commands_pass_metacharacter_check() {
        for good in ["ls -la", "git status", "cargo build --release"] {
            assert!(check_metacharacters(good).is_ok(), "should allow: {good}");
        }
    }

    #[test]
    fn sudo_is_always_denied() {
        let tokens = tokenize("sudo ls").unwrap();
        assert!(matches!(
            check_hard_denylist(&tokens),
            Err(Rejection::HardDeny { .. })
        ));
    }

    #[test]
    fn rm_rf_root_is_always_denied() {
        let tokens = tokenize("rm -rf /").unwrap();
        assert!(matches!(
            check_hard_denylist(&tokens),
            Err(Rejection::HardDeny { .. })
        ));
    }

    #[test]
    fn cwd_inside_root_is_accepted() {
        let root = PathBuf::from("/tmp/launch");
        assert_eq!(
            resolve_cwd(&root, Some("sub/dir")).unwrap(),
            PathBuf::from("/tmp/launch/sub/dir")
        );
        assert_eq!(resolve_cwd(&root, None).unwrap(), root);
    }

    #[test]
    fn cwd_escaping_root_is_rejected() {
        let root = PathBuf::from("/tmp/launch");
        assert!(resolve_cwd(&root, Some("../other")).is_err());
        assert!(resolve_cwd(&root, Some("sub/../../other")).is_err());
        assert!(resolve_cwd(&root, Some("/etc")).is_err());
    }
}
