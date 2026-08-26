/* Verify index.html: extract every inline <script> and syntax-check it, then
   assert the structural facts the desktop shell and the web app both depend on.
   Run:  node .build/verify-frontend.mjs  */
import { readFileSync, writeFileSync, mkdtempSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/* Where the files are is worked out from where this script is, never from the
   working directory and never from an absolute path typed on one machine.

   This used to be a hard-coded C:\Users\... path, which meant the script only
   ran on the author's desktop: on a CI runner the checkout lives somewhere else
   entirely, so `readFileSync` threw before a single check had run and the job
   failed for a reason that had nothing to do with the code being checked. A
   verifier that cannot run in CI is a verifier that is not verifying anything,
   so the path is now derived the same way desktop/scripts/snapshot.mjs derives
   it — from `import.meta.url`, which is true wherever the repository sits. */
const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const HTML = join(REPO, "index.html");

/* A missing file is a legible sentence and a non-zero exit, not a stack trace.
   The person reading CI output needs to know which file was expected where. */
function mustRead(file, what) {
  if (!existsSync(file)) {
    console.log(`  FAIL  ${what} is missing — expected it at ${file}`);
    console.log("\n1 FRONTEND CHECK(S) FAILED");
    process.exit(1);
  }
  return readFileSync(file, "utf8");
}

const html = mustRead(HTML, "the frontend (index.html)");

let fails = 0;
const ok = (m) => console.log("  ok    " + m);
const bad = (m) => { console.log("  FAIL  " + m); fails++; };

/* ── 1. every inline script must parse ───────────────────────────── */
console.log("\ninline scripts");
const re = /<script(?![^>]*\bsrc=)([^>]*)>([\s\S]*?)<\/script>/gi;
const dir = mkdtempSync(join(tmpdir(), "compass-verify-"));
let n = 0, m;
while ((m = re.exec(html)) !== null) {
  const attrs = m[1] || "";
  const body = m[2];
  if (/type\s*=\s*"application\/json"/i.test(attrs)) continue; // the state-data island
  n++;
  const line = html.slice(0, m.index).split("\n").length;
  const f = join(dir, `block-${n}.js`);
  writeFileSync(f, body, "utf8");
  try {
    execFileSync(process.execPath, ["--check", f], { stdio: "pipe" });
    ok(`script block ${n} (line ${line}, ${body.length} chars) parses`);
  } catch (e) {
    bad(`script block ${n} (line ${line}): ${String(e.stderr || e).slice(0, 400)}`);
  }
}
if (n === 0) bad("no inline scripts found at all");

/* ── 2. the existing app must still be wired the way it was ─────── */
console.log("\npreserved behaviour");
const must = [
  ["window.CompassBridge={", "CompassBridge still exported"],
  ['var LS_KEY = "compass_state_v2"', "localStorage state key unchanged"],
  ['var CFG_KEY = "compass_ai_cfg"', "AI config key unchanged"],
  ['"X-Compass-Secret":cfg.secret', "chat still authenticates with the passphrase header"],
  ['action:"sync.put"', "sync push unchanged"],
  ['action:"sync.get"', "sync pull unchanged"],
  ['action:"capabilities"', "capability probe unchanged"],
  ["var BUDGET = {", "the turn budget exists in one place"],
  ["var HARD_ROUNDS", "the round budget has a compiled-in ceiling"],
  ["function ledgerStop(L)", "one place decides whether a turn may continue"],
  ["Promise.allSettled(crew)", "independent lookups run concurrently and survive one failing"],
  ["var MAX_TRIES", "the retry policy is bounded and named"],
  ["var NATIVE_TOOLS = null", "tool calling starts unprobed rather than assumed"],
  ["function probeNativeTools()", "the provider is asked once whether it takes tool calls"],
  ["if(NATIVE_TOOLS){", "tools are only sent when the probe said yes, so the fenced path is untouched"],
  ["sp = splitActions(slot.content)", "the fenced protocol remains the fallback"],
  ["var visionOn = false", "vision is assumed absent until the worker says otherwise"],
  ["function splitActions(txt)", "fenced compass block parser intact"],
  ["telegram-web-app.js", "Telegram shell still loaded"],
  ["var ACTIONS_SPEC =", "original action spec intact"],
  ["var QUERY_SPEC =", "Compass lookup spec intact"],
  ["var NOTION_SPEC =", "Notion spec intact"],
];
for (const [needle, label] of must) {
  html.includes(needle) ? ok(label) : bad(label + ` (missing: ${needle})`);
}

/* no secrets baked into the shipped frontend */
console.log("\nno embedded secrets");
const secretish = [
  [/sk-[A-Za-z0-9]{20,}/, "OpenAI-style key"],
  [/ntn_[A-Za-z0-9]{20,}/, "Notion integration token"],
  [/secret\s*[:=]\s*"[^"]{12,}"/i, "hardcoded secret literal"],
  [/APP_SECRET\s*[:=]\s*"[^"]+"/, "hardcoded APP_SECRET"],
];
for (const [rx, label] of secretish) {
  rx.test(html) ? bad(`${label} found in index.html`) : ok(`no ${label}`);
}

/* ── 3. the new agent layer must be present and correctly ordered ─ */
console.log("\nagent layer");
const agentAt = html.indexOf("COMPASS AGENT \u2014 THE NATIVE TOOL LAYER");
const aiAt = html.indexOf("COMPASS AI \u2550");
agentAt > 0 ? ok("agent block present") : bad("agent block missing");
agentAt > 0 && aiAt > 0 && agentAt < aiAt
  ? ok("agent registry is defined before the chat layer that consumes it")
  : bad("agent block is not ahead of the chat layer");

const hooks = [
  ["function agent(){", "agent() accessor added to the chat closure"],
  ["|| isNativeRead(a)", "native reads join the tool loop"],
  ["if(isNativeWrite(a)) return agent().runWrite(a)", "native writes dispatch from runAct"],
  /* Checked by route rather than by exact expression: the call has to still go
     through the agent's runRead, but how its result is wrapped is the orchestrator's
     business and changed once already when reads gained a ran/refused distinction. */
  ["agent().runRead(a)", "native reads dispatch through the agent"],
  ["async function runOneRead(", "a single lookup is its own unit, so it can be stepped and retried"],
  ["+ (A ? A.spec() : \"\")", "agent spec appended to the system prompt"],
  ["|| !!agent();", "native writes may be proposed"],
  ["agent().readingLabel(fresh)", "activity line names the running tool"],
  ["window.CompassAgent.paintPanel()", "desktop panel re-attaches after a Sync repaint"],
];
for (const [needle, label] of hooks) {
  html.includes(needle) ? ok(label) : bad(label + ` (missing: ${needle})`);
}

/* every registered tool needs a spec line and a describe, or the model would be
   told about a tool that cannot render an approval card */
console.log("\ntool registry completeness");
const names = [
  ...html.matchAll(/name\s*:\s*"([a-z]+\.[a-z0-9_]+)"\s*,\s*cmd\s*:\s*"([a-z0-9_]+)"/g),
];
const expected = [
  "list_files", "search_files", "grep_files", "read_file", "read_document", "diff_file", "restore_file", "write_file", "create_folder",
  "move_file", "rename_file", "delete_file", "open_application",
  "clipboard_read", "clipboard_write", "get_system_information", "show_notification",
  // browser control
  "browser_tabs", "browser_open", "browser_read", "browser_click", "browser_type",
];
ok(`${names.length} tools registered`);
for (const e of expected) {
  names.some(([, , cmd]) => cmd === e) ? ok(`tool ${e} registered`) : bad(`tool ${e} MISSING`);
}

/* ── 4. the namespace allow-list ─────────────────────────────────── */
console.log("\nregistry namespaces");
/* Read the allow-list out of the frontend rather than hard-coding it here, so
   this check follows a deliberate addition and still catches an accidental one. */
const nsLine = /var NAMESPACES\s*=\s*\[([^\]]*)\]/.exec(html);
let namespaces = [];
if (!nsLine) {
  bad("the NAMESPACES allow-list is missing from the agent registry");
} else {
  namespaces = [...nsLine[1].matchAll(/"([a-z]+\.)"/g)].map((m) => m[1]);
  namespaces.length
    ? ok(`namespaces allow-listed: ${namespaces.join(", ")}`)
    : bad("the NAMESPACES allow-list is empty, so no tool can register");
}
/* A tool registered outside the list is silently unavailable at runtime. That is
   how a `sys.` or `os.` family would ship dead: the code is there, the model is
   never told, and it looks like the model declining to use it. */
for (const [, name] of names) {
  const prefix = name.slice(0, name.indexOf(".") + 1);
  namespaces.includes(prefix)
    ? ok(`${name} is in an allow-listed namespace`)
    : bad(`${name} registers outside NAMESPACES (${namespaces.join(", ")}) and would be dropped`);
}

/* ── 5. the four-place registration ritual ───────────────────────── */
/* Adding a native tool means five edits: the frontend record, the #[tauri::command],
   the invoke_handler! list, the build.rs command list, and an allow- entry in BOTH
   capability files. Miss the build.rs line and the command is not ACL-checked at
   all; miss a capability line and it is refused at runtime in one build but not
   the other. None of those failures is visible by reading any single file, which
   is exactly why this is checked by a machine rather than by remembering.

   Compared in both directions on purpose. A frontend tool with no Rust command is
   a broken feature; a Rust command with no frontend tool is attack surface reachable
   over IPC that nobody is maintaining. */
console.log("\nfour-place registration ritual");

const TAURI = join(REPO, "desktop", "src-tauri");
const buildRs = mustRead(join(TAURI, "build.rs"), "desktop/src-tauri/build.rs");
const libRs = mustRead(join(TAURI, "src", "lib.rs"), "desktop/src-tauri/src/lib.rs");
const capLocal = mustRead(join(TAURI, "capabilities", "local.json"), "capabilities/local.json");
const capRemote = mustRead(join(TAURI, "capabilities", "remote.json"), "capabilities/remote.json");

/* Commands the frontend invokes by name rather than through the registry: the
   startup handshake, the audit panel, the button that opens the policy file, and
   `open_url`, which is not a tool of its own but the desktop implementation of
   the `open.app` / `open.url` actions that existed long before this layer did.
   They are ACL-checked like everything else; they simply have no register()
   record, so they are named here instead of weakening the comparison.

   Naming one is not free. Each has to be genuinely invoked somewhere in
   index.html, which is asserted below — otherwise this list would become the
   place a command goes to be exempted from the check that it is used at all. */
const NON_TOOL_COMMANDS = [
  "agent_handshake",
  "agent_audit",
  "agent_open_settings",
  "open_url",
];
for (const c of NON_TOOL_COMMANDS) {
  html.includes(`invoke("${c}"`)
    ? ok(`${c} is exempt from the registry and really is invoked directly`)
    : bad(`${c} is exempt from the registry but nothing in index.html invokes it — drop the exemption`);
}

const sortedSet = (xs) => [...new Set(xs)].sort();
const kebab = (cmd) => "allow-" + cmd.replace(/_/g, "-");

const registryCmds = sortedSet(names.map(([, , cmd]) => cmd));

/* build.rs: the string literals inside commands(&[ ... ]) */
const cmdBlock = /commands\(&\[([\s\S]*?)\]\)/.exec(buildRs);
const buildCmds = cmdBlock
  ? sortedSet([...cmdBlock[1].matchAll(/"([a-z0-9_]+)"/g)].map((m) => m[1]))
  : null;
if (!buildCmds) bad("could not find commands(&[...]) in build.rs");

/* lib.rs: the last path segment of each invoke_handler! entry */
const invokeBlock = /invoke_handler\(tauri::generate_handler!\[([\s\S]*?)\]\)/.exec(libRs);
const invokeCmds = invokeBlock
  ? sortedSet(
      invokeBlock[1]
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean)
        .map((s) => s.split("::").pop())
    )
  : null;
if (!invokeCmds) bad("could not find invoke_handler(generate_handler![...]) in lib.rs");

/* the capability files: only the app's own allow- entries, not core:/updater: */
function capAllows(text, label) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    bad(`${label} is not valid JSON: ${String(e).slice(0, 120)}`);
    return null;
  }
  const perms = Array.isArray(parsed.permissions) ? parsed.permissions : [];
  return sortedSet(perms.filter((p) => typeof p === "string" && /^allow-[a-z0-9-]+$/.test(p)));
}
const localAllows = capAllows(capLocal, "capabilities/local.json");
const remoteAllows = capAllows(capRemote, "capabilities/remote.json");

/* Compare a set of command names against the expected set, in both directions. */
function sameCommands(label, actual, wanted, fixHint) {
  if (!actual) return;
  const missing = wanted.filter((c) => !actual.includes(c));
  const extra = actual.filter((c) => !wanted.includes(c));
  if (!missing.length && !extra.length) {
    ok(`${label} matches the registry (${actual.length} entries)`);
    return;
  }
  for (const c of missing) bad(`${label} is missing "${c}" — ${fixHint(c)}`);
  for (const c of extra) {
    bad(`${label} exposes "${c}", which no frontend tool registers — remove it or register the tool`);
  }
}

const wantedCmds = sortedSet(registryCmds.concat(NON_TOOL_COMMANDS));
const wantedAllows = sortedSet(wantedCmds.map(kebab));

sameCommands(
  "build.rs commands(&[...])",
  buildCmds,
  wantedCmds,
  (c) => `add "${c}" so the command is ACL-checked instead of implicitly available`
);
sameCommands(
  "lib.rs invoke_handler!",
  invokeCmds,
  wantedCmds,
  (c) => `add the tools::<family>::${c} entry or the command cannot be invoked`
);
sameCommands(
  "capabilities/local.json",
  localAllows,
  wantedAllows,
  (c) => `add "${c}" or the offline build refuses that tool`
);
sameCommands(
  "capabilities/remote.json",
  remoteAllows,
  wantedAllows,
  (c) => `add "${c}" or the live frontend refuses that tool`
);

/* The two capability files must stay identical in what they allow. local.json
   says so in its own description: if they drift, a bug appears in the offline
   build and not the online one, or the reverse, and it is found by a user. */
if (localAllows && remoteAllows) {
  const onlyLocal = localAllows.filter((p) => !remoteAllows.includes(p));
  const onlyRemote = remoteAllows.filter((p) => !localAllows.includes(p));
  onlyLocal.length || onlyRemote.length
    ? bad(
        "the capability files have drifted — " +
          `only in local: [${onlyLocal.join(", ")}], only in remote: [${onlyRemote.join(", ")}]`
      )
    : ok("both capability files allow exactly the same tools");
}

/* no generic command execution anywhere in the frontend */
const danger = /execute_any_command|run_command|exec_shell|"eval"|\bnew Function\(/;
danger.test(html) ? bad("a generic command-execution path exists in the frontend") : ok("no generic command-execution tool");

console.log(`\n${fails === 0 ? "ALL FRONTEND CHECKS PASSED" : fails + " FRONTEND CHECK(S) FAILED"}`);
process.exit(fails === 0 ? 0 : 1);
