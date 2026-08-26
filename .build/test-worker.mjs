/* Behavioural tests for worker.js.
 *
 * The point is not coverage, it is regression: the web app must behave exactly as
 * it did before the desktop shell existed. Every assertion about the GitHub Pages
 * origin and the passphrase encodes behaviour that already worked and must not
 * have changed.
 *
 * Run:  node .build/test-worker.mjs
 */
import worker from "../worker.js";

const WEB = "https://navidnj2007-lgtm.github.io";
const SECRET = "correct-horse-battery-staple";

let pass = 0;
let fail = 0;
function check(label, cond, extra) {
  if (cond) {
    console.log("  ok    " + label);
    pass++;
  } else {
    console.log("  FAIL  " + label + (extra ? "  →  " + extra : ""));
    fail++;
  }
}

/* A Workers KV stub with just enough behaviour for the sync handlers. */
function kv() {
  const store = new Map();
  return {
    get: async (k) => (store.has(k) ? store.get(k) : null),
    put: async (k, v) => void store.set(k, v),
    _store: store,
  };
}

function env(over = {}) {
  return {
    APP_SECRET: SECRET,
    ALLOWED_ORIGIN: WEB,
    QWEN_API_KEY: "test-key",
    SYNC: kv(),
    ...over,
  };
}

function req(body, { origin = WEB, secret = SECRET, method = "POST" } = {}) {
  const headers = { "Content-Type": "application/json" };
  if (origin !== null) headers.Origin = origin;
  if (secret !== null) headers["X-Compass-Secret"] = secret;
  return new Request("https://compass-ai.example.workers.dev/", {
    method,
    headers,
    body: method === "POST" && body !== undefined ? JSON.stringify(body) : undefined,
  });
}

const call = (body, opts, e) => worker.fetch(req(body, opts), e || env());

/* ── the web app, unchanged ──────────────────────────────────────── */
console.log("\nexisting web behaviour is preserved");
{
  const r = await call({ action: "capabilities" });
  const j = await r.json();
  check("capabilities from the web origin returns 200", r.status === 200, `got ${r.status}`);
  check("capabilities reports sync is available", j.sync === true, JSON.stringify(j));
  check(
    "CORS echoes the web origin",
    r.headers.get("Access-Control-Allow-Origin") === WEB,
    r.headers.get("Access-Control-Allow-Origin")
  );
  check("Vary: Origin is set", r.headers.get("Vary") === "Origin");
}
{
  const r = await call({ action: "capabilities" }, { secret: "wrong" });
  check("a wrong passphrase is still 401", r.status === 401, `got ${r.status}`);
}
{
  // The old code compared lengths first; the new one is constant-time. Both must
  // reject, and a same-length wrong secret is the case worth pinning.
  const r = await call({ action: "capabilities" }, { secret: "x".repeat(SECRET.length) });
  check("a same-length wrong passphrase is 401", r.status === 401, `got ${r.status}`);
}
{
  const r = await call({ action: "capabilities" }, { secret: null });
  check("a missing passphrase is 401", r.status === 401, `got ${r.status}`);
}
{
  const r = await call({ action: "capabilities" }, { origin: "https://evil.example.com" });
  check("an unknown origin is still 403", r.status === 403, `got ${r.status}`);
}
{
  const r = await call(undefined, { method: "GET" });
  check("GET is still 405", r.status === 405, `got ${r.status}`);
}
{
  const r = await call(undefined, { method: "OPTIONS" });
  check("preflight is still 204", r.status === 204, `got ${r.status}`);
  check(
    "preflight allows the secret header",
    (r.headers.get("Access-Control-Allow-Headers") || "").includes("X-Compass-Secret")
  );
}
{
  const r = await call({ action: "capabilities" }, { origin: null });
  check("a request with no Origin header still works", r.status === 200, `got ${r.status}`);
}

/* ── sync, unchanged ─────────────────────────────────────────────── */
console.log("\nsync still works the same way");
{
  const e = env();
  let r = await worker.fetch(req({ action: "sync.get" }), e);
  let j = await r.json();
  check("an empty store reports rev 0", r.status === 200 && j.rev === 0, JSON.stringify(j));

  r = await worker.fetch(
    req({ action: "sync.put", rev: 0, state: { domains: [], tasks: [] }, device: "PC" }),
    e
  );
  j = await r.json();
  check("the first push is accepted at rev 1", r.status === 200 && j.rev === 1, JSON.stringify(j));

  r = await worker.fetch(
    req({ action: "sync.put", rev: 0, state: { domains: [], tasks: ["different"] } }),
    e
  );
  check("a stale push conflicts with 409", r.status === 409, `got ${r.status}`);

  r = await worker.fetch(req({ action: "sync.get" }), e);
  j = await r.json();
  check("the stored state comes back", r.status === 200 && j.rev === 1, JSON.stringify(j));

  const huge = { domains: [], big: "x".repeat(1_600_000) };
  r = await worker.fetch(req({ action: "sync.put", rev: 1, state: huge }), e);
  check("an oversized backup is refused with 413", r.status === 413, `got ${r.status}`);
}
{
  const r = await call({ action: "sync.get" }, {}, env({ SYNC: undefined }));
  check("a missing KV binding reports 503", r.status === 503, `got ${r.status}`);
}

/* ── chat validation, unchanged ──────────────────────────────────── */
console.log("\nchat request validation is unchanged");
{
  const r = await call({ messages: [] });
  check("an empty messages array is 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call({ messages: [{ role: "wizard", content: "hi" }] });
  check("an unexpected role is 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call({ messages: Array.from({ length: 41 }, () => ({ role: "user", content: "x" })) });
  check("too many messages is 413", r.status === 413, `got ${r.status}`);
}
{
  const r = await call({
    messages: [
      { role: "user", content: [{ type: "image_url", image_url: { url: "https://evil/x.png" } }] },
    ],
  });
  check("a remote image URL is refused", r.status === 400, `got ${r.status}`);
}
{
  const r = await call({ action: "notion.search", query: "x" }, {}, env({ NOTION_TOKEN: "" }));
  check("Notion without a token reports 503", r.status === 503, `got ${r.status}`);
}

/* ── the new bit: desktop origins ────────────────────────────────── */
console.log("\nthe desktop shell can reach the worker");
for (const origin of ["tauri://localhost", "http://tauri.localhost"]) {
  const r = await call({ action: "capabilities" }, { origin });
  check(`${origin} is allowed`, r.status === 200, `got ${r.status}`);
  check(
    `${origin} is echoed back in CORS`,
    r.headers.get("Access-Control-Allow-Origin") === origin,
    r.headers.get("Access-Control-Allow-Origin")
  );
}
{
  const r = await call({ action: "capabilities" }, { origin: "https://extra.example.com" },
    env({ ALLOWED_ORIGINS: "https://extra.example.com" }));
  check("ALLOWED_ORIGINS adds an origin", r.status === 200, `got ${r.status}`);
}
{
  const r = await call({ action: "capabilities" }, { origin: "tauri://localhost.evil.com" });
  check("a lookalike desktop origin is refused", r.status === 403, `got ${r.status}`);
}
{
  // Belt and braces: the passphrase still gates the desktop origins too, so
  // widening the allowlist did not widen access.
  const r = await call({ action: "capabilities" }, { origin: "tauri://localhost", secret: "wrong" });
  check("a desktop origin without the passphrase is 401", r.status === 401, `got ${r.status}`);
}

/* ── tool calling and the vision binding ─────────────────────────
   These need the outbound request, not just the status code: the whole point of
   the passthrough is what reaches the provider, so the upstream fetch is stubbed
   and the payload inspected. Restored afterwards, because a leaked stub would make
   every later test pass for the wrong reason. */
console.log("\ntool calling passes through");

const realFetch = globalThis.fetch;
let sent = null;
function stubUpstream(bodyText, { status = 200, sse = false } = {}) {
  sent = null;
  globalThis.fetch = async (url, init) => {
    sent = { url: String(url), init, payload: JSON.parse(init.body) };
    return new Response(bodyText, {
      status,
      headers: { "Content-Type": sse ? "text/event-stream" : "application/json" },
    });
  };
}
function unstub() {
  globalThis.fetch = realFetch;
  sent = null;
}

const TOOLS = [
  {
    type: "function",
    function: {
      name: "win.list_files",
      description: "what is in a folder",
      parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
    },
  },
];
const chat = (over = {}) => ({ messages: [{ role: "user", content: "hi" }], stream: false, ...over });

{
  stubUpstream(JSON.stringify({ choices: [{ message: { content: "ok" } }] }));
  const r = await call(chat({ tools: TOOLS, tool_choice: "auto" }));
  check("a request with tools is accepted", r.status === 200, `got ${r.status}`);
  check("tools reach the provider", JSON.stringify(sent?.payload?.tools) === JSON.stringify(TOOLS));
  check("tool_choice reaches the provider", sent?.payload?.tool_choice === "auto");
  unstub();
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await call(chat());
  check("a request without tools sends no tools field", sent && !("tools" in sent.payload));
  check("...and no tool_choice either", sent && !("tool_choice" in sent.payload));
  unstub();
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  const calls = [
    { id: "call_1", type: "function", function: { name: "win.list_files", arguments: '{"path":"~"}' } },
  ];
  const r = await call(
    chat({
      messages: [
        { role: "user", content: "what is in my downloads" },
        { role: "assistant", content: null, tool_calls: calls },
        { role: "tool", tool_call_id: "call_1", content: "FOLDER ~/Downloads\n2 items" },
      ],
    })
  );
  check("an assistant tool_calls turn with null content is accepted", r.status === 200, `got ${r.status}`);
  check("a role:tool result is accepted", r.status === 200);
  check(
    "the tool_calls survive to the provider",
    JSON.stringify(sent?.payload?.messages?.[1]?.tool_calls) === JSON.stringify(calls)
  );
  check("the tool_call_id survives", sent?.payload?.messages?.[2]?.tool_call_id === "call_1");
  unstub();
}
{
  const r = await call(chat({ messages: [{ role: "assistant", content: null }] }));
  check("null content WITHOUT tool_calls is still 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(
    chat({ messages: [{ role: "tool", content: "result with no id" }] })
  );
  check("a tool result with no tool_call_id is 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(
    chat({
      messages: [
        { role: "user", content: "x", tool_calls: [{ id: "a", type: "function", function: { name: "f" } }] },
      ],
    })
  );
  check("only an assistant message may carry tool_calls", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(chat({ tools: Array.from({ length: 25 }, () => TOOLS[0]) }));
  check("too many tools is 400", r.status === 400, `got ${r.status}`);
}
{
  const fat = [
    {
      type: "function",
      function: { name: "win.fat", description: "x".repeat(30000), parameters: {} },
    },
  ];
  const r = await call(chat({ tools: fat }));
  check("an oversized tool schema is 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(chat({ tools: [{ type: "function", function: { name: "no spaces allowed" } }] }));
  check("an unusable tool name is 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(chat({ tools: [] }));
  check("an empty tools array is 400 rather than silently ignored", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(chat({ tool_choice: "auto" }));
  check("tool_choice without tools is 400", r.status === 400, `got ${r.status}`);
}
{
  const r = await call(chat({ tools: TOOLS, tool_choice: "sometimes" }));
  check("an unrecognised tool_choice is 400", r.status === 400, `got ${r.status}`);
}
{
  /* The streamed shape the frontend has to read back. The worker pipes the
     upstream body through untouched, so this pins that delta.tool_calls is not
     mangled or buffered away on the way out. */
  const sse =
    'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9",' +
    '"function":{"name":"win.list_files","arguments":"{\\"path\\""}}]}}]}\n\n' +
    "data: [DONE]\n\n";
  stubUpstream(sse, { sse: true });
  const r = await call(chat({ tools: TOOLS, stream: true }));
  const text = await r.text();
  check("a streamed response is content-type event-stream", (r.headers.get("Content-Type") || "").includes("event-stream"));
  check("delta.tool_calls arrives at the browser intact", text.includes('"tool_calls"') && text.includes("call_9"));
  check("stream_options asks for usage, so a token count is available", sent?.payload?.stream_options?.include_usage === true);
  unstub();
}

console.log("\nthe output cap is raised but still a cap");
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await call(chat());
  check("the default output cap is now 8000", sent?.payload?.max_tokens === 8000, String(sent?.payload?.max_tokens));
  unstub();
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await call(chat({ max_tokens: 500 }));
  check("a smaller max_tokens is honoured", sent?.payload?.max_tokens === 500, String(sent?.payload?.max_tokens));
  unstub();
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await call(chat({ max_tokens: 999999 }));
  check("max_tokens cannot exceed the ceiling", sent?.payload?.max_tokens === 8000, String(sent?.payload?.max_tokens));
  unstub();
}

console.log("\nthe optional vision model");
{
  const r = await call({ action: "capabilities" });
  const j = await r.json();
  check("capabilities reports vision false with no binding", j.vision === false, JSON.stringify(j));
  check("...and names no vision model", j.visionModel === null, JSON.stringify(j));
}
{
  const r = await call({ action: "capabilities" }, {}, env({ VISION_MODEL: "qwen-vl-max" }));
  const j = await r.json();
  check("capabilities reports vision true with a binding", j.vision === true, JSON.stringify(j));
  check("...and names it", j.visionModel === "qwen-vl-max", JSON.stringify(j));
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await worker.fetch(req(chat({ vision: true, model: "qwen3.8-max" })), env({ VISION_MODEL: "qwen-vl-max" }));
  check("a vision turn routes to the vision model", sent?.payload?.model === "qwen-vl-max", String(sent?.payload?.model));
  unstub();
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await call(chat({ vision: true, model: "qwen3.8-max" }));
  check(
    "a vision turn with no binding falls back to the asked-for model rather than failing",
    sent?.payload?.model === "qwen3.8-max",
    String(sent?.payload?.model)
  );
  unstub();
}
{
  stubUpstream(JSON.stringify({ choices: [] }));
  await worker.fetch(req(chat({ model: "qwen3.8-max" })), env({ VISION_MODEL: "qwen-vl-max" }));
  check(
    "an ordinary turn is untouched by the binding",
    sent?.payload?.model === "qwen3.8-max",
    String(sent?.payload?.model)
  );
  unstub();
}

console.log("\na captured screenshot travels the same path a photo does");
{
  /* The point of routing pc.screenshot through the ordinary attachment pipeline is
     that these limits apply to it unchanged. A second image path would be a second
     place for an oversized or wrongly-typed image to get through. */
  const shot = "data:image/jpeg;base64," + "A".repeat(400);
  stubUpstream(JSON.stringify({ choices: [] }));
  const r = await call(
    chat({
      vision: true,
      messages: [
        { role: "user", content: [{ type: "text", text: "what is on my screen" },
                                  { type: "image_url", image_url: { url: shot } }] },
      ],
    })
  );
  check("an agent-captured screenshot is accepted like any image", r.status === 200, `got ${r.status}`);
  check("...and reaches the provider intact",
    JSON.stringify(sent?.payload?.messages?.[0]?.content).includes("data:image/jpeg"), "image lost");
  unstub();
}
{
  const huge = "data:image/jpeg;base64," + "A".repeat(2_500_000);
  const r = await call(
    chat({ messages: [{ role: "user", content: [{ type: "image_url", image_url: { url: huge } }] }] })
  );
  check("an oversized screenshot is refused by the same limit as a photo", r.status === 400, `got ${r.status}`);
}
{
  const five = Array.from({ length: 5 }, () => ({
    type: "image_url",
    image_url: { url: "data:image/png;base64,AAAA" },
  }));
  const r = await call(chat({ messages: [{ role: "user", content: five }] }));
  check("more images than the limit is refused however they were obtained", r.status === 413, `got ${r.status}`);
}

/* The gate still comes first: none of the above is reachable without the
   passphrase, and a tool schema is not a way around it. */
console.log("\nthe passphrase still gates the new fields");
{
  const r = await call(chat({ tools: TOOLS }), { secret: "wrong" });
  check("tools with a wrong passphrase is 401, not 400", r.status === 401, `got ${r.status}`);
}
{
  const r = await call(chat({ tools: TOOLS }), { origin: "https://evil.example.com" });
  check("tools from a bad origin is 403", r.status === 403, `got ${r.status}`);
}

console.log(`\n${fail === 0 ? `ALL ${pass} WORKER TESTS PASSED` : `${fail} of ${pass + fail} FAILED`}`);
process.exit(fail === 0 ? 0 : 1);
