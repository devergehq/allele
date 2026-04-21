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
LINK="$MACOS_DIR/Allele"

# Find Info.plist relative to this script
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SOURCE_PLIST="$PROJECT_DIR/resources/Info.plist"

# Create bundle structure if missing
mkdir -p "$MACOS_DIR"

# Remove stale symlink left by earlier script versions — if $LINK is a
# symlink pointing at $BINARY, `cat >` would follow it and clobber the
# compiled binary with the launcher script.
[[ -L "$LINK" ]] && rm -f "$LINK"

# Copy Info.plist if missing or outdated, then stamp CFBundleVersion
# with a monotonic build number so Launch Services invalidates its
# icon cache on each rebuild (same trick as bundle-mac.sh).
PLIST_CHANGED=0
if [[ ! -f "$PLIST" ]] || ! cmp -s "$SOURCE_PLIST" "$PLIST"; then
    cp "$SOURCE_PLIST" "$PLIST"
    PLIST_CHANGED=1
fi
BUILD_NUMBER="$(git -C "$PROJECT_DIR" rev-list --count HEAD 2>/dev/null || date +%s)"
CURRENT_BUILD_NUMBER="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$PLIST" 2>/dev/null || echo "")"
if [[ "$CURRENT_BUILD_NUMBER" != "$BUILD_NUMBER" ]]; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_NUMBER" "$PLIST"
    PLIST_CHANGED=1
fi

# Copy app icon into bundle Resources
RESOURCES_DIR="$CONTENTS_DIR/Resources"
SOURCE_ICNS="$PROJECT_DIR/resources/Allele.icns"
DEST_ICNS="$RESOURCES_DIR/Allele.icns"
mkdir -p "$RESOURCES_DIR"
ICNS_CHANGED=0
if [[ -f "$SOURCE_ICNS" ]] && { [[ ! -f "$DEST_ICNS" ]] || ! cmp -s "$SOURCE_ICNS" "$DEST_ICNS"; }; then
    cp "$SOURCE_ICNS" "$DEST_ICNS"
    ICNS_CHANGED=1
fi

# Stderr log — macOS `open` launches a new process whose stderr is
# disconnected from the calling terminal. Write a launcher script
# inside the bundle that redirects stderr to a log file, then exec's
# the real binary. This keeps eprintln! diagnostics accessible.
ALLELE_LOG="${ALLELE_LOG:-/tmp/allele-stderr.log}"

cat > "$LINK" <<LAUNCHER
#!/usr/bin/env bash
exec "$BINARY" "\$@" 2>>"$ALLELE_LOG"
LAUNCHER
chmod +x "$LINK"

# If plist or icon changed, nudge Launch Services so the Dock picks up
# the new icon. Without this, macOS keeps serving the cached icon even
# after the bundle contents change at a stable target/ path.
if [[ "$PLIST_CHANGED" -eq 1 || "$ICNS_CHANGED" -eq 1 ]]; then
    touch "$APP_DIR"
    /System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -f "$APP_DIR" >/dev/null 2>&1 || true
fi

# Launch through the bundle. -W waits for exit so `cargo run` blocks.
# -n opens a new instance even if one is already running.
exec open -W -n "$APP_DIR" --args "$@"
