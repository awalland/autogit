Name:           autogit
# Version is automatically updated by build-rpm.sh from Cargo.toml
Version:        0.1.0
Release:        1%{?dist}
Summary:        Automatic git commit daemon with CLI configuration tool

License:        WTFPL
URL:            https://github.com/yourusername/autogit
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.70
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  openssl-devel
BuildRequires:  libgit2-devel

Requires:       git-core
Requires:       systemd

%description
autogit is a daemon that automatically commits changes in configured git
repositories at regular intervals. It includes a CLI tool for easy configuration
and integrates with systemd for service management.

Features:
- Automatic commits at configurable intervals
- Monitor multiple git repositories
- Simple CLI configuration interface
- Customizable commit message templates
- Respects .gitconfig for commit author info

%prep
%setup -q

%build
# Build release binaries
cargo build --release --locked

%install
# Create directories
mkdir -p %{buildroot}%{_bindir}
mkdir -p %{buildroot}%{_userunitdir}

# Install binaries
install -m 0755 target/release/autogit-daemon %{buildroot}%{_bindir}/autogit-daemon
install -m 0755 target/release/autogit %{buildroot}%{_bindir}/autogit

# Install systemd user service
install -m 0644 autogit-daemon.service %{buildroot}%{_userunitdir}/autogit-daemon.service

%files
%license LICENSE
%doc README.md
%{_bindir}/autogit-daemon
%{_bindir}/autogit
%{_userunitdir}/autogit-daemon.service

%post
# Inform user about systemd service
cat <<EOF

autogit has been installed successfully!

To enable and start the daemon:

  systemctl --user daemon-reload
  systemctl --user enable autogit-daemon
  systemctl --user start autogit-daemon

Configure repositories with:

  autogit add /path/to/your/repo
  autogit list
  autogit status

View daemon logs:

  journalctl --user -u autogit-daemon -f

For more information, see: man autogit or autogit --help

EOF

%preun
# Stop the service if it's running before uninstall
if [ $1 -eq 0 ]; then
    systemctl --user stop autogit-daemon 2>/dev/null || true
    systemctl --user disable autogit-daemon 2>/dev/null || true
fi

%postun
if [ $1 -eq 0 ]; then
    systemctl --user daemon-reload 2>/dev/null || true
fi

%changelog
* Sat Nov 15 2025 Armin Walland <armin@wal.land> - 0.1.0-1
- Initial RPM release
- Automatic git commit daemon
- CLI configuration tool
- systemd user service integration
