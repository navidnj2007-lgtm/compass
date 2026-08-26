/* Tests for the step timeline's rendering.
 *
 * Same slicing technique as test-orchestrator.mjs and the same reason: index.html
 * has no module boundary and must not grow one. This evaluates the real renderer
 * against fabricated step records and asserts on the HTML it produces, which is
 * enough to pin the things that matter and cannot be checked by reading — that a
 * reloaded turn admits its detail was dropped, that a step with no result is not
 * focusable, that nothing in here can reach Apply, and that untrusted text from a
 * tool result is escaped before it becomes markup.
 *
 * Run:  node .build/test-timeline.mjs
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

const from = html.indexOf("var STEP_MARK = {");
const to = html.indexOf("/* ── ATTACHMENTS");
if (from < 0 || to < 0 || to < from) {
  console.log("  FAIL  could not locate the timeline renderer in index.html");
  process.exit(1);
}
const src = html.slice(from, to);

/* The renderer reaches out for `esc`, the message list and a jQuery-ish $. Only esc
   matters to the output, so it is the real one lifted out of the same file rather
   than a re-implementation — an escaping test against a second implementation of
   escaping proves nothing.

   `lastIndexOf`, not `indexOf`: index.html defines esc four times, once per script
   closure, and the wanted one is the last. The first is written on a single line
   with slightly different spacing, and anchoring to it made the end-marker search
   run on to the next closure's copy — a 3,400-line slice that swallowed a
   `</script>`. Located by index rather than by regex because the definition spans
   two lines and contains braces inside a regex literal, which is the shape that
   defeats a naive pattern. */
const escStart = html.lastIndexOf('function esc(s){ return String(s==null?"":s).replace(');
const escEnd = html.indexOf("}); }", escStart);
if (escStart < 0 || escEnd < 0) {
  console.log("  FAIL  could not locate esc() in index.html");
  process.exit(1);
}
const escSrc = html.slice(escStart, escEnd + "}); }".length);
const esc = new Function(escSrc + "\nreturn esc;")();

function render(msg, i = 0, over = {}) {
  const env = {
    esc,
    history: [msg],
    busy: false,
    paint: () => {},
    $: () => null,
    setTimeout: () => 0,
    ...over,
  };
  const names = Object.keys(env);
  const made = new Function(...names, src + "\nreturn { stepsHTML, stepMs, stepNote, indexOfStep, sayProgress };")(
    ...names.map((k) => env[k])
  );
  return { html: made.stepsHTML(msg, i), fns: made };
}

const step = (over = {}) => ({
  id: "s1", tool: "query.tasks", label: "Look through your tasks",
  state: "done", at: 1, ms: 250, raw: "TASKS — 1:\n  - a task",
  prov: "", tries: 1, after: "", ...over,
});

/* ── the reloaded turn ───────────────────────────────────────────── */
console.log("\na reloaded turn says its detail was dropped");
{
  const { html: out } = render({ role: "assistant", sdrop: 4 });
  check("something is rendered rather than nothing", out.length > 0, JSON.stringify(out));
  check("it says how many steps ran", out.includes("4 step"), out);
  check("it says the detail is not available", /isn.t kept|isn.t available/.test(out), out);
  check("it does not fabricate a timeline", !out.includes("<ol>"), out);
}
{
  const { html: out } = render({ role: "assistant" });
  check("a turn that never ran a tool renders nothing at all", out === "", JSON.stringify(out));
}
{
  const { html: out } = render({ role: "assistant", sdrop: 1 });
  check("one step is singular", out.includes("1 step.") || /1 step\b/.test(out), out);
}

/* ── live and finished states ────────────────────────────────────── */
console.log("\nstates render distinguishably");
{
  const { html: out } = render({ role: "assistant", steps: [step({ state: "running", ms: 0 })] });
  check("a running step is marked running", out.includes('class="run"'), out);
  check("a running timeline says it is working", /Working/.test(out), out);
  check("a running timeline shows no summary note yet", !out.includes("snote"), out);
}
{
  const { html: out } = render({ role: "assistant", steps: [step(), step({ id: "s2", state: "failed" })] });
  check("a failed step is marked failed", out.includes('class="bad"'), out);
  check("the header counts the failures", out.includes("1 failed"), out);
  check("the note warns that dependents may be incomplete", /may be incomplete/.test(out), out);
}
{
  const { html: out } = render({ role: "assistant", steps: [step({ tries: 2 })] });
  check("a retried step shows the attempt", out.includes("attempt 2"), out);
  check("the note mentions the second attempt", /second attempt/.test(out), out);
}
{
  const { html: out } = render({ role: "assistant", steps: [step({ prov: "file" })] });
  check("provenance is shown when a step has it", out.includes("file"), out);
}
{
  const { html: out } = render({ role: "assistant", steps: [step({ prov: "" })] });
  check("a step with no provenance claims none rather than guessing",
    !out.includes("<b></b>"), out);
}

/* ── elapsed time ────────────────────────────────────────────────── */
console.log("\nelapsed time reads like time");
{
  const { fns } = render({ role: "assistant", steps: [step()] });
  check("milliseconds under a second", fns.stepMs(250) === "250 ms", fns.stepMs(250));
  check("seconds with one decimal when short", fns.stepMs(2500) === "2.5 s", fns.stepMs(2500));
  check("seconds without a decimal when long", fns.stepMs(45000) === "45 s", fns.stepMs(45000));
  check("minutes and seconds past a minute", fns.stepMs(95000) === "1 min 35 s", fns.stepMs(95000));
  check("nothing at all for zero", fns.stepMs(0) === "", JSON.stringify(fns.stepMs(0)));
}

/* ── keyboard and safety ─────────────────────────────────────────── */
console.log("\nthe timeline is operable and cannot reach Apply");
{
  const { html: out } = render({ role: "assistant", steps: [step({ raw: "something came back" })] });
  check("a step with a result is a real button", /<button[^>]*data-step=/.test(out), out);
  check("...and declares whether it is expanded", /aria-expanded="false"/.test(out), out);
}
{
  const { html: out } = render({ role: "assistant", steps: [step({ raw: "" })] });
  check("a step with nothing to show is not a button, so Tab skips it",
    !/<button/.test(out), out);
}
{
  const { html: out } = render({ role: "assistant", steps: [step()], open: { s1: true } });
  check("an expanded step shows its raw result", out.includes("<pre"), out);
  check("...and says so to a screen reader", /aria-expanded="true"/.test(out), out);
}
{
  /* The rule that matters: nothing the timeline renders can trigger an action on
     his files. Apply lives on the approval card, which is a different element. */
  const { html: out } = render({
    role: "assistant",
    steps: [step({ raw: "x" })],
    open: { s1: true },
  });
  for (const forbidden of ["data-actapply", "data-actundo", "data-actskip"]) {
    check(`the timeline never emits ${forbidden}`, !out.includes(forbidden), out.slice(0, 80));
  }
}

/* ── untrusted text ─────────────────────────────────────────────── */
console.log("\ntool output is escaped, because a tool result is untrusted text");
{
  const nasty = '</pre><img src=x onerror="alert(1)">';
  const { html: out } = render({
    role: "assistant",
    steps: [step({ raw: nasty, label: nasty, tool: nasty })],
    open: { s1: true },
  });
  check("a script-shaped result cannot close the pre", !out.includes("</pre><img"), out.slice(0, 200));
  check("no raw onerror survives", !/onerror="alert/.test(out), out.slice(0, 200));
  check("the angle brackets are entities", out.includes("&lt;img"), out.slice(0, 200));
}
{
  /* A filename is attacker-controlled in exactly the same way a page is: anyone who
     can put a file in Downloads chooses its name. */
  const { html: out } = render({
    role: "assistant",
    steps: [step({ label: 'Look in "<script>evil</script>"' })],
  });
  check("a hostile filename in a label is escaped", !out.includes("<script>"), out.slice(0, 200));
}

/* ── ordering references ─────────────────────────────────────────── */
console.log("\ndependent steps point at each other legibly");
{
  const a = step({ id: "a", label: "Open the page" });
  const b = step({ id: "b", label: "Read the page", after: "a" });
  const { html: out } = render({ role: "assistant", steps: [a, b] });
  check("a dependent step names the step it waited for", out.includes("after step 1"), out);
}
{
  const { fns } = render({ role: "assistant", steps: [step()] });
  check("an unknown dependency does not throw", fns.indexOfStep([step()], "nope") === -1);
}

console.log(`\n${fail === 0 ? `ALL ${pass} TIMELINE TESTS PASSED` : `${fail} of ${pass + fail} FAILED`}`);
process.exit(fail === 0 ? 0 : 1);
