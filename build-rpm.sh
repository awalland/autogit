#!/bin/bash
set -e

# Build script for creating autogit RPM package

PACKAGE_NAME="autogit"
RELEASE="1"

# Get the absolute path to the project directory
PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$PROJECT_DIR"

# Extract version from Cargo.toml
VERSION=$(grep -m1 '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')

if [ -z "$VERSION" ]; then
    echo >&2 "Error: Could not extract version from Cargo.toml"
    exit 1
fi

echo "==================================="
echo "Building ${PACKAGE_NAME} RPM v${VERSION}-${RELEASE}"
echo "==================================="

# Check for required tools
command -v rpmbuild >/dev/null 2>&1 || {
    echo >&2 "Error: rpmbuild is required but not installed."
    echo >&2 "Install it with: sudo dnf install rpm-build rpmdevtools"
    exit 1
}

command -v cargo >/dev/null 2>&1 || {
    echo >&2 "Error: cargo is required but not installed."
    echo >&2 "Install Rust from: https://rustup.rs/"
    exit 1
}

echo "Project directory: $PROJECT_DIR"

# Set up RPM build directories
RPMBUILD_DIR="$HOME/rpmbuild"
echo "Setting up RPM build directories in: $RPMBUILD_DIR"
mkdir -p "$RPMBUILD_DIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

# Create source tarball
TARBALL_NAME="${PACKAGE_NAME}-${VERSION}.tar.gz"
TARBALL_PATH="$RPMBUILD_DIR/SOURCES/$TARBALL_NAME"

echo "Creating source tarball: $TARBALL_NAME"

# Create a temporary directory for the tarball
TEMP_DIR=$(mktemp -d)
PACKAGE_DIR="$TEMP_DIR/${PACKAGE_NAME}-${VERSION}"

# Copy project files to temp directory
mkdir -p "$PACKAGE_DIR"
cp -r autogit autogit-daemon autogit-shared "$PACKAGE_DIR/"
cp Cargo.toml Cargo.lock "$PACKAGE_DIR/" 2>/dev/null || cp Cargo.toml "$PACKAGE_DIR/"
cp README.md LICENSE autogit-daemon.service "$PACKAGE_DIR/"

# Create the tarball
cd "$TEMP_DIR"
tar czf "$TARBALL_PATH" "${PACKAGE_NAME}-${VERSION}"
cd "$PROJECT_DIR"

# Clean up temp directory
rm -rf "$TEMP_DIR"

echo "Source tarball created: $TARBALL_PATH"

# Copy spec file to SPECS directory and update version
echo "Copying spec file and updating version to $VERSION"
sed "s|^Version:.*|Version:        $VERSION|" autogit.spec > "$RPMBUILD_DIR/SPECS/autogit.spec"

# Build the RPM
echo ""
echo "Building RPM..."
echo "This may take a few minutes as Rust dependencies are downloaded and compiled..."
echo ""

rpmbuild -ba --nodeps "$RPMBUILD_DIR/SPECS/autogit.spec"

# Find the built RPM
RPM_ARCH=$(uname -m)
RPM_FILE=$(find "$RPMBUILD_DIR/RPMS/$RPM_ARCH" -name "${PACKAGE_NAME}-${VERSION}-${RELEASE}*.rpm" | head -n 1)
SRPM_FILE=$(find "$RPMBUILD_DIR/SRPMS" -name "${PACKAGE_NAME}-${VERSION}-${RELEASE}*.src.rpm" | head -n 1)

echo ""
echo "==================================="
echo "Build completed successfully!"
echo "==================================="
echo ""
echo "RPM package:  $RPM_FILE"
echo "Source RPM:   $SRPM_FILE"
echo ""
echo "To install the RPM:"
echo "  sudo rpm -ivh $RPM_FILE"
echo ""
echo "Or with DNF:"
echo "  sudo dnf install $RPM_FILE"
echo ""
echo "To uninstall:"
echo "  sudo rpm -e $PACKAGE_NAME"
echo ""
