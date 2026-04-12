#!/usr/bin/env bash
#
# Cargo runner that wraps the Allele binary in a macOS .app bundle
# before launching. This gives the process a CFBundleIdentifier,
# which clipboard history apps (e.g. Paste) need to track sources.
#
# Invoked automatically by `cargo run` via .cargo/config.toml.
# Usage: run-bundled.sh <binary-path> [args...]

set -euo pipefail

BINARY="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
shift
BINARY_DIR="$(dirname "$BINARY")"

APP_DIR="$BINARY_DIR/Allele.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
PLIST="$CONTENTS_DIR/Info.plist"
LINK="$MACOS_DIR/allele"

# Find Info.plist relative to this script
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SOURCE_PLIST="$PROJECT_DIR/resources/Info.plist"

# Create bundle structure if missing
mkdir -p "$MACOS_DIR"

# Copy Info.plist if missing or outdated
if [[ ! -f "$PLIST" ]] || ! cmp -s "$SOURCE_PLIST" "$PLIST"; then
    cp "$SOURCE_PLIST" "$PLIST"
fi

# Symlink the binary (always update — it may have been recompiled)
ln -sf "$BINARY" "$LINK"

# Launch through the bundle. -W waits for exit so `cargo run` blocks.
# -n opens a new instance even if one is already running.
exec open -W -n "$APP_DIR" --args "$@"
