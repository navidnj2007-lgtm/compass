#!/usr/bin/env bash
# Check a built installer before publishing it.
#
# Run inside WSL after .build/wsl-build.sh, or point REL at a native build.
#
#   1. Does the updater signature verify against the public key that is compiled
#      into the app? If not, every install would reject its own updates.
#   2. Is the __TAURI_BUNDLE_TYPE marker in the binary?
#   3. Are the artefacts real Windows PE files?
set -uo pipefail

REL=${REL:-/root/cb/src-tauri/target/x86_64-pc-windows-msvc/release}
NSIS="$REL/bundle/nsis"
REPO=${REPO:-/mnt/c/Users/Navid/OneDrive/Desktop/compass-main}

fail=0
ok()   { printf '  ok    %s\n' "$1"; }
bad()  { printf '  FAIL  %s\n' "$1"; fail=$((fail+1)); }

SIG=$(ls "$NSIS"/*.sig 2>/dev/null | head -1)
EXE=$(ls "$NSIS"/*-setup.exe 2>/dev/null | head -1)
BIN="$REL/compass-desktop.exe"

echo "=== 1. updater signature ==="
if [ -z "${SIG:-}" ] || [ -z "${EXE:-}" ]; then
  bad "no installer or signature found in $NSIS"
else
  # Tauri stores the minisign public key and signature base64-encoded.
  tr -d '\r\n' < "$REPO/desktop/.tauri-updater.key.pub" | base64 -d > /tmp/minisign.pub
  tr -d '\r\n' < "$SIG" | base64 -d > /tmp/installer.sig
  if minisign -V -m "$EXE" -x /tmp/installer.sig -p /tmp/minisign.pub >/dev/null 2>&1; then
    ok "signature verifies against the key compiled into the app"
  else
    bad "signature does NOT verify"
  fi
fi

echo
echo "=== 2. updater bundle-type marker ==="
# A raw byte search, not `strings`. The marker is a plain string literal that the
# bundler patches in place, and `strings` gave a false negative on it once — which
# is exactly the sort of wrong answer a check like this must not produce.
if [ -f "$BIN" ]; then
  if grep -q -a -- '__TAURI_BUNDLE_TYPE_VAR' "$BIN"; then
    ok "marker present in compass-desktop.exe"
  else
    bad "marker missing — stripping or dead-code elimination removed it, and the updater will not work"
  fi
else
  printf '  skip  no unpackaged binary at %s\n' "$BIN"
fi

echo
echo "=== 3. artefacts are Windows PE files ==="
for f in "$BIN" "${EXE:-}"; do
  [ -n "$f" ] && [ -f "$f" ] || continue
  desc=$(file -b "$f")
  case "$desc" in
    PE32*) ok "$(basename "$f"): $desc" ;;
    *)     bad "$(basename "$f") is not a PE file: $desc" ;;
  esac
done

echo
if [ "$fail" -eq 0 ]; then
  echo "ARTEFACT CHECKS PASSED"
else
  echo "$fail ARTEFACT CHECK(S) FAILED"
  exit 1
fi
