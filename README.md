# shell-mcp

[![CI](https://github.com/devrelopers/shell-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/devrelopers/shell-mcp/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/shell-mcp.svg)](https://crates.io/crates/shell-mcp)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Scoped, allowlisted shell access for Claude Desktop and other MCP clients.

`shell-mcp` is a small Rust binary that speaks the [Model Context Protocol]
over stdio. It exposes two tools — `shell_exec` and `shell_describe` — and
enforces a strict, layered safety model so you can hand a Claude session
useful read access by default and opt in to write access per directory.

[Model Context Protocol]: https://modelcontextprotocol.io

## Why another shell server?

Most "shell" MCP servers either run anything the model asks (scary) or
require you to enumerate every command up front (tedious). `shell-mcp` takes
a middle path:

- A curated, platform-aware **read-only allowlist** is on by default
  (`ls`, `git status`, `cargo metadata`, etc.).
- **Write commands require an explicit `.shell-mcp.toml`** in the project,
  with shell-style glob patterns (`cargo build **`).
- Configuration files are discovered by **walking up the directory tree
  like git does**, so a workspace can layer rules over a repo over a global
  default in `~/.shell-mcp.toml`.
- A small **hard denylist** (`sudo`, `rm -rf /`, fork bombs) is enforced
  *before* the allowlist and cannot be overridden.
- All shell metacharacters (`; && || | $() backticks > < >>`) are rejected.
  If you need a pipeline, write a script and allowlist the script.

## Install

```sh
cargo install shell-mcp
```

This drops a `shell-mcp` binary on your `PATH`.

## Wire it into Claude Desktop

Edit your Claude Desktop config (`~/Library/Application Support/Claude/claude_desktop_config.json`
on macOS; `%APPDATA%\Claude\claude_desktop_config.json` on Windows):

```json
{
  "mcpServers": {
    "shell": {
      "command": "shell-mcp",
      "args": ["--root", "/Users/you/code/your-project"]
    }
  }
}
```

Or use the env var form, which is equivalent:

```json
{
  "mcpServers": {
    "shell": {
      "command": "shell-mcp",
      "env": { "SHELL_MCP_ROOT": "/Users/you/code/your-project" }
    }
  }
}
```

> **Heads up: setting `cwd` in your Desktop MCP config does NOT scope
> `shell-mcp`.** Claude Desktop launches MCP servers from an undefined
> working directory (often `/` on macOS), and `cwd` in the Desktop config
> is not honoured for stdio servers. **Always pass `--root` or set
> `SHELL_MCP_ROOT`** when running under Desktop — otherwise the safety
> boundary collapses to the whole filesystem.

Restart Claude Desktop and the `shell_exec` and `shell_describe` tools will
be available.

### Launch-root precedence

`shell-mcp` resolves the launch root from these sources, highest precedence
first:

1. `--root <PATH>` CLI flag
2. `SHELL_MCP_ROOT` environment variable
3. The process's current working directory at launch (fine for direct
   shell invocations; **unsafe under Claude Desktop** — see above)

A user-supplied path (flag or env) **must be absolute, must exist, and
must be a directory**. The chosen path is canonicalized so symlinks are
resolved up front.

## Tools

### `shell_describe`

```jsonc
{ "cwd": "optional/relative/subdir" }
```

Returns the merged allowlist for the given subdirectory (or the launch
root), the resolved working directory, the platform label, and the list of
TOML files that were loaded in merge order. **Call this first in every new
session** so the model can see what it's allowed to run.

### `shell_exec`

```jsonc
{
  "command": "git status --short",
  "cwd": "optional/relative/subdir"
}
```

Returns:

```jsonc
{
  "ok": true,
  "cwd": "/abs/path/where/it/ran",
  "matched_rule": "git status **",
  "matched_rule_source": "/abs/path/.shell-mcp.toml",
  "exit_code": 0,
  "truncated": false,
  "timed_out": false,
  "stdout": "...",
  "stderr": ""
}
```

If the command is rejected, `ok: false` and a `rejection` block names the
layer that refused it (`metacharacter`, `hard_deny`, `escapes_root`,
`not_allowlisted`).

## Configuration

A `.shell-mcp.toml` file looks like this:

```toml
include_defaults = true

allow = [
  "cargo build",
  "cargo build **",
  "git commit -m **",
  "./scripts/deploy.sh **",
]
```

**Pattern syntax** (one entry = one shell-tokenized pattern):

| Pattern | Matches |
| --- | --- |
| `git status` | exactly `git status` |
| `cargo build *` | `cargo build` plus exactly one more argument |
| `cargo build **` | `cargo build` plus any number of arguments (incl. zero) |
| `cargo test foo??` | `cargo test foo` plus any two characters |

`**` only acts as a rest-matcher when it's the **final** token.

**Discovery and merging**:

1. Start at the working directory `shell-mcp` is asked to run a command in.
2. Walk up to filesystem root collecting every `.shell-mcp.toml`.
3. Prepend `~/.shell-mcp.toml` if present.
4. Merge outermost-first; the innermost file wins for `include_defaults`,
   and rules from every file are concatenated.

The merge result is cached per `(launch_root, cwd)` pair.

## Safety model in one paragraph

`shell-mcp` runs commands by spawning the program directly with discrete
arguments — **no shell is invoked**. Any input containing shell
metacharacters is rejected outright before parsing. Tokenized commands are
checked against a small hard denylist (`sudo`, `rm -rf /`, etc.) that no
user TOML can override. The working directory is normalized lexically and
forced to stay inside the launch root. Only after all of that does the
allowlist matcher decide whether the command runs. Output is captured
separately for stdout and stderr, normalized from CRLF, and clipped at
200 lines or 8 KB per stream with an explicit `truncated` flag.

## Default allowlist

**Unix (macOS + Linux):**
`ls`, `cat`, `head`, `tail`, `wc`, `grep`, `rg`, `find`, `tree`, `file`,
`stat`, `pwd`, `which`, `echo`, `env`, `git status|log|diff|show|branch`,
`git remote -v`, `cargo metadata|tree|--version`, `rustc --version`.

**Windows:**
`dir`, `type`, `findstr`, `where`, `tree /F`, `git status|log|diff|show|branch`,
`git remote -v`, `cargo metadata|tree|--version`, `rustc --version`, `whoami`.

## Building from source

```sh
git clone https://github.com/devrelopers/shell-mcp
cd shell-mcp
cargo build --release
./target/release/shell-mcp --root .
```

Run the tests:

```sh
cargo test
```

CI runs the full matrix on Ubuntu, macOS, and Windows on every push.

## Status

v0.1.0. The MCP wire shape and the TOML schema are stable for the v0.1
series. Pipelines, environment variable controls, and per-rule timeouts are
on the v0.2 roadmap.

## License

MIT — see [LICENSE](LICENSE).
