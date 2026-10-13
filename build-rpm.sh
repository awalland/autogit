#!/bin/bash
set -euo pipefail

build_mode=-ba
case "${1:-}" in
    "") ;;
    --source-only) build_mode=-bs ;;
    *) echo "Usage: $0 [--source-only]" >&2; exit 2 ;;
esac
if (( $# > 1 )); then
    echo "Usage: $0 [--source-only]" >&2
    exit 2
fi

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for tool in cargo rpmbuild python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Error: $tool is required to build the RPM." >&2
        exit 1
    fi
done

version=$(python3 - "$project_dir/Cargo.toml" <<'PYTHON'
import sys
import tomllib

with open(sys.argv[1], "rb") as manifest:
    print(tomllib.load(manifest)["workspace"]["package"]["version"])
PYTHON
)
rpmbuild_dir="${AUTOGIT_RPMBUILD_DIR:-$HOME/rpmbuild}"
mkdir -p "$rpmbuild_dir"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
rpmbuild_dir="$(cd "$rpmbuild_dir" && pwd)"

temporary_dir=$(mktemp -d)
trap 'rm -rf "$temporary_dir"' EXIT
package_dir="$temporary_dir/autogit-$version"
mkdir -p "$package_dir/.cargo"
cp -a "$project_dir/autogit" "$project_dir/autogit-daemon" "$project_dir/autogit-shared" "$package_dir/"
cp "$project_dir/Cargo.toml" "$project_dir/README.md" "$project_dir/LICENSE" \
    "$project_dir/autogit-daemon.service" "$package_dir/"
if [[ -f "$project_dir/Cargo.lock" ]]; then
    cp "$project_dir/Cargo.lock" "$package_dir/"
else
    cargo generate-lockfile --manifest-path "$package_dir/Cargo.toml"
fi

# Include dependencies so the source RPM can build without network access.
(
    cd "$package_dir"
    cargo vendor --locked vendor > .cargo/config.toml
)
tar -czf "$rpmbuild_dir/SOURCES/autogit-$version.tar.gz" \
    -C "$temporary_dir" "autogit-$version"
sed "s|^Version:.*|Version:        $version|" "$project_dir/autogit.spec" \
    > "$rpmbuild_dir/SPECS/autogit.spec"

rpmbuild "$build_mode" --define "_topdir $rpmbuild_dir" "$rpmbuild_dir/SPECS/autogit.spec"
echo "RPM artifacts: $rpmbuild_dir/RPMS and $rpmbuild_dir/SRPMS"
