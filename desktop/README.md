# Compass for Windows

The desktop shell for Compass. It ships **no frontend of its own** — it loads the
same `index.html` that GitHub Pages serves — and adds a narrow, audited set of
native capabilities the assistant can ask to use.

```
                        COMPASS
                           |
        +------------------+-------------------+
        |                                      |
  Web (browser / PWA / Telegram)      Windows (this project)
        |                                      |
        |                          loads the SAME remote URL
        |                                      |
        |                          window.__TAURI__ present
        |                                      |
        |                          CompassAgent native tools
        |                                      |
        |                          Rust: policy -> guard -> consent -> audit
        +------------------+-------------------+
                           |
                  Cloudflare Worker (unchanged)
                           |
              +------------+------------+
             AI        Memory / Sync    Notion
```

## Why there is only one frontend

`../index.html` is the single source of truth. The desktop window points at the
live GitHub Pages URL, so an HTML, CSS or JavaScript change reaches the desktop
app the moment Pages deploys it — no rebuild, no reinstall, no release.

The same file runs in a browser. The native layer registers itself only when
`window.__TAURI__` exists, so in a browser nothing registers, the agent tool spec
is an empty string, and the model is never told the tools exist. The web app
behaves exactly as it did before this project existed.

`desktop/local/` holds a generated snapshot used only when the machine is
offline. It is produced by `npm run snapshot` and is gitignored. Never edit it —
the moment a second editable copy of the frontend exists, the two drift and the
desktop app quietly becomes a different application.

## Check this before the first release

One value is an inference rather than something the existing code proved:

```rust
// src-tauri/src/shell.rs
pub const REMOTE_URL: &str = "https://navidnj2007-lgtm.github.io/compass/";
```

The worker's `ALLOWED_ORIGIN` establishes the *origin*, not the path. The
`/compass/` comes from the repository folder name, which is the usual shape for a
project site. If Pages serves Compass from a user site instead, it should be the
bare origin. Either set it at build time —

```powershell
$env:COMPASS_REMOTE_URL = "https://navidnj2007-lgtm.github.io/"
npm run build
```

— or edit the constant. The IPC capability grants access by origin with a
wildcard path, so it works either way; only a change of *origin* would require
editing `capabilities/remote.json`.

If the URL is wrong the app is not broken, it just always starts in offline mode
and shows the bundled snapshot.

## Prerequisites

- **Rust** (stable, MSVC toolchain) — `winget install Rustlang.Rustup`
- **Visual Studio Build Tools 2022** with the *Desktop development with C++*
  workload — Rust needs `link.exe`
- **Node 18+**, for the Tauri CLI and the snapshot script
- **WebView2 runtime** — present on Windows 11 and modern Windows 10; the
  installer downloads it if missing

## Build

```bash
cd desktop
npm install
npm run build          # snapshot + tauri build
```

The installer lands in:

```
desktop/src-tauri/target/release/bundle/nsis/Compass_1.0.0_x64-setup.exe
```

It installs per-user (no admin prompt), creates a Start Menu entry and a desktop
shortcut, and registers a proper uninstaller.

For development, with hot reload of the native layer:

```bash
npm run dev
```

## Verifying it

```bash
# from the repository root
node .build/verify-frontend.mjs      # every inline script parses; hooks intact
node .build/test-worker.mjs          # worker regressions + the new origins

cd desktop
npm run verify                       # the path guard, against a real filesystem
npm run check                        # clippy, warnings as errors
cargo test --manifest-path src-tauri/Cargo.toml
```

`npm run verify` is the important one. It builds `desktop/verify`, which pulls in
`guard.rs` and `rules.rs` **verbatim** with `#[path]` — not a copy — and runs 125
checks against a real filesystem: parent traversal, UNC paths, alternate data
streams, reserved device names, credential-shaped names, sibling-prefix
containment, every blocked extension, and NTFS junction escape in both the read
and the write direction.

It found two real bugs during development, which is the reason it exists rather
than a set of string assertions:

- A trailing space or dot bypassed the executable blocklist. Windows strips them
  when it opens a file, so `payload.exe ` becomes `payload.exe` on disk while
  `Path::extension()` reports `exe ` — a string that was not on the list. Both the
  path-shape rule and the extension rule now handle it, and both are pinned by
  checks.
- The guard trimmed its whole input before inspecting it, which erased that very
  trailing space before the check could see it. Only leading whitespace is
  trimmed now.

### Why the verifier is a plain binary with no dependencies

Smart App Control (see below) blocks the `cargo test` libtest harness on some
machines. `desktop/verify` is therefore dependency-free — no registry build
scripts to execute — and a plain binary rather than a test harness, which keeps
the security core checkable on exactly the sort of locked-down machine the app
should be safe on. Its string-rule section is platform-independent and runs
anywhere, including inside WSL; the filesystem section needs real Windows path
semantics and skips elsewhere rather than pretending.

`cargo test` still runs the same guard's own unit tests in CI.

### Building when Smart App Control blocks it

On machines with **Smart App Control** enforced, Windows refuses to execute
freshly built unsigned binaries — including Cargo's build scripts — with
`os error 4551`. `npm run build` cannot run at all in that state, and Smart App
Control cannot be re-enabled once turned off, so it is not something to disable
casually.

There are two ways round it that do not weaken the machine.

**CI.** `.github/workflows/desktop.yml` builds and signs on a GitHub runner. This
is the supported release path.

**Cross-build from WSL.** Build scripts then run as Linux binaries, where no such
policy applies, while `cargo-xwin` supplies the MSVC CRT and Windows SDK import
libraries and `lld-link` does the linking. This is how the installer in this
repository was produced and verified:

```bash
# one-time setup inside WSL
sudo apt-get install -y build-essential clang lld llvm nsis rsync nodejs npm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin

# then
bash .build/wsl-build.sh
```

It produces `Compass_1.0.0_x64-setup.exe` and `.exe.sig` and copies both to
`desktop/dist/`.

Two caveats, both expected:

- Tauri prints *"Cross-platform compilation is experimental"*, and the bundler
  cannot patch the `__TAURI_BUNDLE_TYPE` marker it uses to record how the app was
  installed. The marker is present in the binary — verified by searching it — the
  patcher simply cannot write to it through a cross-link. A native build (CI) does
  this correctly, so release builds should come from CI.
- Authenticode code signing of the installer needs a Windows host and a
  certificate and is skipped. The **updater** signature is unaffected and is
  produced normally.

## Deploying a frontend update

```bash
# from the repository root
git add index.html
git commit -m "..."
git push
```

GitHub Pages deploys, and **every** Compass picks it up: browser, iPhone PWA,
Telegram, and the Windows app on next launch. Nothing needs rebuilding.

Only rebuild the installer when the Rust layer changes — a new tool, a policy
change, a Tauri upgrade.

## Deploying a native update

The updater is wired to GitHub Releases and verifies an Ed25519 signature before
installing anything.

```bash
# bump the version in src-tauri/tauri.conf.json AND src-tauri/Cargo.toml first
$env:TAURI_SIGNING_PRIVATE_KEY = (Resolve-Path .\.tauri-updater.key)
npm run build
```

Publish the `.exe`, the `.exe.sig` and a `latest.json` to a GitHub release. The
app checks on startup, silently, and does nothing at all if the check fails.

> The private key `.tauri-updater.key` is gitignored. If it leaks, anyone can
> push a signed "update" to every install. The one generated during setup has no
> password; for real distribution, regenerate it with one:
> `npx tauri signer generate -w .tauri-updater.key`

## The agent

### How a tool call travels

```
model  ──►  ```compass  [{"do":"win.move_file", ...}]
              │
              ▼
        CompassAgent registry (index.html)
          validates shape, builds the approval card
              │
              ▼
        user taps Apply                       ← in-app consent, every write
              │
              ▼
        invoke("move_file", {req:{...}})       ← Tauri IPC
              │
              ▼
        guard::resolve                         ← canonicalise, confine to roots
        policy                                 ← caps, blocked extensions
        consent::require                       ← native dialog, destructive only
        the operation
        audit::record
              │
              ▼
        result ──►  back into the model's next round
```

Reads run automatically inside the existing look-then-think loop, because making
someone approve a directory listing would make the agent useless. Writes always
get an approval card. Destructive writes get a second dialog drawn by Windows.

### The tools

| Tool | Kind | Risk | Notes |
|---|---|---|---|
| `win.list_files` | read | low | one folder, not recursive |
| `win.search_files` | read | low | depth-capped, entry-capped |
| `win.read_file` | read | medium | text only, size-capped |
| `win.get_system_information` | read | low | OS, CPU, memory, disks, uptime |
| `win.clipboard_read` | read | medium | text only |
| `win.create_folder` | write | low | idempotent |
| `win.write_file` | write | medium/high | `overwrite` is treated as destructive |
| `win.move_file` | write | high | batch, collision-safe |
| `win.rename_file` | write | medium | name only, never a path |
| `win.delete_file` | write | high | Recycle Bin, never an unlink |
| `win.open_application` | write | medium | fixed allowlist, no arguments |
| `win.clipboard_write` | write | low | |
| `win.show_notification` | write | low | |

There is no `execute_any_command`, and no path to one.

### Adding a tool

Four small edits, in four obvious places:

1. **`../index.html`** — a `register({...})` record in the agent block. The
   prompt, the approval card, the read/write routing and the audit line are all
   derived from it, so there is no central list to keep in step.
2. **`src-tauri/src/tools/*.rs`** — a `#[tauri::command]` that resolves paths
   through `Guard`, calls `consent::require` for its risk tier, and records to
   the audit log.
3. **`src-tauri/build.rs`** — the command name, so the ACL knows about it.
4. **`src-tauri/capabilities/{local,remote}.json`** — `allow-your-command`.

If you skip 3 or 4 the command exists but the frontend cannot reach it, which
fails closed.

## Security

### The boundary is Rust, not the ACL

The frontend is treated as untrusted, for three concrete reasons: it is loaded
from a remote origin by design; Tauri has shipped both a remote-iframe IPC bypass
([GHSA-57fm-592m-34r7][1]) and an origin-confusion bug
([GHSA-7gmj-67g7-phm9][2]); and the assistant driving it consumes text — file
contents, web pages, notes — that may itself be hostile.

So `capabilities/` is defence in depth. The enforcement is:

- **`guard.rs`** — every path from the model goes through `resolve()`, and
  nothing else may turn a string into a path. It canonicalises **first**, so
  `..`, symlinks and NTFS junctions are resolved before the sandbox is checked;
  a junction from `Downloads\photos` to `C:\Windows` is rejected on the resolved
  path, not the pretty one. Root containment is component-wise, so `…\Navid2`
  cannot pass as a child of `…\Navid`. UNC paths, alternate data streams,
  reserved device names and `%ENV%` expansion are all refused.
- **`policy.rs`** — the allowed roots, and hard rules with no setting: no writing
  any of ~60 executable extensions, no reading anything that looks like a
  credential store, no touching the app's own config or install directory.
- **`consent.rs`** — a native dialog, built from the arguments Rust is about to
  act on. It fails closed and is never remembered.
- **`audit.rs`** — an append-only log in a directory the file tools are denied.

### The allowed folders

Downloads, Documents, Desktop, Pictures, Music, Videos — read from the OS
known-folder API, so OneDrive redirection works.

Note what is missing: the home directory itself, so dotfiles and `AppData` are
out of reach; and the install directory.

Edit them from the app: **Sync → This PC → Open folder settings**. That opens
`%APPDATA%\app.compass.desktop\agent-policy.json`. Changes apply on the next
handshake, no restart.

### Secrets

Unchanged from the web app, and worth restating: the Cloudflare Worker holds
every credential. The passphrase is typed once per device and lives in that
device's `localStorage`, deliberately outside the backup and outside the synced
state. Nothing secret is compiled into the executable, and the desktop app adds
no new credential of its own.

## Known limitations

- **The offline copy ages.** It only refreshes when a new installer is built.
  This is deliberate: caching the frontend into a writable directory would let
  the agent's own `write_file` rewrite the frontend, which is privilege
  escalation. The install directory is not user-writable and is denied to the
  guard.
- **The sync record is single-tenant.** One KV key, `compass:state`, and the
  passphrase alone grants read/write. Fine for one person with several devices;
  it is not multi-user, and anyone holding the passphrase holds the data.
- **The origin allowlist is not a security control.** `Origin` is enforced by
  browsers, not attackers. `APP_SECRET` is the real gate. This was already true
  before the desktop app.
- **`clipboard_read` is the sharpest edge.** People copy passwords and one-time
  codes. It is gated, capped and audited, but if in doubt, refuse the dialog.
- **Windows only.** The guard is written against Windows path semantics — drive
  letters, ADS, device names, junctions. macOS and Linux would need their own
  rules, not a relaxed version of these.
- **No code signing.** Without an Authenticode certificate, SmartScreen will warn
  on first run. Updates are still signature-verified by the updater's own key.

[1]: https://github.com/tauri-apps/tauri/security/advisories/GHSA-57fm-592m-34r7
[2]: https://github.com/tauri-apps/tauri/security/advisories/GHSA-7gmj-67g7-phm9
