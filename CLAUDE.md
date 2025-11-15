# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.


## Code Style Conventions

- **String conversions**: Use `.to_owned()` instead of `.to_string()` when converting `&str` or string slices to `String`. Reserve `.to_string()` for types that implement `Display` trait (like integers, floats, `Path::display()`, format results, etc.)

## Project Overview

`autogit` is an automatic git commit daemon with CLI configuration tool. It consists of three Rust workspace crates:

- **autogit-daemon**: Background daemon that monitors git repositories and automatically commits/pushes changes at intervals
- **autogit**: CLI tool for configuring the daemon (add/remove repos, set intervals, trigger manual runs)
- **autogit-shared**: Shared configuration types and TOML serialization logic

## Architecture

### Communication Between Components

1. **Configuration**: Both daemon and CLI read/write `~/.config/autogit/config.toml` (managed by `autogit-shared`)
2. **Hot-reload**: Daemon uses `notify` crate to watch config file for changes and reloads automatically
3. **Manual triggers**: CLI sends SIGUSR1 signal to daemon for `autogit now` command (triggers immediate check cycle)
4. **Desktop notifications**: Daemon sends notifications via `notify-rust` when git push/pull operations fail

### Daemon Event Loop

The daemon uses `tokio::select!` to handle multiple concurrent events:
- **Periodic timer**: Checks repositories at configured interval
- **SIGUSR1**: Manual trigger from `autogit now` command
- **Config reload channel**: Triggered by file watcher when config changes
- **SIGTERM/SIGINT**: Graceful shutdown

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

## Testing with systemd

```bash
# Install binaries
sudo cp target/release/autogit-daemon /usr/bin/
sudo cp target/release/autogit /usr/bin/

# Copy service file
mkdir -p ~/.config/systemd/user
cp autogit-daemon.service ~/.config/systemd/user/

# Essential: Import SSH agent for git operations
systemctl --user import-environment SSH_AUTH_SOCK

# Start daemon
systemctl --user daemon-reload
systemctl --user start autogit-daemon

# View logs
journalctl --user -u autogit-daemon -f

# After code changes: restart daemon
systemctl --user restart autogit-daemon
```

## Version Management

Version is centralized in workspace `Cargo.toml` at `[workspace.package].version`. All crates inherit this version. When bumping versions:

1. Update `Cargo.toml` workspace version
2. Commit and tag: `git tag v0.x.0 && git push origin v0.x.0`

## Key Implementation Details

- **Signal handling**: Daemon must handle SIGUSR1 for manual triggers (not just SIGTERM/SIGINT)
- **SSH agent**: Service file uses `PassEnvironment=SSH_AUTH_SOCK` - users must run `systemctl --user import-environment SSH_AUTH_SOCK` after login
- **Config path**: Always `~/.config/autogit/config.toml` (uses `dirs` crate)
- **Async runtime**: Uses tokio for daemon, but git operations run in `tokio::task::spawn_blocking` since they're synchronous
- **Desktop notifications**: Shows stderr output from failed git operations so users can see specific errors (SSH key issues, network problems, etc.)

