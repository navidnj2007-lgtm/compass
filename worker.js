/**
 * Compass AI proxy — Cloudflare Worker
 *
 * One endpoint, several jobs, all gated by the same passphrase:
 *   • chat completions, streamed straight back (text + images + tool calls)
 *   • cross-device sync of the Compass state, in Workers KV
 *   • a Notion proxy, so the assistant can read and write Navid's notes
 *
 * Every credential lives here, never in the browser or the public repo.
 *
 * This file is a pipe, and the discipline is to keep it one. It validates shapes
 * and bounds sizes; it does not know what any tool does, cannot run one, and must
 * never learn. Tool names go up to the provider and tool results come back from
 * the desktop app; nothing in between is interpreted here.
 *
 * Cloudflare settings (Workers → compass-ai → Settings):
 *
 *   Secrets:
 *     QWEN_API_KEY    the provider API key
 *     APP_SECRET      the passphrase also typed into Compass on each device
 *     NOTION_TOKEN    a Notion internal integration secret (ntn_...), optional
 *
 *   Plain variables:
 *     ALLOWED_ORIGIN  https://navidnj2007-lgtm.github.io
 *     ALLOWED_ORIGINS optional, comma-separated extra origins. The Windows
 *                     desktop shell's offline origins are allowed already.
 *     QWEN_BASE       https://qwen.aikit.club/v1
 *     QWEN_MODEL      qwen3.8-max
 *     VISION_MODEL    optional. A model that can actually see, used only for
 *                     turns the frontend marks with `vision: true`. Set it if the
 *                     chat model is text-only: computer use depends on reading
 *                     screenshots, and a blind agent does not fail loudly, it
 *                     guesses. Leave it unset and everything behaves as before.
 *
 *   Bindings:
 *     SYNC            KV namespace (compass_sync) — holds one record
 */

const LIMITS = {
  maxMessages: 40,
  maxCharsPerMessage: 45000,
  maxTotalChars: 120000,

  /* Raised from 1500, deliberately and for one reason.
   *
   * 1500 output tokens is comfortable for a chat reply and too small for an
   * agentic one. A turn that plans several steps, explains what it is about to
   * do, and then emits a fenced action block routinely ran past it — and the
   * failure mode is worse than a short answer, because the thing that gets cut
   * off is the end of the reply, which is exactly where the JSON lives. A
   * truncated fence does not parse, so the whole plan is silently lost rather
   * than merely abbreviated.
   *
   * This is a ceiling on how much the model MAY produce, not a spend commitment:
   * generation still stops when the reply is finished, so an ordinary two-sentence
   * answer costs exactly what it did before. Only genuinely long turns cost more,
   * and those are the ones that were being broken. */
  maxTokensOut: 8000,

  maxImages: 4,
  maxImageChars: 2400000,
  maxTotalImageChars: 6000000,
  maxStateBytes: 1500000,
  maxNotionChars: 20000,
  maxNotionOut: 14000,

  /* Tool schemas are capped the same way messages are, and for the same reason:
   * everything the browser can put in the request body is something a compromised
   * browser can put in the request body. The schema is forwarded to the provider
   * rather than interpreted here, so the check is about size and shape, not
   * meaning — a hundred tools with pathological JSON Schema would be a way to
   * burn the provider quota through this worker. */
  maxTools: 24,
  maxToolSchemaChars: 24000,
  maxToolCallIdChars: 128,
};

const DEFAULTS = {
  base: "https://qwen.aikit.club/v1",
  model: "qwen3.8-max",
};

const NOTION_API = "https://api.notion.com/v1";
const NOTION_VERSION = "2022-06-28";
const SYNC_KEY = "compass:state";

/* ── who may call this worker ────────────────────────────────────────
   Compass now runs in three places: the browser (GitHub Pages), the Telegram
   mini app, and the Windows desktop shell. The desktop shell normally loads the
   very same GitHub Pages URL, so its requests carry the ordinary web origin and
   nothing here needs to change for it. But when it falls back to its bundled
   offline copy the page is served locally by Tauri, and the origin becomes
   tauri://localhost (or http://tauri.localhost on Windows).

   So ALLOWED_ORIGIN is still honoured exactly as before, and is still the first
   entry; it has simply grown into a list. Set ALLOWED_ORIGINS (plural) to a
   comma-separated list to add more. The desktop origins are permitted by
   default because they are not reachable by a hostile web page — a browser can
   never forge them, and only code inside the signed desktop bundle can use them.

   None of this is the real gate. The origin header is advisory: it is enforced
   by browsers, not by attackers. APP_SECRET is what actually protects this
   worker, which is why the check below is unchanged. */
const DESKTOP_ORIGINS = ["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

function originList(env) {
  const out = [];
  const push = (v) => {
    String(v || "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean)
      .forEach((s) => { if (!out.includes(s)) out.push(s); });
  };
  push(env.ALLOWED_ORIGIN);
  push(env.ALLOWED_ORIGINS);
  DESKTOP_ORIGINS.forEach((o) => { if (!out.includes(o)) out.push(o); });
  return out;
}

/** The origin to echo back: the caller's if we trust it, else the primary one. */
function pickOrigin(env, requestOrigin) {
  const list = originList(env);
  if (requestOrigin && list.includes(requestOrigin)) return requestOrigin;
  return env.ALLOWED_ORIGIN || list[0] || "";
}

function cors(origin) {
  return {
    "Access-Control-Allow-Origin": origin || "*",
    "Access-Control-Allow-Methods": "POST, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type, X-Compass-Secret",
    "Access-Control-Max-Age": "86400",
    Vary: "Origin",
  };
}

/** Compare in constant time, so the response delay can't leak the passphrase. */
function sameSecret(given, want) {
  const a = String(given || "");
  const b = String(want || "");
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}
function json(obj, status, origin) {
  return new Response(JSON.stringify(obj), {
    status: status || 200,
    headers: { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store", ...cors(origin) },
  });
}
function fail(status, message, origin) {
  return json({ error: message }, status, origin);
}

/* ── message validation ─────────────────────────────────────────────── */

function measure(content) {
  if (typeof content === "string") return { chars: content.length, imageChars: 0, images: 0 };
  if (!Array.isArray(content) || !content.length) {
    return { error: "content must be a string or a non-empty array of parts" };
  }
  if (content.length > 12) return { error: "too many parts in one message" };
  let chars = 0, imageChars = 0, images = 0;
  for (const part of content) {
    if (!part || typeof part.type !== "string") return { error: "each content part needs a type" };
    if (part.type === "text") {
      if (typeof part.text !== "string") return { error: "a text part had no text" };
      chars += part.text.length;
    } else if (part.type === "image_url") {
      const url = part.image_url && part.image_url.url;
      if (typeof url !== "string") return { error: "an image part had no url" };
      if (!/^data:image\/(png|jpe?g|webp|gif);base64,/i.test(url)) {
        return { error: "images must be inline data URLs (png, jpeg, webp or gif)" };
      }
      if (url.length > LIMITS.maxImageChars) return { error: "one image is too large" };
      imageChars += url.length;
      images += 1;
    } else {
      return { error: `unsupported content part: ${part.type}` };
    }
  }
  return { chars, imageChars, images };
}

/* ── tool calling ───────────────────────────────────────────────────
   Compass's own tool protocol is a fenced JSON block in the reply text, and that
   is not going away: it works on every provider, including ones with no function
   calling at all. But where a provider does support the OpenAI shape it is
   markedly more reliable than asking a model to emit valid JSON as prose, so the
   frontend probes for it and uses it when it is there.

   That means three fields have to survive the trip through here — `tools` going
   up, `tool_calls` coming back down and going up again in the next turn, and
   `tool_choice` — plus two message shapes this worker used to reject outright: an
   assistant message with `content: null` because its payload is the tool call, and
   a `role: "tool"` message carrying a result.

   Nothing here interprets a tool. The worker does not know what `win.list_files`
   is, cannot run it, and must not learn: the tool names travel to the provider and
   the results travel back from the desktop app, and this file stays a pipe. What
   it does do is bound the size and check the shape, so the browser cannot use a
   tool schema as an unmetered channel to the provider. */

function validTools(tools) {
  if (!Array.isArray(tools)) return "tools must be an array";
  if (!tools.length) return "tools was an empty array — omit it instead";
  if (tools.length > LIMITS.maxTools) return `too many tools (max ${LIMITS.maxTools})`;

  let size;
  try {
    size = JSON.stringify(tools).length;
  } catch {
    return "tools could not be serialised";
  }
  if (size > LIMITS.maxToolSchemaChars) {
    return `the tool schemas are too large (max ${LIMITS.maxToolSchemaChars} characters)`;
  }

  for (const t of tools) {
    if (!t || t.type !== "function") return "each tool needs type \"function\"";
    const fn = t.function;
    if (!fn || typeof fn.name !== "string" || !fn.name.length) {
      return "each tool needs a function name";
    }
    if (!/^[A-Za-z0-9_.-]{1,64}$/.test(fn.name)) {
      return `"${fn.name.slice(0, 32)}" is not a usable tool name`;
    }
  }
  return null;
}

function validToolChoice(choice) {
  if (typeof choice === "string") {
    return ["auto", "none", "required"].includes(choice)
      ? null
      : `tool_choice must be auto, none, required, or a named function`;
  }
  if (choice && choice.type === "function") {
    const name = choice.function && choice.function.name;
    return typeof name === "string" && /^[A-Za-z0-9_.-]{1,64}$/.test(name)
      ? null
      : "tool_choice named a function without a usable name";
  }
  return "tool_choice was not a shape this worker recognises";
}

function validToolCalls(calls) {
  if (!Array.isArray(calls) || !calls.length) return "tool_calls must be a non-empty array";
  if (calls.length > LIMITS.maxTools) return `too many tool_calls (max ${LIMITS.maxTools})`;
  for (const c of calls) {
    if (!c || typeof c.id !== "string" || !c.id.length) return "a tool_call had no id";
    if (c.id.length > LIMITS.maxToolCallIdChars) return "a tool_call id was too long";
    if (c.type !== "function") return "a tool_call was not a function call";
    const fn = c.function;
    if (!fn || typeof fn.name !== "string" || !fn.name.length) {
      return "a tool_call had no function name";
    }
    if (fn.arguments !== undefined && typeof fn.arguments !== "string") {
      return "tool_call arguments must be a JSON string";
    }
  }
  return null;
}

/* ── Notion helpers ─────────────────────────────────────────────────── */

function notionFetch(env, path, init) {
  return fetch(NOTION_API + path, {
    ...init,
    headers: {
      Authorization: `Bearer ${env.NOTION_TOKEN}`,
      "Notion-Version": NOTION_VERSION,
      "Content-Type": "application/json",
      ...(init && init.headers),
    },
  });
}

/** Accept a bare id, a dashed id, or any Notion URL and return a bare 32-hex id. */
function pageId(v) {
  const m = String(v || "").match(/[0-9a-f]{32}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i);
  return m ? m[0].replace(/-/g, "") : "";
}

function plain(rich) {
  return Array.isArray(rich) ? rich.map((r) => (r && r.plain_text) || "").join("") : "";
}

function titleOf(obj) {
  if (!obj) return "(untitled)";
  if (Array.isArray(obj.title) && obj.title.length) return plain(obj.title) || "(untitled)";
  const props = obj.properties || {};
  for (const k of Object.keys(props)) {
    const p = props[k];
    if (p && p.type === "title") return plain(p.title) || "(untitled)";
  }
  return "(untitled)";
}

/** Flatten a list of Notion blocks into readable plain text. */
function blocksToText(blocks) {
  const out = [];
  for (const b of blocks) {
    const t = b && b.type;
    if (!t) continue;
    const d = b[t] || {};
    const txt = plain(d.rich_text);
    switch (t) {
      case "heading_1": out.push("\n# " + txt); break;
      case "heading_2": out.push("\n## " + txt); break;
      case "heading_3": out.push("\n### " + txt); break;
      case "bulleted_list_item": out.push("- " + txt); break;
      case "numbered_list_item": out.push("1. " + txt); break;
      case "to_do": out.push((d.checked ? "[x] " : "[ ] ") + txt); break;
      case "quote": out.push("> " + txt); break;
      case "callout": out.push("> " + txt); break;
      case "toggle": out.push("- " + txt); break;
      case "code": out.push("```" + (d.language || "") + "\n" + txt + "\n```"); break;
      case "equation": out.push("$$ " + (d.expression || "") + " $$"); break;
      case "child_page": out.push("[sub-page] " + (d.title || "")); break;
      case "child_database": out.push("[database] " + (d.title || "")); break;
      case "divider": out.push("---"); break;
      case "image": case "video": case "file": case "pdf":
        out.push("[" + t + (d.caption ? ": " + plain(d.caption) : "") + "]"); break;
      case "table_row":
        out.push("| " + (d.cells || []).map((c) => plain(c)).join(" | ") + " |"); break;
      default:
        if (txt) out.push(txt);
    }
  }
  return out.join("\n");
}

/** Read a page's blocks, following children one level down, capped. */
async function readPage(env, id, budget) {
  let text = "", cursor = null, guard = 0;
  while (guard++ < 6) {
    const q = new URLSearchParams({ page_size: "100" });
    if (cursor) q.set("start_cursor", cursor);
    const r = await notionFetch(env, `/blocks/${id}/children?${q}`, { method: "GET" });
    if (!r.ok) {
      const body = await r.text().catch(() => "");
      throw Object.assign(new Error(`Notion ${r.status}. ${body.slice(0, 200)}`), { status: r.status });
    }
    const j = await r.json();
    const blocks = j.results || [];
    text += (text ? "\n" : "") + blocksToText(blocks);

    // one level of nesting (toggles, columns, synced blocks)
    for (const b of blocks) {
      if (text.length >= budget) break;
      if (b.has_children && b.type !== "child_page" && b.type !== "child_database") {
        const rc = await notionFetch(env, `/blocks/${b.id}/children?page_size=100`, { method: "GET" });
        if (rc.ok) {
          const jc = await rc.json();
          const sub = blocksToText(jc.results || []);
          if (sub) text += "\n" + sub.split("\n").map((l) => "  " + l).join("\n");
        }
      }
    }
    if (text.length >= budget || !j.has_more) break;
    cursor = j.next_cursor;
  }
  return text.length > budget ? text.slice(0, budget) + "\n[...page continues]" : text;
}

/** Turn plain text into Notion blocks. Understands -, 1., # and ## only. */
function textToBlocks(text) {
  const lines = String(text).split(/\r?\n/);
  const blocks = [];
  for (const raw of lines) {
    if (blocks.length >= 90) break;
    const line = raw.replace(/\s+$/, "");
    if (!line.trim()) continue;
    let type = "paragraph", content = line;
    if (/^###\s+/.test(line)) { type = "heading_3"; content = line.replace(/^###\s+/, ""); }
    else if (/^##\s+/.test(line)) { type = "heading_2"; content = line.replace(/^##\s+/, ""); }
    else if (/^#\s+/.test(line)) { type = "heading_2"; content = line.replace(/^#\s+/, ""); }
    else if (/^[-*]\s+/.test(line)) { type = "bulleted_list_item"; content = line.replace(/^[-*]\s+/, ""); }
    else if (/^\d+[.)]\s+/.test(line)) { type = "numbered_list_item"; content = line.replace(/^\d+[.)]\s+/, ""); }
    blocks.push({
      object: "block", type,
      [type]: { rich_text: [{ type: "text", text: { content: content.slice(0, 1900) } }] },
    });
  }
  if (!blocks.length) {
    blocks.push({ object: "block", type: "paragraph", paragraph: { rich_text: [] } });
  }
  return blocks;
}

async function handleNotion(env, body, allowed) {
  if (!env.NOTION_TOKEN) {
    return fail(503, "Notion isn't connected yet — add NOTION_TOKEN in Cloudflare.", allowed);
  }
  const act = body.action;
  try {
    if (act === "notion.search") {
      const query = String(body.query || "").slice(0, 200);
      const payload = { query, page_size: Math.min(12, Math.max(1, body.limit || 8)) };
      if (body.kind === "page" || body.kind === "database") {
        payload.filter = { property: "object", value: body.kind };
      }
      const r = await notionFetch(env, "/search", { method: "POST", body: JSON.stringify(payload) });
      if (!r.ok) return fail(r.status, `Notion ${r.status}. ${(await r.text()).slice(0, 200)}`, allowed);
      const j = await r.json();
      return json({
        results: (j.results || []).map((x) => ({
          id: x.id, object: x.object, title: titleOf(x),
          url: x.url, edited: x.last_edited_time,
        })),
      }, 200, allowed);
    }

    if (act === "notion.read") {
      const id = pageId(body.id);
      if (!id) return fail(400, "notion.read needs a page id.", allowed);
      const meta = await notionFetch(env, `/pages/${id}`, { method: "GET" });
      let title = "(untitled)";
      if (meta.ok) title = titleOf(await meta.json());
      const text = await readPage(env, id, LIMITS.maxNotionOut);
      return json({ id, title, text }, 200, allowed);
    }

    if (act === "notion.append") {
      const id = pageId(body.id);
      const text = String(body.text || "").slice(0, LIMITS.maxNotionChars);
      if (!id) return fail(400, "notion.append needs a page id.", allowed);
      if (!text.trim()) return fail(400, "notion.append needs some text.", allowed);
      const r = await notionFetch(env, `/blocks/${id}/children`, {
        method: "PATCH",
        body: JSON.stringify({ children: textToBlocks(text) }),
      });
      if (!r.ok) return fail(r.status, `Notion ${r.status}. ${(await r.text()).slice(0, 200)}`, allowed);
      return json({ ok: true, id }, 200, allowed);
    }

    if (act === "notion.create") {
      const parent = pageId(body.parent);
      const title = String(body.title || "Untitled").slice(0, 200);
      const text = String(body.text || "").slice(0, LIMITS.maxNotionChars);
      if (!parent) return fail(400, "notion.create needs a parent page id.", allowed);
      const payload = {
        parent: { page_id: parent },
        properties: { title: { title: [{ type: "text", text: { content: title } }] } },
        children: textToBlocks(text),
      };
      const r = await notionFetch(env, "/pages", { method: "POST", body: JSON.stringify(payload) });
      if (!r.ok) return fail(r.status, `Notion ${r.status}. ${(await r.text()).slice(0, 200)}`, allowed);
      const j = await r.json();
      return json({ ok: true, id: j.id, url: j.url, title }, 200, allowed);
    }
  } catch (e) {
    return fail(502, `Notion request failed: ${e.message}`, allowed);
  }
  return fail(400, `Unknown Notion action: ${act}`, allowed);
}

/* ── sync ───────────────────────────────────────────────────────────── */

async function handleSync(env, body, allowed) {
  if (!env.SYNC) {
    return fail(503, "Sync isn't set up — the KV binding is missing.", allowed);
  }
  const raw = await env.SYNC.get(SYNC_KEY);
  const stored = raw ? JSON.parse(raw) : null;

  if (body.action === "sync.get") {
    if (!stored) return json({ rev: 0, state: null }, 200, allowed);
    return json(stored, 200, allowed);
  }

  // sync.put
  const state = body.state;
  if (!state || typeof state !== "object") return fail(400, "sync.put needs a state object.", allowed);
  const encoded = JSON.stringify(state);
  if (encoded.length > LIMITS.maxStateBytes) {
    return fail(413, "That backup is too large to sync.", allowed);
  }
  const baseRev = typeof body.rev === "number" ? body.rev : -1;
  const currentRev = stored ? stored.rev : 0;

  if (!body.force && baseRev !== currentRev) {
    return json({
      conflict: true, rev: currentRev,
      updatedAt: stored ? stored.updatedAt : null,
      device: stored ? stored.device : null,
      state: stored ? stored.state : null,
    }, 409, allowed);
  }

  const record = {
    rev: currentRev + 1,
    updatedAt: new Date().toISOString(),
    device: String(body.device || "a device").slice(0, 40),
    state,
  };
  await env.SYNC.put(SYNC_KEY, JSON.stringify(record));
  return json({ ok: true, rev: record.rev, updatedAt: record.updatedAt }, 200, allowed);
}

/* ── entry point ────────────────────────────────────────────────────── */

export default {
  async fetch(request, env) {
    const origin = request.headers.get("Origin") || "";
    /* `allowed` is what gets echoed in Access-Control-Allow-Origin and passed to
       every helper below, exactly as before — but it now reflects which of the
       permitted origins actually called, so the desktop shell's offline mode
       gets a usable CORS header instead of the web one. */
    const allowed = pickOrigin(env, origin);
    const list = originList(env);

    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: cors(allowed || origin) });
    }
    if (request.method !== "POST") return fail(405, "Use POST.", allowed);
    if (origin && list.length && !list.includes(origin)) return fail(403, "Origin not allowed.", allowed);

    if (!env.APP_SECRET) return fail(500, "Worker is missing APP_SECRET.", allowed);
    const given = request.headers.get("X-Compass-Secret") || "";
    if (!sameSecret(given, env.APP_SECRET)) {
      return fail(401, "Wrong or missing passphrase.", allowed);
    }

    let body;
    try { body = await request.json(); }
    catch { return fail(400, "Body must be JSON.", allowed); }

    const act = body.action;

    if (act === "sync.get" || act === "sync.put") return handleSync(env, body, allowed);
    if (typeof act === "string" && act.indexOf("notion.") === 0) return handleNotion(env, body, allowed);

    if (act === "capabilities") {
      /* `vision` says whether a separate vision model is configured, which is what
         lets the UI be honest instead of hopeful: with no binding and a text-only
         chat model, screenshots are worth nothing and the app should say so rather
         than let the agent narrate a screen it cannot see. It reports the binding,
         not a claim about the default model — nobody here can know whether
         qwen3.8-max accepts an image except by asking it. */
      return json({
        sync: !!env.SYNC,
        notion: !!env.NOTION_TOKEN,
        model: env.QWEN_MODEL || DEFAULTS.model,
        vision: !!(typeof env.VISION_MODEL === "string" && env.VISION_MODEL.length),
        visionModel: (typeof env.VISION_MODEL === "string" && env.VISION_MODEL) || null,
      }, 200, allowed);
    }

    if (!env.QWEN_API_KEY) return fail(500, "Worker is missing QWEN_API_KEY.", allowed);
    const base = (env.QWEN_BASE || DEFAULTS.base).replace(/\/+$/, "");

    if (act === "models") {
      let list;
      try {
        list = await fetch(`${base}/models`, { headers: { Authorization: `Bearer ${env.QWEN_API_KEY}` } });
      } catch (e) {
        return fail(502, `Could not reach the provider: ${e.message}`, allowed);
      }
      const text = await list.text();
      return new Response(text, {
        status: list.status,
        headers: { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store", ...cors(allowed || origin) },
      });
    }

    const messages = Array.isArray(body.messages) ? body.messages : null;
    if (!messages || !messages.length) return fail(400, "messages[] is required.", allowed);
    if (messages.length > LIMITS.maxMessages) {
      return fail(413, `Too many messages (max ${LIMITS.maxMessages}).`, allowed);
    }

    let total = 0, totalImageChars = 0, totalImages = 0;
    for (const m of messages) {
      if (!m || typeof m.role !== "string") return fail(400, "Each message needs a role.", allowed);
      if (!["system", "user", "assistant", "tool"].includes(m.role)) {
        return fail(400, `Unexpected role: ${m.role}`, allowed);
      }

      /* An assistant turn whose payload is a tool call carries no prose, so its
         content is null. That used to be rejected here, which is the reason tool
         calling could not work at all: the model's own reply, fed back on the next
         round exactly as the provider returned it, failed validation. */
      const calls = m.tool_calls;
      if (calls !== undefined) {
        if (m.role !== "assistant") return fail(400, "Only an assistant message may carry tool_calls.", allowed);
        const why = validToolCalls(calls);
        if (why) return fail(400, why, allowed);
      }
      if (m.role === "tool" && (typeof m.tool_call_id !== "string" || !m.tool_call_id.length)) {
        return fail(400, "A tool result needs the tool_call_id it answers.", allowed);
      }
      if (typeof m.tool_call_id === "string" && m.tool_call_id.length > LIMITS.maxToolCallIdChars) {
        return fail(400, "A tool_call_id was too long.", allowed);
      }

      let got;
      if (m.content === null || m.content === undefined) {
        // Permitted only when the message says something a different way.
        if (!(m.role === "assistant" && calls !== undefined)) {
          return fail(400, "A message with no content must carry tool_calls.", allowed);
        }
        got = { chars: 0, imageChars: 0, images: 0 };
      } else {
        got = measure(m.content);
      }
      if (got.error) return fail(400, got.error, allowed);
      if (got.chars > LIMITS.maxCharsPerMessage) {
        return fail(413, "One message is too long — shorten it or attach less.", allowed);
      }
      total += got.chars;
      totalImageChars += got.imageChars;
      totalImages += got.images;
    }
    if (total > LIMITS.maxTotalChars) return fail(413, "Conversation too long — start a new chat.", allowed);
    if (totalImages > LIMITS.maxImages) return fail(413, `Too many images (max ${LIMITS.maxImages}).`, allowed);
    if (totalImageChars > LIMITS.maxTotalImageChars) return fail(413, "Those images are too large altogether.", allowed);

    const stream = body.stream !== false;
    const asked =
      typeof body.model === "string" && body.model.length && body.model.length <= 64 ? body.model : null;

    /* Which model answers.
     *
     * `vision: true` is the frontend saying "this turn contains a screenshot and
     * the answer depends on seeing it". If a separate vision model is configured,
     * that turn goes there and the ordinary model choice is overridden, because a
     * text-only model handed an image does not fail loudly — it guesses, and a
     * computer-use agent that guesses at coordinates is worse than one that admits
     * it cannot see.
     *
     * With no VISION_MODEL binding this expression collapses to exactly what it was
     * before: the model the user picked, else the worker default. */
    const wantsVision = body.vision === true;
    const visionModel = typeof env.VISION_MODEL === "string" && env.VISION_MODEL.length
      ? env.VISION_MODEL
      : null;
    const chosen = wantsVision && visionModel ? visionModel : asked;

    const payload = {
      model: chosen || env.QWEN_MODEL || DEFAULTS.model,
      messages,
      stream,
      temperature: typeof body.temperature === "number" ? Math.max(0, Math.min(2, body.temperature)) : 0.6,
      max_tokens: Math.min(
        LIMITS.maxTokensOut,
        typeof body.max_tokens === "number" ? body.max_tokens : LIMITS.maxTokensOut
      ),
    };

    /* Forwarded only when asked for, so a request that does not mention tools
       produces byte-identical upstream JSON to the one it produced before this
       feature existed. That is what lets the fenced-block protocol keep working
       untouched on a provider that has no function calling. */
    if (body.tools !== undefined) {
      const why = validTools(body.tools);
      if (why) return fail(400, why, allowed);
      payload.tools = body.tools;
    }
    if (body.tool_choice !== undefined) {
      if (payload.tools === undefined) {
        return fail(400, "tool_choice was sent without any tools.", allowed);
      }
      const why = validToolChoice(body.tool_choice);
      if (why) return fail(400, why, allowed);
      payload.tool_choice = body.tool_choice;
    }

    if (stream) payload.stream_options = { include_usage: true };

    let upstream;
    try {
      upstream = await fetch(`${base}/chat/completions`, {
        method: "POST",
        headers: { Authorization: `Bearer ${env.QWEN_API_KEY}`, "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
    } catch (e) {
      return fail(502, `Could not reach the model provider: ${e.message}`, allowed);
    }

    if (!upstream.ok) {
      const text = await upstream.text().catch(() => "");
      return fail(upstream.status, `Provider returned ${upstream.status}. ${text.slice(0, 300)}`, allowed);
    }

    return new Response(upstream.body, {
      status: 200,
      headers: {
        "Content-Type": stream ? "text/event-stream; charset=utf-8" : "application/json; charset=utf-8",
        "Cache-Control": "no-cache, no-store",
        Connection: "keep-alive",
        ...cors(allowed || origin),
      },
    });
  },
};
