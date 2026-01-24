# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.


## Code Style Conventions

- **Ownership vs conversion**: Use `.to_owned()` as the primary way to make values owned (e.g., `&str` → `String`). Only use `.to_string()` when the goal is to convert a value to its string representation (e.g., `42.to_string()` or `path.display().to_string()`).

## Project Overview

`autogit` is an automatic git commit daemon with CLI configuration tool. It consists of three Rust workspace crates:

- **autogit-daemon**: Background daemon that monitors git repositories and automatically commits/pushes changes at intervals
- **autogit**: CLI tool for configuring the daemon (add/remove repos, set intervals, trigger manual runs)
- **autogit-shared**: Shared configuration types, TOML serialization, and daemon communication protocol (Unix socket)

## Architecture

### Communication Between Components

1. **Configuration**: Both daemon and CLI read/write `~/.config/autogit/config.toml` (managed by `autogit-shared`)
2. **Hot-reload**: Daemon uses `notify` crate to watch config file for changes and reloads automatically
3. **Unix Domain Socket**: Daemon listens on `~/.config/autogit/daemon.sock` for CLI commands
   - CLI sends JSON-encoded commands: `Trigger`, `Status`, `Ping`
   - Daemon responds with JSON-encoded responses including detailed results
   - Protocol defined in `autogit-shared/src/protocol.rs`
   - Socket is automatically cleaned up on daemon shutdown
4. **Desktop notifications**: Daemon sends notifications via `notify-rust` when git push/pull operations fail

**Important**: systemd is only used to start/stop the daemon. All daemon interaction (status checks, manual triggers) works via the Unix socket, so the daemon can be run manually without systemd if needed.

### Daemon Event Loop

The daemon uses `tokio::select!` to handle multiple concurrent events:
- **Periodic timer**: Checks repositories at configured interval
- **Unix socket connections**: Accepts incoming CLI commands (Trigger, Status, Ping)
  - Each connection is handled in a spawned tokio task
  - Commands are parsed from JSON, executed, and responses sent back
- **Config reload channel**: Triggered by file watcher when config changes
- **SIGTERM/SIGINT**: Graceful shutdown (also cleans up socket file)

### Git Operations Strategy

Git operations use `std::process::Command` to invoke git commands (not libgit2) for push/pull operations to properly inherit SSH_AUTH_SOCK from systemd environment. This ensures SSH authentication works when running as a systemd user service.

Key behaviors:
- **Initialization**: On startup, daemon commits any pending changes and runs `git pull --rebase`
- **Per-cycle**: Stage all changes → commit with templated message → push → pull/rebase
- **Non-fatal errors**: Push/pull failures are logged and notified but don't stop the daemon
- **Commit message templates**: Support `{timestamp}`, `{date}`, `{time}` placeholders (expanded in `autogit-daemon/src/git.rs`)

## Build & Development

```bash
# Build all workspace crates
cargo build --release

# Build produces stripped binaries (configured in workspace Cargo.toml)
# Binaries: target/release/autogit-daemon, target/release/autogit

# Run tests
cargo test

# Run daemon with debug logging
RUST_LOG=debug cargo run --bin autogit-daemon

# Test CLI commands
cargo run --bin autogit -- add ~/test-repo
cargo run --bin autogit -- list
cargo run --bin autogit -- now
```

## Shell Completions

Completions are auto-generated during build via `autogit/build.rs`:
- Uses `clap_complete` to generate bash, zsh, and fish completions
- Build script copies completions to `autogit/completions/` directory
- CLI definition is in `autogit/src/cli.rs` (shared with build.rs via `include!`)

## Version Management

Version is centralized in workspace `Cargo.toml` at `[workspace.package].version`. All crates inherit this version. When bumping versions:

1. Update `Cargo.toml` workspace version
2. Commit and tag: `git tag v0.x.0 && git push origin v0.x.0`

## Key Implementation Details

- **Socket communication**: Daemon communicates with CLI via Unix domain socket at `~/.config/autogit/daemon.sock`
  - Protocol uses line-delimited JSON (one command/response per line)
  - Enables bidirectional communication (daemon can return detailed results)
  - Socket existence = daemon running (no stale file issues)
- **SSH agent**: Service file uses `PassEnvironment=SSH_AUTH_SOCK` - users must run `systemctl --user import-environment SSH_AUTH_SOCK` after login
- **Config path**: Always `~/.config/autogit/config.toml` (uses `dirs` crate)
- **Socket path**: Always `~/.config/autogit/daemon.sock` (uses `dirs` crate)
- **Async runtime**: Uses tokio for daemon (and CLI for socket ops), but git operations run in `tokio::task::spawn_blocking` since they're synchronous
- **Desktop notifications**: Shows stderr output from failed git operations so users can see specific errors (SSH key issues, network problems, etc.)
- **systemd optional**: systemd is only used for starting/stopping the daemon. All other operations work via socket, so daemon can run without systemd

