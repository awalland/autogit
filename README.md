[![codecov](https://codecov.io/github/awalland/autogit/graph/badge.svg?token=P5GON3Z8VH)](https://codecov.io/github/awalland/autogit)
[![Rust](https://github.com/awalland/autogit/actions/workflows/rust.yml/badge.svg)](https://github.com/awalland/autogit/actions/workflows/rust.yml)
# autogit

Automatic git commit daemon with a simple CLI configuration tool and system tray icon support.

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
> **⚠️ IMPORTANT: SSH Agent Setup**
>
> If you use SSH for git operations, you need to ensure the daemon has access to your SSH agent:
>
> ```bash
> systemctl --user import-environment SSH_AUTH_SOCK
> ```
>
> This command must be run every time you log in. To make it automatic, add it to:
> - Your shell profile (`~/.bash_profile`, `~/.zprofile`, `~/.bashrc`)
> - Or your desktop environment startup configuration (e.g., i3 config, KDE or GNOME startup applications)

### Commit Message Templates

You can use the following placeholders in commit message templates:

- `{timestamp}`: Full timestamp (e.g., "2025-11-15 14:30:00")
- `{date}`: Date only (e.g., "2025-11-15")
- `{time}`: Time only (e.g., "14:30:00")


## OpenSuSE Tumbleweed RPM installation

```bash
sudo zypper addrepo https://download.opensuse.org/repositories/home:/brezel/openSUSE_Tumbleweed/ home:brezel
sudo zypper refresh
sudo zypper install autogit
```

This will install the systemd service automatically, you just have to enable it (see below)

## Building

```bash
cargo build --release
```

The binaries will be available at:
- `target/release/autogit-daemon`
- `target/release/autogit`

## Manuall Installation

### Manuall installation
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
journalctl --user-unit=autogit-daemon -f
```


## Requirements

- Rust 1.85 or later
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

