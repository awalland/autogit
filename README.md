# autogit

Automatic git commit daemon with a simple CLI configuration tool.

## Overview

`autogit` consists of two components:

- **autogit-daemon**: A background daemon that watches configured git repositories and automatically commits changes at regular intervals
- **autogit**: A CLI tool for configuring the daemon

## Usage

### Adding a Repository

```bash
# Add a repository with default settings
autogit add ~/projects/notes

# Add with custom commit message
autogit add ~/documents/journal -m "Journal update: {timestamp}"

# Add and set check interval to 60 seconds
autogit add ~/code/drafts -i 60
```

### Listing Repositories

```bash
autogit list
```

### Removing a Repository

```bash
autogit remove ~/projects/notes
```

### Enable/Disable Auto-Commit

```bash
# Disable auto-commit for a repository (keeps it in config)
autogit disable ~/projects/notes

# Re-enable it
autogit enable ~/projects/notes
```

### Setting Global Check Interval

```bash
# Set interval to 5 minutes (300 seconds)
autogit interval 300
```

### Viewing Configuration

```bash
# Show current configuration
autogit status

# Edit configuration file directly
autogit edit
```

## Configuration

Configuration is stored at `~/.config/autogit/config.toml`.

Example configuration:

```toml
[daemon]
check_interval_seconds = 300

[[repositories]]
path = "/home/user/notes"
auto_commit = true
commit_message_template = "Auto-commit: {timestamp}"

[[repositories]]
path = "/home/user/journal"
auto_commit = true
commit_message_template = "Journal update: {date}"
```

### Commit Message Templates

You can use the following placeholders in commit message templates:

- `{timestamp}`: Full timestamp (e.g., "2025-11-15 14:30:00")
- `{date}`: Date only (e.g., "2025-11-15")
- `{time}`: Time only (e.g., "14:30:00")

## Features

- 🔄 Automatic commits at configurable intervals
- 📁 Monitor multiple git repositories
- ⚙️ Simple CLI configuration interface
- 🎨 Customizable commit message templates
- 🔐 Respects your `.gitconfig` for commit author info
- 📝 Structured logging with `tracing`
- 🛡️ Graceful shutdown on SIGTERM/SIGINT
- 🔃 Automatic `git pull --rebase` before committing to sync across devices
- 🔥 Hot-reload configuration without restarting the daemon

## Building

### Building from Source

```bash
cargo build --release
```

The binaries will be available at:
- `target/release/autogit-daemon`
- `target/release/autogit`

### Building RPM Package

For RPM-based distributions (Fedora, RHEL, openSUSE, etc.):

```bash
# Install build dependencies
sudo dnf install rpm-build rpmdevtools rust cargo gcc openssl-devel libgit2-devel

# Build the RPM (version is automatically read from Cargo.toml)
./build-rpm.sh
```

The script will:
1. Create the source tarball
2. Set up RPM build directories
3. Build the RPM package
4. Output the location of the built RPM

Install the generated RPM:

```bash
# Find the built RPM (output by build-rpm.sh)
sudo dnf install ~/rpmbuild/RPMS/x86_64/autogit-*.rpm

# Or with rpm directly
sudo rpm -ivh ~/rpmbuild/RPMS/x86_64/autogit-*.rpm
```

## Installation

### From RPM Package

If you built or downloaded an RPM package:

```bash
sudo dnf install autogit-*.rpm
```

After installation, enable the daemon:

```bash
systemctl --user daemon-reload
systemctl --user enable --now autogit-daemon
```

### Manual Installation

```bash
# Build the project
cargo build --release

# Install binaries
sudo cp target/release/autogit-daemon /usr/bin/
sudo cp target/release/autogit /usr/bin/
```

### systemd User Service Setup

Create a systemd user service file at `~/.config/systemd/user/autogit-daemon.service`:

```ini
[Unit]
Description=Automatic Git Commit Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/autogit-daemon
Restart=on-failure
RestartSec=10

# Logging
StandardOutput=journal
StandardError=journal

# Optional: Set log level
Environment="RUST_LOG=info"
# Inherit SSH agent socket from user session
PassEnvironment=SSH_AUTH_SOCK

[Install]
WantedBy=default.target
```

Enable and start the service:

```bash
# Import SSH agent socket to systemd user environment
systemctl --user import-environment SSH_AUTH_SOCK

# Reload systemd user daemon
systemctl --user daemon-reload

# Enable the service to start on boot
systemctl --user enable autogit-daemon

# Start the service
systemctl --user start autogit-daemon

# Check status
systemctl --user status autogit-daemon

# View logs
journalctl --user -u autogit-daemon -f
```

**Note:** The `systemctl --user import-environment SSH_AUTH_SOCK` command needs to be run every time you log in. Add it to your shell profile (`~/.bash_profile`, `~/.zprofile`) or desktop environment startup configuration (e.g., i3 config) to make it automatic.

## Requirements

- Rust 1.70 or later
- Git (with `user.name` and `user.email` configured)
- Linux (for systemd integration)

## Development

### Running Tests

```bash
cargo test
```

### Running with Debug Logging

```bash
RUST_LOG=debug cargo run --bin autogit-daemon
```

### Manual Testing

```bash
# Start the daemon manually
cargo run --bin autogit-daemon

# In another terminal, configure repositories
cargo run --bin autogit -- add /path/to/repo
cargo run --bin autogit -- list
```

## Troubleshooting

### Daemon Not Starting

Check the logs:
```bash
journalctl --user -u autogit-daemon -n 50
```

### Repository Not Being Committed

1. Verify the repository is enabled: `autogit list`
2. Check daemon logs for errors
3. Ensure git is configured: `git config user.name` and `git config user.email`
4. Verify the path exists and is a git repository

### Changes Not Detected

The daemon checks at the configured interval. You can:
- Reduce the check interval: `autogit interval 60`
- Check if files are in `.gitignore`
- Verify the daemon is running: `systemctl --user status autogit-daemon`

## License

WTFPL - Do What The Fuck You Want To Public License

See the [LICENSE](LICENSE) file for details.

## TODO

- [ ] Implement real-time file watching with `notify` crate for more responsive commits
- [ ] Web UI for configuration
- [ ] Commit hooks integration
- [ ] Configurable command to execute when merge/rebase conflicts occur (e.g., send notification, run custom conflict resolution script)
- [ ] Per-repository push/pull configuration (some repos may be local-only)
- [ ] Support for multiple remotes
