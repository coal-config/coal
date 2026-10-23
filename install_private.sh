#!/usr/bin/env bash
set -e

REPO="coal-config/coal"

if [ -z "$GITHUB_TOKEN" ]; then
    echo "Error: GITHUB_TOKEN environment variable is required to access this private repository."
    exit 1
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" != "Linux" ] || [ "$ARCH" != "x86_64" ]; then
    echo "Error: This installation script currently only supports Linux x86_64."
    exit 1
fi

TARGET="x86_64-unknown-linux-gnu"
AUTH_HEADER="Authorization: token $GITHUB_TOKEN"

echo "Fetching latest COAL version info..."
# Query the API using the token
RELEASES_JSON=$(curl -s -H "$AUTH_HEADER" "https://api.github.com/repos/$REPO/releases")

# Use Python to safely parse the JSON, find the latest 'coal-v' tag, and get the API Asset URL
ASSET_URL=$(echo "$RELEASES_JSON" | python3 -c "
import sys, json
try:
    releases = json.load(sys.stdin)
    for r in releases:
        if r.get('tag_name', '').startswith('coal-v'):
            tag = r.get('tag_name')
            version = tag.replace('coal-v', '')
            filename = f'coal-binaries-{version}-${TARGET}.tar.gz'
            for a in r.get('assets', []):
                if a.get('name') == filename:
                    print(a.get('url'))
                    sys.exit(0)
except Exception as e:
    pass
")

if [ -z "$ASSET_URL" ]; then
    echo "Error: Could not find a matching release asset for your architecture."
    exit 1
fi

echo "Downloading release asset..."
# For private repos, you MUST use the API url with the 'application/octet-stream' header
curl -sSL -H "$AUTH_HEADER" -H "Accept: application/octet-stream" -o coal-tmp.tar.gz "$ASSET_URL"

echo "Extracting binaries..."
tar -xzf coal-tmp.tar.gz

INSTALL_DIR="$HOME/.local/bin"
echo "Installing to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"

mv coal "$INSTALL_DIR/coal"
mv coal-lsp "$INSTALL_DIR/coal-lsp"
chmod +x "$INSTALL_DIR/coal" "$INSTALL_DIR/coal-lsp"

rm coal-tmp.tar.gz

echo ""
echo "✅ COAL successfully installed!"
echo "Binaries placed in: $INSTALL_DIR"