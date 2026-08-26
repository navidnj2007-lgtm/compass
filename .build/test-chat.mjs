/* Tests for the chat layer: storage, search, the slash palette, export and the
 * transparency footer.
 *
 * Same slicing approach as the other suites. What is worth pinning here is not the
 * appearance but the behaviour that is easy to break and awkward to notice: that a search
 * result points at the right message in the right conversation, that deleting a question
 * takes its answer with it, that export produces something that parses, and that the
 * palette stops offering itself the moment the text stops looking like a command.
 *
 * Run:  node .build/test-chat.mjs
 */
import { readFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const HTML = join(REPO, "index.html");
if (!existsSync(HTML)) {
  console.log(`  FAIL  no frontend at ${HTML}`);
  process.exit(1);
}
const html = readFileSync(HTML, "utf8");

let pass = 0, fail = 0;
function check(label, cond, extra) {
  if (cond) { console.log("  ok    " + label); pass++; }
  else { console.log("  FAIL  " + label + (extra ? "  →  " + extra : "")); fail++; }
}

function region(startMark, endMark) {
  const a = html.indexOf(startMark);
  const b = html.indexOf(endMark, a + 1);
  if (a < 0 || b < 0 || b < a) {
    console.log(`  FAIL  could not locate ${JSON.stringify(startMark)} .. ${JSON.stringify(endMark)}`);
    process.exit(1);
  }
  return html.slice(a, b);
}

/* ── storage ─────────────────────────────────────────────────────── */
console.log("\nchat storage keeps what it should and drops what it must");
{
  const src = region("function slimMsg(m){", "function saveChats(){");
  const make = (idb) => new Function("idb", "return " + src.trim() + ";")(idb);

  const withImages = make({});   // a truthy idb, meaning IndexedDB is in use
  const withoutIdb = make(null); // the localStorage fallback

  const msg = {
    role: "user", at: 5, content: "look at this",
    chips: [{ name: "worksheet.jpg", thumb: "data:image/jpeg;base64,AAA" }],
  };
  check("with IndexedDB, an attachment thumbnail is kept",
    withImages(msg).chips[0].thumb === "data:image/jpeg;base64,AAA",
    JSON.stringify(withImages(msg).chips[0]));
  check("on the localStorage fallback it is still stripped, as before",
    withoutIdb(msg).chips[0].thumb === null,
    JSON.stringify(withoutIdb(msg).chips[0]));

  const withSteps = { role: "assistant", at: 6, content: "done", steps: [{ id: "a" }, { id: "b" }] };
  check("step records are never persisted", withImages(withSteps).steps === undefined);
  check("...but the count is", withImages(withSteps).sdrop === 2, String(withImages(withSteps).sdrop));

  const withUsage = { role: "assistant", at: 7, content: "x", usage: { rounds: 3, tokens: 900 } };
  check("what the turn cost is persisted, so a reloaded footer still says it",
    withUsage && withImages(withUsage).usage.rounds === 3, JSON.stringify(withImages(withUsage).usage));
}
{
  /* The migration must not be destructive, and must not run twice. Asserted by reading the
     code rather than by driving a fake IndexedDB: what matters is the absence of a
     removeItem, which a behavioural test would not notice. */
  const src = region("function loadChats(){", "function dropChatRecord(");
  check("the migration never deletes the localStorage copy",
    !/removeItem/.test(src), "found a removeItem in the migration path");
  check("...and only runs when the new store is empty",
    /!found\.length && legacy\.chats\.length/.test(src), "migration is not guarded");
  const save = region("function saveChats(){", "function loadLegacy(){");
  check("a failed IndexedDB write falls through to localStorage",
    /catch[\s\S]*localStorage\.setItem/.test(save), "no fallback on write");
}

/* ── search ──────────────────────────────────────────────────────── */
console.log("\nsearch finds the right message in the right conversation");
{
  const src = region("var searchQ = \"\", searchHits = [];", "function searchHTML(){");
  const env = {
    chats: [
      {
        id: "c1", title: "Chemistry", at: 10,
        messages: [
          { role: "user", at: 1, content: "explain titration curves" },
          { role: "assistant", at: 2, body: "A titration curve plots pH against volume." },
        ],
      },
      {
        id: "c2", title: "Physics", at: 20,
        messages: [{ role: "user", at: 3, content: "what is impulse" }],
      },
    ],
    rawOf: (m) => (m.role === "user" ? m.content : m.body || m.content || ""),
    titleFor: (c) => c.title,
  };
  const names = Object.keys(env);
  const made = new Function(...names, src + "\nreturn { runSearch, searchHits };")(
    ...names.map((k) => env[k])
  );

  let hits = made.runSearch("titration");
  check("both messages mentioning the word are found", hits.length === 2, String(hits.length));
  check("a hit names its conversation", hits[0].chat === "c1", hits[0].chat);
  check("a hit names the message index, which is what jump-to uses",
    hits[0].index === 0 && hits[1].index === 1, JSON.stringify(hits.map((h) => h.index)));
  check("the excerpt surrounds the match rather than starting at the message",
    hits[1].before.length > 0 && hits[1].after.length > 0, JSON.stringify(hits[1]));
  check("the matched text is captured separately so it can be marked",
    hits[0].hit.toLowerCase() === "titration", hits[0].hit);

  check("matching is case-insensitive", made.runSearch("TITRATION").length === 2);
  check("one character searches nothing, so the list is not the whole history",
    made.runSearch("t").length === 0);
  check("a word in another conversation is found there",
    made.runSearch("impulse")[0].chat === "c2");
  check("nothing matching gives no hits", made.runSearch("zzzz").length === 0);
  check("search is not fuzzy: iteration does not match titration",
    made.runSearch("iteration").length === 0);
}

/* ── the slash palette ───────────────────────────────────────────── */
console.log("\nthe palette offers modes, not actions");
{
  const src = region("function paletteItems(){", "function paintPalette(){");
  const env = {
    MODES: {
      advise: { label: "Advise", hint: "h1" },
      explain: { label: "Explain", hint: "h2" },
    },
    cur: { mode: "advise" },
    saveChats: () => {},
    render: () => {},
    stop: () => {},
    newChat: () => {},
    exportChat: () => {},
    $: () => null,
    setTimeout: () => 0,
    mode: "advise",
    drawerOn: false,
  };
  const names = Object.keys(env);
  let value = "";
  env.$ = (sel) => (sel === "#askInput" ? { value } : null);
  const made = new Function(...names, src + "\nreturn { paletteItems, paletteQuery, paletteMatches };")(
    ...names.map((k) => env[k])
  );

  const items = made.paletteItems();
  check("every mode is offered", items.some((i) => i.label === "/advise") && items.some((i) => i.label === "/explain"));
  check("new, search and export are offered",
    ["/new", "/search", "/export"].every((l) => items.some((i) => i.label === l)),
    items.map((i) => i.label).join(","));
  /* The rule that matters: the palette must not offer to act on his machine. */
  const forbidden = items.filter((i) => /click|type|delete|move|write|hotkey|drag/.test(i.label));
  check("nothing in the palette acts on his files or his screen",
    forbidden.length === 0, forbidden.map((i) => i.label).join(","));

  value = "";
  check("no slash, no palette", made.paletteQuery() === null);
  value = "/";
  check("a bare slash offers everything", made.paletteMatches().length === items.length);
  value = "/exp";
  check("typing filters by prefix, and an ambiguous prefix keeps both",
    made.paletteMatches().length === 2,
    JSON.stringify(made.paletteMatches().map((i) => i.label)));
  value = "/expo";
  check("...narrowing to one leaves one",
    made.paletteMatches().length === 1 && made.paletteMatches()[0].label === "/export",
    JSON.stringify(made.paletteMatches().map((i) => i.label)));
  value = "/explain the thing";
  check("a sentence beginning with a slash is not a command",
    made.paletteQuery() === null, JSON.stringify(made.paletteQuery()));
  value = "/zzz";
  check("an unknown command matches nothing rather than everything",
    made.paletteMatches().length === 0);
  value = "hello /explain";
  check("a slash mid-sentence is not a command", made.paletteQuery() === null);
}

/* ── deleting a turn ─────────────────────────────────────────────── */
console.log("\ndeleting a turn leaves a conversation that still makes sense");
{
  const src = region("function deleteTurn(i){", "function exportChat(kind){");
  function run(messages, i, busy = false) {
    const env = {
      history: messages,
      busy,
      saveChats: () => {},
      paint: () => {},
      toast: () => {},
    };
    const names = Object.keys(env);
    new Function(...names, src + "\nreturn deleteTurn;")(...names.map((k) => env[k]))(i);
    return env.history;
  }

  const pair = [
    { role: "user", content: "q1" },
    { role: "assistant", content: "a1" },
    { role: "user", content: "q2" },
    { role: "assistant", content: "a2" },
  ];
  let left = run(pair.slice(), 0);
  check("deleting a question takes its answer with it", left.length === 2, JSON.stringify(left.map((m) => m.content)));
  check("...and the right pair survives", left[0].content === "q2", left[0].content);

  left = run(pair.slice(), 1);
  check("deleting an answer leaves its question", left.length === 3 && left[0].content === "q1",
    JSON.stringify(left.map((m) => m.content)));

  const trailing = [{ role: "user", content: "q1" }];
  check("deleting an unanswered question removes just it", run(trailing, 0).length === 0);

  check("deleting while busy does nothing", run(pair.slice(), 0, true).length === 4);
  check("an index off the end does nothing", run(pair.slice(), 99).length === 4);
  check("a negative index does nothing", run(pair.slice(), -1).length === 4);
}

/* ── the transparency footer ─────────────────────────────────────── */
console.log("\nthe footer says something only when there is something to say");
{
  const src = region("function usageFooter(m){", "/* ── TOOL RESULT CARDS");
  const env = {
    esc: (s) => String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])),
    stepMs: (ms) => (ms < 1000 ? ms + " ms" : (ms / 1000).toFixed(1) + " s"),
    fmtN: (n) => String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ","),
  };
  const names = Object.keys(env);
  const usageFooter = new Function(...names, src + "\nreturn usageFooter;")(...names.map((k) => env[k]));

  check("a turn with no usage shows nothing", usageFooter({}) === "");
  check("a one-round answer with no tools shows nothing, rather than furniture",
    usageFooter({ usage: { rounds: 1, steps: 0, ms: 400, tokens: 0 } }) === "",
    usageFooter({ usage: { rounds: 1, steps: 0, ms: 400, tokens: 0 } }));

  const busy = usageFooter({ usage: { rounds: 4, steps: 9, ms: 41000, tokens: 23400, model: "qwen3.8-max" } });
  check("a worked turn reports its rounds", busy.includes("4 rounds"), busy);
  check("...its tool steps", busy.includes("9 tool steps"), busy);
  check("...its wall clock", /41\.0 s/.test(busy), busy);
  check("...its tokens, with thousands separated", busy.includes("23,400 tokens"), busy);
  check("...and which model answered", busy.includes("qwen3.8-max"), busy);

  const nomodel = usageFooter({ usage: { rounds: 3, steps: 1, ms: 100, tokens: 10 } });
  check("with no model chosen it says so rather than leaving a gap",
    nomodel.includes("the worker default"), nomodel);
  const vision = usageFooter({ usage: { rounds: 2, steps: 1, ms: 100, tokens: 10, model: "m", vision: true } });
  check("a vision turn says so", vision.includes("+ vision"), vision);
}

console.log(`\n${fail === 0 ? `ALL ${pass} CHAT TESTS PASSED` : `${fail} of ${pass + fail} FAILED`}`);
process.exit(fail === 0 ? 0 : 1);
