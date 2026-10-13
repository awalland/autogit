Name:           autogit
Version:        0.9.0
Release:        1
Summary:        Automatic Git commit and synchronisation daemon
License:        WTFPL
URL:            https://github.com/awalland/autogit
Source0:        %{name}-%{version}.tar.gz
BuildRequires:  cargo >= 1.89
BuildRequires:  rust >= 1.89
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  pkgconf-pkg-config
BuildRequires:  libopenssl-devel
BuildRequires:  libssh2-devel
BuildRequires:  zlib-devel
BuildRequires:  systemd-rpm-macros
Requires:       git

%description
Autogit periodically commits changes in configured Git repositories and
synchronises them with their remotes. It includes a configuration CLI,
an optional system tray interface, and a systemd user service.

%prep
%setup -q

%build
cargo build --release --frozen --workspace

%check
cargo test --frozen --workspace

%install
install -Dm755 target/release/autogit %{buildroot}%{_bindir}/autogit
install -Dm755 target/release/autogit-daemon %{buildroot}%{_bindir}/autogit-daemon
install -Dm644 autogit-daemon.service %{buildroot}%{_userunitdir}/autogit-daemon.service
install -Dm644 autogit/completions/autogit.bash %{buildroot}%{_datadir}/bash-completion/completions/autogit
install -Dm644 autogit/completions/_autogit %{buildroot}%{_datadir}/zsh/site-functions/_autogit
install -Dm644 autogit/completions/autogit.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/autogit.fish

%files
%license LICENSE
%doc README.md
%{_bindir}/autogit
%{_bindir}/autogit-daemon
%{_userunitdir}/autogit-daemon.service
%{_datadir}/bash-completion/completions/autogit
%{_datadir}/zsh/site-functions/_autogit
%{_datadir}/fish/vendor_completions.d/autogit.fish

%changelog
