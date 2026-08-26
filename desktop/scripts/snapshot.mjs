/* Generate the offline copy of the frontend that ships inside the installer.
 *
 * There is exactly one frontend in this project: ../../index.html. This script
 * copies it into desktop/local/ at build time so the installer has something to
 * show when the machine is offline. It is generated, never edited, and it is
 * gitignored — the moment a second editable copy of the frontend exists, the two
 * start drifting and the desktop app quietly becomes a different application.
 *
 * Run automatically by `npm run build` and `npm run dev`.
 */
import { readFileSync, writeFileSync, mkdirSync, copyFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const out = resolve(here, "..", "local");

const source = join(repo, "index.html");
if (!existsSync(source)) {
  console.error(`FAIL: no frontend at ${source}`);
  process.exit(1);
}

mkdirSync(out, { recursive: true });

let html = readFileSync(source, "utf8");

/* The bundled copy is loaded from the app's own origin, where a request to
   telegram.org would fail and log a console error on every start. The Telegram
   shell has no job in a desktop window anyway — it detects its own absence and
   stands down — so the script tag is dropped from the offline copy only. The
   live copy served to browsers and to the online desktop is untouched. */
const before = html.length;
html = html.replace(
  /\s*<script src="https:\/\/telegram\.org\/js\/telegram-web-app\.js"><\/script>/,
  ""
);
const dropped = before - html.length;

/* A marker so a support question can be answered by looking at the page: is this
   the live frontend or the snapshot, and from when? */
html = html.replace(
  /<\/head>/i,
  `<meta name="compass-offline-snapshot" content="${new Date().toISOString()}">\n</head>`
);

writeFileSync(join(out, "index.html"), html, "utf8");

/* The manifest and icons, so the offline copy renders with its own identity. */
for (const f of ["manifest.json", "icon-192.png", "icon-512.png", "icon-180.png"]) {
  const from = join(repo, f);
  if (existsSync(from)) copyFileSync(from, join(out, f));
}

console.log(
  `snapshot written to ${out}\n` +
    `  index.html  ${html.length} chars` +
    (dropped ? ` (telegram script removed, ${dropped} chars)` : "")
);
