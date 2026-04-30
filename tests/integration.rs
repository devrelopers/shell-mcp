//! End-to-end tests that drive the [`Engine`] the same way the MCP server
//! does. We deliberately exercise the full pipeline (metacharacter check →
//! tokenize → hard deny → cwd resolve → walks-up config → allowlist →
//! execute) so the tests catch regressions in any single layer.

use std::fs;
use std::path::Path;

use shell_mcp::safety::RejectionKind;
use shell_mcp::tools::{Engine, EngineError};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

#[tokio::test]
async fn happy_path_default_command_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::new(tmp.path());

    let cmd = if cfg!(windows) { "where where" } else { "pwd" };
    let result = engine
        .exec(cmd, None)
        .await
        .unwrap_or_else(|e| panic!("expected ok, got {e:?}"));
    assert_eq!(
        result.outcome.exit_code,
        Some(0),
        "stderr: {}",
        result.outcome.stderr
    );
    assert!(!result.outcome.truncated);
    assert!(!result.matched_rule.is_empty());
}

#[tokio::test]
async fn metacharacter_rejection() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::new(tmp.path());

    for bad in [
        "ls; rm foo",
        "ls && echo hi",
        "ls | grep x",
        "ls > out.txt",
        "echo `whoami`",
        "echo $(whoami)",
    ] {
        let err = engine.exec(bad, None).await.unwrap_err();
        match err {
            EngineError::Rejection(ref r) => {
                assert_eq!(r.kind(), RejectionKind::Metacharacter, "for input: {bad}");
            }
            other => panic!("expected Rejection for {bad}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn hard_denylist_blocks_sudo_even_if_allowlisted() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join(".shell-mcp.toml"),
        r#"allow = ["sudo **", "rm -rf /"]"#,
    );
    let engine = Engine::new(tmp.path());

    let err = engine.exec("sudo whoami", None).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Rejection(ref r) if r.kind() == RejectionKind::HardDeny),
        "got {err:?}"
    );

    let err = engine.exec("rm -rf /", None).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Rejection(ref r) if r.kind() == RejectionKind::HardDeny),
        "got {err:?}"
    );
}

#[tokio::test]
async fn unknown_command_is_not_allowlisted() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::new(tmp.path());
    let err = engine
        .exec("definitely-not-a-real-command --yolo", None)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::NotAllowed { .. }), "got {err:?}");
}

#[tokio::test]
async fn cwd_escape_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::new(tmp.path());
    let err = engine.exec("pwd", Some("../escape")).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Rejection(ref r) if r.kind() == RejectionKind::EscapesRoot),
        "got {err:?}"
    );
}

#[tokio::test]
async fn walks_up_merges_outer_with_inner() {
    let tmp = tempfile::tempdir().unwrap();
    let inner = tmp.path().join("project").join("sub");
    fs::create_dir_all(&inner).unwrap();

    write(
        &tmp.path().join(".shell-mcp.toml"),
        r#"allow = ["my-outer-tool **"]"#,
    );
    write(
        &tmp.path().join("project").join(".shell-mcp.toml"),
        r#"allow = ["my-mid-tool **"]"#,
    );
    write(
        &inner.join(".shell-mcp.toml"),
        r#"allow = ["my-inner-tool **"]"#,
    );

    let engine = Engine::new(tmp.path());
    let described = engine.describe(Some("project/sub")).unwrap();

    let patterns: Vec<&str> = described.rules.iter().map(|r| r.pattern.as_str()).collect();
    assert!(patterns.contains(&"my-outer-tool **"));
    assert!(patterns.contains(&"my-mid-tool **"));
    assert!(patterns.contains(&"my-inner-tool **"));

    // sources include the three files we wrote, in outermost-first order
    let in_tree: Vec<_> = described
        .sources
        .iter()
        .filter(|p| p.starts_with(tmp.path()))
        .collect();
    assert_eq!(in_tree.len(), 3);
    assert!(in_tree[0].parent().unwrap() == tmp.path());
    assert!(in_tree[2].parent().unwrap() == inner);
}

#[tokio::test]
async fn truncation_flag_is_set_on_long_output() {
    let tmp = tempfile::tempdir().unwrap();

    // Allowlist a command that prints many lines on the host platform.
    let toml = if cfg!(windows) {
        // On Windows, print 1000 lines via cmd's `for /L`.
        r#"
include_defaults = true
allow = ["cmd **"]
"#
    } else {
        // On Unix, use `seq 1 1000` (POSIX `seq` is on macOS and most Linuxes).
        r#"
include_defaults = true
allow = ["seq **"]
"#
    };
    write(&tmp.path().join(".shell-mcp.toml"), toml);
    let engine = Engine::new(tmp.path());

    let cmd = if cfg!(windows) {
        r#"cmd /C for /L %i in (1,1,1000) do @echo line-%i"#
    } else {
        "seq 1 1000"
    };

    // Skip on platforms that lack `seq` (uncommon, but be defensive).
    let result = match engine.exec(cmd, None).await {
        Ok(r) => r,
        Err(EngineError::Exec(_)) => {
            eprintln!("skipping: command unavailable on this host");
            return;
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    };

    assert!(
        result.outcome.truncated,
        "expected truncation flag to be set; stdout had {} bytes / {} lines",
        result.outcome.stdout.len(),
        result.outcome.stdout.lines().count(),
    );
    assert!(result.outcome.stdout.lines().count() <= shell_mcp::exec::MAX_LINES_PER_STREAM);
}

// --- Allowlisted-write tests --------------------------------------------
//
// `mkdir` is a clean cross-platform write command in spirit, but on Windows
// it's a `cmd.exe` builtin rather than a standalone executable on `PATH`.
// We split into per-platform tests so each one allowlists the actual
// program tokens that get spawned (and so the rejection test below uses a
// different real write command per platform).

#[cfg(unix)]
#[tokio::test]
async fn allowlisted_write_command_creates_directory_unix() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join(".shell-mcp.toml"),
        r#"allow = ["mkdir *"]"#,
    );
    let engine = Engine::new(tmp.path());

    let new_dir_name = "created-by-shell-mcp";
    let result = engine
        .exec(&format!("mkdir {new_dir_name}"), None)
        .await
        .unwrap_or_else(|e| panic!("expected ok, got {e:?}"));

    assert_eq!(
        result.outcome.exit_code,
        Some(0),
        "stderr was: {}",
        result.outcome.stderr
    );
    assert!(!result.outcome.truncated);
    assert_eq!(result.matched_rule, "mkdir *");

    let created = engine.root().join(new_dir_name);
    assert!(
        created.is_dir(),
        "expected `{}` to exist after mkdir",
        created.display()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn allowlisted_write_command_creates_directory_windows() {
    // On Windows we go through cmd.exe so we can use its `mkdir` builtin.
    // The allowlist names the *program* token (`cmd`) plus the exact
    // remaining arguments. We accept any directory name via `*` so the test
    // chooses the path it wants to create.
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join(".shell-mcp.toml"),
        r#"allow = ["cmd /C mkdir *"]"#,
    );
    let engine = Engine::new(tmp.path());

    let new_dir_name = "created-by-shell-mcp";
    let result = engine
        .exec(&format!("cmd /C mkdir {new_dir_name}"), None)
        .await
        .unwrap_or_else(|e| panic!("expected ok, got {e:?}"));

    assert_eq!(
        result.outcome.exit_code,
        Some(0),
        "stderr was: {}",
        result.outcome.stderr
    );
    assert!(!result.outcome.truncated);
    assert_eq!(result.matched_rule, "cmd /C mkdir *");

    let created = engine.root().join(new_dir_name);
    assert!(
        created.is_dir(),
        "expected `{}` to exist after cmd /C mkdir",
        created.display()
    );
}

// --- Rejection tests ----------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn unallowlisted_write_command_is_rejected_unix() {
    let tmp = tempfile::tempdir().unwrap();
    // Allowlist *only* `mkdir *` — `touch` should be denied.
    write(
        &tmp.path().join(".shell-mcp.toml"),
        r#"allow = ["mkdir *"]"#,
    );
    let engine = Engine::new(tmp.path());
    let cmd = "touch touched.txt";
    assert_rejected_as_not_allowlisted(&engine, tmp.path(), cmd, "touched.txt").await;
}

#[cfg(windows)]
#[tokio::test]
async fn unallowlisted_write_command_is_rejected_windows() {
    let tmp = tempfile::tempdir().unwrap();
    // Allowlist `cmd /C mkdir *` — `cmd /C copy ...` is a different rule
    // and should be denied even though it shares the `cmd` program token.
    write(
        &tmp.path().join(".shell-mcp.toml"),
        r#"allow = ["cmd /C mkdir *"]"#,
    );
    let engine = Engine::new(tmp.path());
    let cmd = "cmd /C copy nul touched.txt";
    assert_rejected_as_not_allowlisted(&engine, tmp.path(), cmd, "touched.txt").await;
}

async fn assert_rejected_as_not_allowlisted(
    engine: &Engine,
    root: &Path,
    cmd: &str,
    side_effect_path: &str,
) {
    let err = engine.exec(cmd, None).await.unwrap_err();
    let EngineError::NotAllowed { command, sources } = err else {
        panic!("expected NotAllowed for `{cmd}`, got: {err:?}");
    };
    assert_eq!(command, cmd);

    // The error must name the loaded TOML so the user knows *where* to add
    // a rule. We filter out any global `~/.shell-mcp.toml` that the host
    // machine running CI might happen to have.
    let toml_path = root.join(".shell-mcp.toml");
    let in_tree: Vec<_> = sources.iter().filter(|p| p.starts_with(root)).collect();
    assert_eq!(
        in_tree.len(),
        1,
        "expected the project's .shell-mcp.toml in the loaded sources, got {sources:?}"
    );
    assert_eq!(in_tree[0], &toml_path);

    // And the side effect must not have happened.
    assert!(
        !root.join(side_effect_path).exists(),
        "rejected command must not have run"
    );
}
