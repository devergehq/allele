#!/usr/bin/env bash
#
# Build Allele and wrap it in a macOS .app bundle.
#
# Usage:
#   ./script/bundle-mac.sh           # debug build
#   ./script/bundle-mac.sh --release # release build
#
# Output: target/{debug|release}/Allele.app/

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PROFILE="debug"
CARGO_FLAG=""

if [[ "${1:-}" == "--release" ]]; then
    PROFILE="release"
    CARGO_FLAG="--release"
fi

echo "==> Building Allele ($PROFILE)..."
cargo build $CARGO_FLAG

BINARY="$PROJECT_DIR/target/$PROFILE/Allele"
if [[ ! -f "$BINARY" ]]; then
    echo "Error: binary not found at $BINARY" >&2
    exit 1
fi

APP_DIR="$PROJECT_DIR/target/$PROFILE/Allele.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"

echo "==> Assembling Allele.app..."
rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR"

# Copy Info.plist
cp "$PROJECT_DIR/resources/Info.plist" "$CONTENTS_DIR/Info.plist"

# Copy binary
cp "$BINARY" "$MACOS_DIR/Allele"

echo "==> Done: $APP_DIR"
echo ""
echo "Launch with:"
echo "  open $APP_DIR"
echo "  # or directly: $MACOS_DIR/allele"
