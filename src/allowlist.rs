//! Allowlist representation and matching.
//!
//! A [`Rule`] is an ordered list of glob patterns. Each pattern matches one
//! command token positionally, with one exception: a trailing `**` matches
//! zero or more remaining tokens. So `cargo build **` accepts every
//! `cargo build ...` invocation, while `cargo build *` accepts exactly one
//! extra argument.
//!
//! Patterns use the [`glob`] crate's syntax: `*` matches anything within a
//! single token, `?` matches one character, `[abc]` matches a character class.

use std::fmt;

use glob::Pattern;

/// A single allowlist entry, parsed once and matched many times.
#[derive(Clone)]
pub struct Rule {
    /// The original textual form, kept for diagnostics and `shell_describe`.
    raw: String,
    /// Compiled glob patterns, one per token.
    patterns: Vec<Pattern>,
    /// True if the final pattern is the literal `**` rest-matcher.
    rest_match: bool,
    /// Where the rule came from (TOML path or `<defaults>`).
    source: String,
}

impl Rule {
    /// Parse a single allowlist entry like `git log --oneline *`.
    pub fn parse(raw: impl Into<String>, source: impl Into<String>) -> Result<Self, RuleError> {
        let raw = raw.into();
        let source = source.into();
        let tokens = shlex::split(raw.trim()).ok_or_else(|| RuleError::Parse {
            raw: raw.clone(),
            reason: "unbalanced quotes".to_string(),
        })?;
        if tokens.is_empty() {
            return Err(RuleError::Parse {
                raw: raw.clone(),
                reason: "empty rule".to_string(),
            });
        }
        let rest_match = tokens.last().map(|t| t == "**").unwrap_or(false);
        let pattern_tokens = if rest_match {
            &tokens[..tokens.len() - 1]
        } else {
            &tokens[..]
        };
        let patterns = pattern_tokens
            .iter()
            .map(|t| {
                Pattern::new(t).map_err(|e| RuleError::Parse {
                    raw: raw.clone(),
                    reason: format!("invalid glob `{t}`: {e}"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            raw,
            patterns,
            rest_match,
            source,
        })
    }

    /// True if every token in `cmd` is matched by the corresponding pattern.
    pub fn matches(&self, cmd: &[String]) -> bool {
        if self.rest_match {
            if cmd.len() < self.patterns.len() {
                return false;
            }
        } else if cmd.len() != self.patterns.len() {
            return false;
        }
        self.patterns
            .iter()
            .zip(cmd.iter())
            .all(|(pat, tok)| pat.matches(tok))
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rule")
            .field("raw", &self.raw)
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("could not parse rule `{raw}`: {reason}")]
    Parse { raw: String, reason: String },
}

/// An ordered collection of rules. Rules are matched in order; the first
/// match wins (used to surface *which* rule allowed a command).
#[derive(Clone, Debug, Default)]
pub struct Allowlist {
    rules: Vec<Rule>,
}

impl Allowlist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_rules(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    pub fn push(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    pub fn extend(&mut self, other: Allowlist) {
        self.rules.extend(other.rules);
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Return the first rule that matches, or `None` if the command is denied.
    pub fn find_match(&self, cmd: &[String]) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matches(cmd))
    }
}

/// Default *read-only* allowlist for the current platform.
///
/// "Read-only" is a descriptive label, not a guarantee — `git log` reads from
/// `.git`, but technically `cargo metadata` may write to a target dir cache.
/// The intent is to ship a useful exploration toolkit out of the box.
pub fn platform_defaults() -> Allowlist {
    let raw_rules: &[&str] = if cfg!(windows) {
        &[
            "dir",
            "dir **",
            "type *",
            "type **",
            "findstr **",
            "where *",
            "where **",
            "tree",
            "tree /F",
            "tree /F **",
            "git status",
            "git status **",
            "git log",
            "git log **",
            "git diff",
            "git diff **",
            "git show",
            "git show **",
            "git branch",
            "git branch **",
            "git remote -v",
            "cargo metadata",
            "cargo metadata **",
            "cargo tree",
            "cargo tree **",
            "cargo --version",
            "rustc --version",
            "whoami",
        ]
    } else {
        &[
            "ls",
            "ls **",
            "cat *",
            "cat **",
            "head *",
            "head **",
            "tail *",
            "tail **",
            "wc *",
            "wc **",
            "grep **",
            "rg **",
            "find **",
            "tree",
            "tree **",
            "file *",
            "file **",
            "stat *",
            "stat **",
            "pwd",
            "which *",
            "which **",
            "echo",
            "echo **",
            "env",
            "git status",
            "git status **",
            "git log",
            "git log **",
            "git diff",
            "git diff **",
            "git show",
            "git show **",
            "git branch",
            "git branch **",
            "git remote -v",
            "cargo metadata",
            "cargo metadata **",
            "cargo tree",
            "cargo tree **",
            "cargo --version",
            "rustc --version",
        ]
    };
    let rules = raw_rules
        .iter()
        .map(|r| Rule::parse(*r, "<defaults>").expect("default rules must parse"))
        .collect();
    Allowlist::from_rules(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &str) -> Vec<String> {
        shlex::split(s).unwrap()
    }

    #[test]
    fn exact_match() {
        let r = Rule::parse("git status", "test").unwrap();
        assert!(r.matches(&tokens("git status")));
        assert!(!r.matches(&tokens("git status --short")));
    }

    #[test]
    fn single_glob_matches_one_token() {
        let r = Rule::parse("cargo build *", "test").unwrap();
        assert!(r.matches(&tokens("cargo build --release")));
        assert!(!r.matches(&tokens("cargo build")));
        assert!(!r.matches(&tokens("cargo build --release --offline")));
    }

    #[test]
    fn double_star_matches_rest() {
        let r = Rule::parse("git log **", "test").unwrap();
        assert!(r.matches(&tokens("git log")));
        assert!(r.matches(&tokens("git log --oneline -n 5")));
    }

    #[test]
    fn defaults_allow_pwd() {
        let al = platform_defaults();
        if !cfg!(windows) {
            assert!(al.find_match(&tokens("pwd")).is_some());
            assert!(al.find_match(&tokens("git status")).is_some());
            assert!(al.find_match(&tokens("git log --oneline")).is_some());
        }
    }

    #[test]
    fn unknown_commands_are_denied_by_default() {
        let al = platform_defaults();
        assert!(al.find_match(&tokens("dangerous-thing --yolo")).is_none());
    }
}
