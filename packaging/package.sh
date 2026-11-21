#!/usr/bin/env bash
# Build distributable packages for the current OS into ./dist.
#   Linux:   zenith-<ver>-linux-<arch>.tar.gz and zenith_<ver>_<arch>.deb
#   macOS:   Zenith-<ver>-macos-<arch>.dmg (contains Zenith.app)
#   Windows: zenith-<ver>-windows-<arch>.zip
# Usage: packaging/package.sh [cargo target triple]
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-}"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
if [ -n "$TARGET" ]; then
    cargo build --release -p zenith-app --target "$TARGET"
    BIN_DIR="target/$TARGET/release"
    ARCH="${TARGET%%-*}"
else
    cargo build --release -p zenith-app
    BIN_DIR="target/release"
    ARCH="$(uname -m)"
fi
rm -rf dist && mkdir -p dist
STAGE="$(mktemp -d)"

case "$(uname -s)" in
Linux)
    # tar.gz
    T="$STAGE/zenith-$VERSION"
    mkdir -p "$T"
    cp "$BIN_DIR/zenith" packaging/zenith.desktop packaging/icon.png README.md LICENSE "$T/"
    cat > "$T/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Installs Zenith for the current user (~/.local).
set -e
cd "$(dirname "$0")"
install -Dm755 zenith "$HOME/.local/bin/zenith"
install -Dm644 icon.png "$HOME/.local/share/icons/hicolor/512x512/apps/zenith.png"
install -Dm644 zenith.desktop "$HOME/.local/share/applications/zenith.desktop"
echo "Installed to ~/.local/bin/zenith (make sure ~/.local/bin is on your PATH)"
INSTALL
    chmod +x "$T/install.sh"
    tar -C "$STAGE" -czf "dist/zenith-$VERSION-linux-$ARCH.tar.gz" "zenith-$VERSION"

    # .deb
    case "$ARCH" in x86_64) DEB_ARCH=amd64 ;; aarch64) DEB_ARCH=arm64 ;; *) DEB_ARCH="$ARCH" ;; esac
    D="$STAGE/deb"
    install -Dm755 "$BIN_DIR/zenith" "$D/usr/bin/zenith"
    install -Dm644 packaging/zenith.desktop "$D/usr/share/applications/zenith.desktop"
    install -Dm644 packaging/icon.png "$D/usr/share/icons/hicolor/512x512/apps/zenith.png"
    install -Dm644 LICENSE "$D/usr/share/doc/zenith/copyright"
    mkdir -p "$D/DEBIAN"
    cat > "$D/DEBIAN/control" <<CONTROL
Package: zenith
Version: $VERSION
Section: editors
Priority: optional
Architecture: $DEB_ARCH
Depends: libc6, libgl1, libxkbcommon-x11-0, libx11-6
Recommends: git
Maintainer: Zenith developers <noreply@zenith.dev>
Homepage: https://github.com/abbosjon-dev/code-editor
Description: Fast, extensible, AI-native code editor
 Zenith is a code editor written in Rust with a built-in terminal,
 language server support, Git integration, plugins, and an AI agent
 panel that runs Claude Code, Codex, Gemini CLI and other agent CLIs.
CONTROL
    dpkg-deb --build --root-owner-group "$D" "dist/zenith_${VERSION}_${DEB_ARCH}.deb" >/dev/null
    ;;
Darwin)
    APP="$STAGE/Zenith.app"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp "$BIN_DIR/zenith" "$APP/Contents/MacOS/zenith"
    sed "s/__VERSION__/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"
    ICONSET="$STAGE/zenith.iconset"
    mkdir -p "$ICONSET"
    for s in 16 32 128 256 512; do
        sips -z $s $s packaging/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
        d=$((s * 2))
        sips -z $d $d packaging/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/zenith.icns"
    # Ad-hoc signature so the binary runs on Apple Silicon.
    codesign --force --deep --sign - "$APP" || true
    DMG_DIR="$STAGE/dmg"
    mkdir -p "$DMG_DIR"
    cp -R "$APP" "$DMG_DIR/"
    ln -s /Applications "$DMG_DIR/Applications"
    hdiutil create -volname "Zenith" -srcfolder "$DMG_DIR" -ov -format UDZO "dist/Zenith-$VERSION-macos-$ARCH.dmg" >/dev/null
    ;;
MINGW* | MSYS* | CYGWIN* | Windows_NT)
    W="$STAGE/Zenith"
    mkdir -p "$W"
    cp "$BIN_DIR/zenith.exe" README.md LICENSE "$W/"
    powershell -NoProfile -Command "Compress-Archive -Path '$(cygpath -w "$W")' -DestinationPath 'dist\\zenith-$VERSION-windows-$ARCH.zip' -Force"
    ;;
*)
    echo "unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac
rm -rf "$STAGE"
ls -la dist
