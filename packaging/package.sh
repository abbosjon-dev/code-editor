#!/usr/bin/env bash
# Build distributable packages for the current OS into ./dist.
#   Linux:   zenith-<ver>-linux-<arch>.tar.gz, zenith_<ver>_<arch>.deb and
#            Zenith-<ver>-<arch>.AppImage (when appimagetool is available)
#   macOS:   Zenith-<ver>-macos-<arch>.dmg (contains Zenith.app)
#   Windows: zenith-<ver>-windows-<arch>.zip and Zenith-<ver>-<arch>.msi (when WiX is available)
# Usage: packaging/package.sh [cargo target triple]
#
# Optional signing (skipped when the variables are not set):
#   macOS:   MACOS_SIGN_IDENTITY="Developer ID Application: ..." signs with the hardened
#            runtime; with APPLE_ID, APPLE_TEAM_ID and APPLE_APP_PASSWORD the .dmg is
#            also notarized and stapled.
#   Windows: WINDOWS_CERT_PFX=path/to/cert.pfx and WINDOWS_CERT_PASSWORD sign the
#            .exe and the .msi with signtool.
# Tools: APPIMAGETOOL=path/to/appimagetool (else `appimagetool` on PATH), WiX v4+ `wix` on PATH.
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

    # AppImage
    TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
    if [ -n "$TOOL" ]; then
        A="$STAGE/Zenith.AppDir"
        install -Dm755 "$BIN_DIR/zenith" "$A/usr/bin/zenith"
        install -Dm644 packaging/zenith.desktop "$A/zenith.desktop"
        install -Dm644 packaging/zenith.desktop "$A/usr/share/applications/zenith.desktop"
        install -Dm644 packaging/icon.png "$A/zenith.png"
        install -Dm644 packaging/icon.png "$A/usr/share/icons/hicolor/512x512/apps/zenith.png"
        ln -s zenith.png "$A/.DirIcon"
        cat > "$A/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/zenith" "$@"
APPRUN
        chmod +x "$A/AppRun"
        # Extract-and-run: works without FUSE (containers, CI).
        ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$A" "dist/Zenith-$VERSION-$ARCH.AppImage" >/dev/null
    else
        echo "appimagetool not found: skipping the AppImage" >&2
    fi
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
    if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
        # Developer ID signature with the hardened runtime (needed for notarization).
        codesign --force --deep --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$APP"
    else
        # Ad-hoc signature so the binary runs on Apple Silicon.
        codesign --force --deep --sign - "$APP" || true
    fi
    DMG_DIR="$STAGE/dmg"
    mkdir -p "$DMG_DIR"
    cp -R "$APP" "$DMG_DIR/"
    ln -s /Applications "$DMG_DIR/Applications"
    DMG="dist/Zenith-$VERSION-macos-$ARCH.dmg"
    hdiutil create -volname "Zenith" -srcfolder "$DMG_DIR" -ov -format UDZO "$DMG" >/dev/null
    if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
        codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$DMG"
        if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
            xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
                --password "$APPLE_APP_PASSWORD" --wait
            xcrun stapler staple "$DMG"
        fi
    fi
    ;;
MINGW* | MSYS* | CYGWIN* | Windows_NT)
    W="$STAGE/Zenith"
    mkdir -p "$W"
    cp "$BIN_DIR/zenith.exe" README.md LICENSE "$W/"
    sign() {
        if [ -n "${WINDOWS_CERT_PFX:-}" ]; then
            local signtool="${SIGNTOOL:-$(command -v signtool || true)}"
            if [ -z "$signtool" ]; then
                # Newest signtool from the Windows SDK.
                signtool="$(ls "/c/Program Files (x86)/Windows Kits/10/bin/"*/x64/signtool.exe 2>/dev/null | sort -V | tail -1)"
            fi
            "$signtool" sign /f "$WINDOWS_CERT_PFX" /p "${WINDOWS_CERT_PASSWORD:-}" /fd sha256 \
                /tr http://timestamp.digicert.com /td sha256 "$1"
        fi
    }
    sign "$W/zenith.exe"
    powershell -NoProfile -Command "Compress-Archive -Path '$(cygpath -w "$W")' -DestinationPath 'dist\\zenith-$VERSION-windows-$ARCH.zip' -Force"
    # .msi installer (WiX v4+: `dotnet tool install --global wix`)
    if command -v wix >/dev/null 2>&1; then
        case "$ARCH" in x86_64) MSI_ARCH=x64 ;; aarch64) MSI_ARCH=arm64 ;; *) MSI_ARCH="$ARCH" ;; esac
        MSI="dist/Zenith-$VERSION-$ARCH.msi"
        wix build packaging/zenith.wxs -arch "$MSI_ARCH" \
            -d Version="$VERSION" -d ExePath="$(cygpath -w "$W/zenith.exe")" \
            -d IconPath="$(cygpath -w packaging/icon.ico)" -d LicensePath="$(cygpath -w LICENSE)" \
            -o "$(cygpath -w "$MSI")"
        rm -f "dist/Zenith-$VERSION-$ARCH.wixpdb" # debug symbols, not for users
        sign "$MSI"
    else
        echo "WiX not found: skipping the .msi" >&2
    fi
    ;;
*)
    echo "unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac
rm -rf "$STAGE"
ls -la dist
