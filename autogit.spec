Name:           autogit
Version:        0.9.1
Release:        1
Summary:        Automatic Git commit and synchronisation daemon
License:        WTFPL
URL:            https://github.com/awalland/autogit
Source0:        %{name}-%{version}.tar.gz
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  pkgconf-pkg-config
BuildRequires:  libopenssl-devel
BuildRequires:  zlib-devel
BuildRequires:  systemd-rpm-macros
BuildRequires:  dbus-1-daemon
BuildRequires:  python3
Requires:       git

%description
Autogit periodically commits changes in configured Git repositories and
synchronises them with their remotes. It includes a configuration CLI,
an optional system tray interface, and a systemd user service.

%prep
%setup -q

%build
mkdir -p target
cargo build --release --frozen --workspace --message-format=json-render-diagnostics > target/build-messages.jsonl
python3 - <<'PYTHON'
import json
from pathlib import Path
from shutil import copyfile

names = ("autogit.bash", "_autogit", "autogit.fish")
outputs = set()
with open("target/build-messages.jsonl") as messages:
    for line in messages:
        message = json.loads(line)
        if message["reason"] == "build-script-executed":
            output = Path(message["out_dir"])
            if all((output / name).is_file() for name in names):
                outputs.add(output)
if len(outputs) != 1:
    raise SystemExit("Expected one autogit shell completion output directory")
destination = Path("target/completions")
destination.mkdir(exist_ok=True)
output = outputs.pop()
for name in names:
    copyfile(output / name, destination / name)
PYTHON

%check
cargo test --frozen --workspace

%install
install -Dm755 target/release/autogit %{buildroot}%{_bindir}/autogit
install -Dm755 target/release/autogit-daemon %{buildroot}%{_bindir}/autogit-daemon
install -Dm644 autogit-daemon.service %{buildroot}%{_userunitdir}/autogit-daemon.service
install -Dm644 target/completions/autogit.bash %{buildroot}%{_datadir}/bash-completion/completions/autogit
install -Dm644 target/completions/_autogit %{buildroot}%{_datadir}/zsh/site-functions/_autogit
install -Dm644 target/completions/autogit.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/autogit.fish

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
