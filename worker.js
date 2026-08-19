/**
 * Compass AI proxy — Cloudflare Worker
 *
 * One endpoint, several jobs, all gated by the same passphrase:
 *   • chat completions, streamed straight back (text + images)
 *   • cross-device sync of the Compass state, in Workers KV
 *   • a Notion proxy, so the assistant can read and write Navid's notes
 *
 * Every credential lives here, never in the browser or the public repo.
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
 *     QWEN_BASE       https://qwen.aikit.club/v1
 *     QWEN_MODEL      qwen3.8-max
 *
 *   Bindings:
 *     SYNC            KV namespace (compass_sync) — holds one record
 */

const LIMITS = {
  maxMessages: 40,
  maxCharsPerMessage: 45000,
  maxTotalChars: 120000,
  maxTokensOut: 1500,
  maxImages: 4,
  maxImageChars: 2400000,
  maxTotalImageChars: 6000000,
  maxStateBytes: 1500000,
  maxNotionChars: 20000,
  maxNotionOut: 14000,
};

const DEFAULTS = {
  base: "https://qwen.aikit.club/v1",
  model: "qwen3.8-max",
};

const NOTION_API = "https://api.notion.com/v1";
const NOTION_VERSION = "2022-06-28";
const SYNC_KEY = "compass:state";

function cors(origin) {
  return {
    "Access-Control-Allow-Origin": origin || "*",
    "Access-Control-Allow-Methods": "POST, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type, X-Compass-Secret",
    "Access-Control-Max-Age": "86400",
    Vary: "Origin",
  };
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
    const allowed = env.ALLOWED_ORIGIN || "";
    const origin = request.headers.get("Origin") || "";

    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: cors(allowed || origin) });
    }
    if (request.method !== "POST") return fail(405, "Use POST.", allowed);
    if (allowed && origin && origin !== allowed) return fail(403, "Origin not allowed.", allowed);

    if (!env.APP_SECRET) return fail(500, "Worker is missing APP_SECRET.", allowed);
    const given = request.headers.get("X-Compass-Secret") || "";
    if (given.length !== env.APP_SECRET.length || given !== env.APP_SECRET) {
      return fail(401, "Wrong or missing passphrase.", allowed);
    }

    let body;
    try { body = await request.json(); }
    catch { return fail(400, "Body must be JSON.", allowed); }

    const act = body.action;

    if (act === "sync.get" || act === "sync.put") return handleSync(env, body, allowed);
    if (typeof act === "string" && act.indexOf("notion.") === 0) return handleNotion(env, body, allowed);

    if (act === "capabilities") {
      return json({ sync: !!env.SYNC, notion: !!env.NOTION_TOKEN, model: env.QWEN_MODEL || DEFAULTS.model }, 200, allowed);
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
      if (!["system", "user", "assistant"].includes(m.role)) {
        return fail(400, `Unexpected role: ${m.role}`, allowed);
      }
      const got = measure(m.content);
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
    const chosen =
      typeof body.model === "string" && body.model.length && body.model.length <= 64 ? body.model : null;
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
