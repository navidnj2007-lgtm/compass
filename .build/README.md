# Checks and local build

## Verification

Three Node scripts, no dependencies. All run in CI on every push
(`.github/workflows/desktop.yml`).

```bash
node .build/verify-frontend.mjs
node .build/test-worker.mjs
node .build/test-orchestrator.mjs
```

**`verify-frontend.mjs`** — `index.html` is one 490 KB file with four inline
script blocks, so a stray brace fails silently in a browser rather than loudly in
a build. This extracts every block and parses it, then asserts the structural
facts the app depends on: that `CompassBridge` is still exported, that the storage
keys and worker actions are unchanged, that the agent registry is defined *before*
the chat layer that consumes it, that every native tool is registered, that no
secret is baked into the file, and that no generic command-execution path exists.

It also enforces the **four-place registration ritual**, which is the check worth
knowing about. Adding one native tool means five edits — the frontend
`register()` record, the `#[tauri::command]`, the `invoke_handler!` list, the
`build.rs` command list, and an `allow-` entry in *both* capability files — and
none of the ways to get that wrong are visible by reading any single file. Miss
the `build.rs` line and the command is not ACL-checked at all. Miss a capability
line and the tool is refused at runtime in one build but not the other. So the
five lists are parsed and compared as sets, in both directions: a frontend tool
with no Rust command is a broken feature, and a Rust command with no frontend tool
is IPC-reachable surface nobody is maintaining. The four commands the frontend
invokes by name instead of through the registry are named explicitly, and each has
to genuinely appear in an `invoke("…")` call or the exemption itself fails.

The namespaces a tool may register in (`win.`, `pc.`) are read out of the frontend
rather than hard-coded here, so a deliberate addition passes and an accidental one
— which at runtime would look exactly like the model declining to use a tool —
fails.

Most of its other assertions are regression guards. They encode behaviour that
already worked before the desktop shell existed and must not have changed.

Paths are resolved from the script's own location, so it runs from any working
directory and on a CI runner where the checkout is not where it was authored.

**`test-worker.mjs`** — imports `worker.js` and drives it with mock `env` and
`Request` objects, including a stubbed KV namespace. Covers the passphrase gate
(including a same-length wrong secret, since the comparison is now constant-time),
the origin allowlist, the sync revision and conflict behaviour, the chat
validation limits, the newly permitted desktop origins, and the tool-calling
passthrough. The passthrough tests stub `globalThis.fetch` and inspect the payload
that would have gone upstream, because a status code cannot tell you whether a
field was forwarded or quietly dropped.

**`test-orchestrator.mjs`** — behavioural tests for the agent loop's read path.
`index.html` has no module boundary and must not grow one, so this slices the
region under test out of the shipped file and evaluates it with stubs for
everything it reaches outward to: the Compass lookups, the native bridge, the
worker. It is weaker than running the app and much stronger than a parse check —
it pins the read cap, the ordering, the step transitions, and the distinction
between a lookup that failed to run and a tool that ran and said no. That last one
matters because the retry policy depends on it: a refusal is an answer and is
never retried.

The path guard has its own suite — `cd desktop && npm run verify`. See
`desktop/README.md`.

## Local build (WSL)

```bash
bash .build/wsl-build.sh          # cross-build the installer into desktop/dist/
wsl -d Ubuntu -u root -- bash -c 'bash /tmp/c.sh'   # after copying check-artifacts.sh
```

**`wsl-build.sh`** cross-builds the Windows installer from WSL using `cargo-xwin`
and `lld-link`. It exists because Smart App Control on this Windows host refuses to
execute freshly built unsigned binaries, including Cargo's build scripts, so the
normal `npm run build` cannot run there. CI remains the supported release path.

**`check-artifacts.sh`** checks a built installer before publishing: that the
updater signature verifies against the public key compiled into the app, that the
`__TAURI_BUNDLE_TYPE` marker survived (if it did not, the updater silently will
not work), and that both artefacts are real PE files.

Both shell scripts are stored with Windows line endings, so run them through
`tr -d '\r'` first if bash complains.
