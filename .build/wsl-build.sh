#!/usr/bin/env bash
# Cross-build the Windows installer from WSL, signed for the updater.
#
# Smart App Control on the Windows host refuses to execute freshly built unsigned
# binaries, including Cargo's build scripts, so `npm run build` cannot run there.
# Inside WSL the build scripts are Linux binaries and no such policy applies;
# cargo-xwin supplies the MSVC CRT and Windows SDK import libraries and lld-link
# does the linking.
#
# CI (.github/workflows/desktop.yml) is still the supported release path. This is
# for verifying the build, and for producing an installer on a machine where the
# normal route is blocked.
#
# Note the two different kinds of signing, which are easy to confuse:
#   * the updater signature (.sig) — Ed25519, our key, works fine here;
#   * Authenticode code signing of the installer — needs a Windows host and a
#     certificate, and is skipped. Without it SmartScreen warns on first run.
set -euo pipefail

export PATH="/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
REPO=/mnt/c/Users/Navid/OneDrive/Desktop/compass-main
WORK=/root/cb

rsync -a --delete "$REPO/desktop/src-tauri/src/"          "$WORK/src-tauri/src/"
rsync -a --delete "$REPO/desktop/src-tauri/capabilities/" "$WORK/src-tauri/capabilities/"
rsync -a --delete "$REPO/desktop/src-tauri/icons/"        "$WORK/src-tauri/icons/"
cp "$REPO/desktop/src-tauri/Cargo.toml"      "$WORK/src-tauri/"
cp "$REPO/desktop/src-tauri/tauri.conf.json" "$WORK/src-tauri/"
cp "$REPO/desktop/src-tauri/build.rs"        "$WORK/src-tauri/"

mkdir -p "$WORK/local"
rsync -a --delete "$REPO/desktop/local/" "$WORK/local/"

# The updater key. Stored with Windows line endings on the host; minisign will
# not accept the stray carriage return.
KEY_SRC="$REPO/desktop/.tauri-updater.key"
if [ -f "$KEY_SRC" ]; then
  tr -d '\r' < "$KEY_SRC" > /tmp/updater.key
  TAURI_SIGNING_PRIVATE_KEY="$(cat /tmp/updater.key)"
  export TAURI_SIGNING_PRIVATE_KEY
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
  echo "updater signing: enabled"
else
  echo "updater signing: no key found, the .sig will not be produced"
fi

cd "$WORK/src-tauri"
"$WORK/cli/node_modules/.bin/tauri" build \
  --runner cargo-xwin \
  --target x86_64-pc-windows-msvc

OUT="$WORK/src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis"
echo
echo "=== artefacts ==="
ls -la "$OUT"

# Hand them back to the Windows side.
DEST="$REPO/desktop/dist"
mkdir -p "$DEST"
cp -f "$OUT"/*.exe "$DEST"/ 2>/dev/null || true
cp -f "$OUT"/*.sig "$DEST"/ 2>/dev/null || true
echo
echo "copied to desktop/dist:"
ls -la "$DEST"
