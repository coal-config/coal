#!/usr/bin/env bash
set -e

REPO="coal-config/coal"

# Currently, the workflow only builds for Linux x86_64 (Unix-wise)
# We check the OS and Architecture to prevent bad downloads
OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" != "Linux" ] || [ "$ARCH" != "x86_64" ]; then
    echo "Error: This installation script currently only supports Linux x86_64."
    echo "Your system is reporting OS: $OS, Architecture: $ARCH."
    exit 1
fi

TARGET="x86_64-unknown-linux-gnu"

echo "Fetching latest COAL version info..."
# Query the GitHub API for releases, find the first tag that starts with 'coal-v'
LATEST_TAG=$(curl -s "https://api.github.com/repos/$REPO/releases" | grep -m 1 '"tag_name": "coal-v' | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$LATEST_TAG" ]; then
    echo "Error: Could not find any COAL releases."
    exit 1
fi

# Extract the raw version number by removing the 'coal-v' prefix
VERSION=${LATEST_TAG#coal-v}
FILENAME="coal-binaries-${VERSION}-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/$REPO/releases/download/${LATEST_TAG}/${FILENAME}"

echo "Downloading COAL v${VERSION}..."
curl -sSL -o coal-tmp.tar.gz "$DOWNLOAD_URL"

echo "Extracting binaries..."
# Extract just the 'coal' and 'coal-lsp' files from the archive
tar -xzf coal-tmp.tar.gz

INSTALL_DIR="$HOME/.local/bin"
echo "Installing to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"

mv coal "$INSTALL_DIR/coal"
mv coal-lsp "$INSTALL_DIR/coal-lsp"
chmod +x "$INSTALL_DIR/coal" "$INSTALL_DIR/coal-lsp"

# Clean up
rm coal-tmp.tar.gz

echo ""
echo "✅ COAL successfully installed!"
echo "Binaries placed in: $INSTALL_DIR"
echo ""
echo "Note: If 'coal' is not recognized, you may need to add this to your ~/.bashrc or ~/.zshrc:"
echo "export PATH=\"\$HOME/.local/bin:\$PATH\""