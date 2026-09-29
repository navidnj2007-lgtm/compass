/**
 * Compass AI proxy — Cloudflare Worker
 *
 * One endpoint, several jobs, all gated by the same passphrase:
 *   • chat completions, streamed straight back (text + images)
 *   • cross-device sync of the Compass state, in Workers KV
 *   • a Notion proxy, so the assistant can read and write Navid's notes
 *   • /health, where an iPhone Shortcut drops Apple Watch data (own key)
 *
 * Every credential lives here, never in the browser or the public repo.
 *
 * Cloudflare settings (Workers → compass-ai → Settings):
 *
 *   Secrets:
 *     QWEN_API_KEY    the provider API key
 *     APP_SECRET      the passphrase also typed into Compass on each device
 *     NOTION_TOKEN    a Notion internal integration secret (ntn_...), optional
 *     HEALTH_KEY      a long random key used only by the iPhone Health Shortcut, optional
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

/* ── Apple Health (via an iPhone Shortcut) ─────────────────────────────
 *
 * The Watch syncs into the Health app on the iPhone. A personal automation
 * in Shortcuts reads the last few days from Health and POSTs them here:
 *
 *   POST <worker url>/health
 *   X-Health-Key: <HEALTH_KEY>          (a second secret, write-only scope)
 *   { "sleep": "...", "steps": "...", "energy": "...", "exercise": "...",
 *     "rhr": "...", "hrv": "..." }
 *
 * Each field is text, one sample per line: value;unit;start;end (dates in
 * ISO 8601). Values in any locale are accepted (8.432 / 8,432 / 62,5).
 * The worker boils the samples down to one small record per day and keeps
 * 400 days in KV under compass:health. Compass reads it with health.get,
 * behind the normal passphrase. HEALTH_KEY can only add data, never read.
 */
const HEALTH_KEY_KV = "compass:health";
const HEALTH_DAYS = 400;
const HEALTH_MAX_BODY = 400000;

function sameSecret(a, b) {
  a = String(a || ""); b = String(b || "");
  if (!a || !b || a.length !== b.length) return false;
  let d = 0;
  for (let i = 0; i < a.length; i++) d |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return d === 0;
}

/** "8.432", "8,432", "8 432", "62,5", "1.234,5" -> number. `big` = counts that are never fractional. */
function healthNum(raw, big) {
  let s = String(raw || "").replace(/[\s  '’]/g, "").replace(/[^0-9.,\-]/g, "");
  if (!s) return NaN;
  const hasDot = s.indexOf(".") > -1, hasComma = s.indexOf(",") > -1;
  if (hasDot && hasComma) {
    const dec = s.lastIndexOf(".") > s.lastIndexOf(",") ? "." : ",";
    s = s.split(dec === "." ? "," : ".").join("").replace(",", ".");
  } else if (hasDot || hasComma) {
    const sep = hasDot ? "." : ",";
    const parts = s.split(sep);
    if (parts.length > 2) s = parts.join("");
    else if (big && parts[1].length === 3) s = parts.join("");
    else s = parts.join(".");
  }
  return parseFloat(s);
}

function healthLocal(ms, tz) {
  const f = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz, year: "numeric", month: "2-digit", day: "2-digit",
    hour: "2-digit", minute: "2-digit", hourCycle: "h23",
  }).formatToParts(new Date(ms));
  const g = (t) => (f.find((x) => x.type === t) || {}).value;
  return { date: `${g("year")}-${g("month")}-${g("day")}`, hm: `${g("hour")}:${g("minute")}` };
}

function healthLines(v) {
  if (Array.isArray(v)) v = v.join("\n");
  return String(v || "").split(/\r?\n/).map((l) => l.trim()).filter(Boolean).slice(0, 6000)
    .map((l) => {
      const p = l.split(";").map((x) => x.trim());
      if (p.length === 3) p.splice(1, 0, "");
      const start = Date.parse(p[2]), end = Date.parse(p[3] || p[2]);
      return { v: p[0], unit: (p[1] || "").toLowerCase(), start, end };
    })
    .filter((x) => !isNaN(x.start));
}

function sleepKind(v) {
  const s = String(v).toLowerCase();
  if (/awake|vågen|vaagen|wach|réveil|despierto/.test(s)) return "awake";
  if (/in ?bed|i seng|im bett|au lit|en cama/.test(s)) return "inbed";
  if (/deep|dyb|tief|profond|profundo/.test(s)) return "deep";
  if (/\brem\b/.test(s)) return "rem";
  if (/core|kerne|kern|essentiel|principal/.test(s)) return "core";
  return "asleep";
}

function unionMinutes(iv) {
  iv = iv.filter((x) => x[1] > x[0]).sort((a, b) => a[0] - b[0]);
  let tot = 0, cs = -1, ce = -1;
  for (const [s, e] of iv) {
    if (s > ce) { if (ce > cs) tot += ce - cs; cs = s; ce = e; }
    else if (e > ce) ce = e;
  }
  if (ce > cs) tot += ce - cs;
  return Math.round(tot / 60000);
}

function healthDigest(body, tz) {
  const days = {};
  const day = (k) => (days[k] = days[k] || {});

  // sleep: a night belongs to the day you wake up (anything ending before 18:00)
  const nights = {};
  for (const x of healthLines(body.sleep)) {
    if (isNaN(x.end) || x.end <= x.start || x.end - x.start > 20 * 3600000) continue;
    const k = healthLocal(x.end + 6 * 3600000, tz).date;
    (nights[k] = nights[k] || []).push({ kind: sleepKind(x.v), s: x.start, e: x.end });
  }
  for (const k of Object.keys(nights)) {
    const n = nights[k];
    const asleep = n.filter((x) => x.kind !== "awake" && x.kind !== "inbed");
    const inbed = n.filter((x) => x.kind === "inbed");
    const sum = (kind) => Math.round(n.filter((x) => x.kind === kind).reduce((a, x) => a + (x.e - x.s), 0) / 60000);
    let min = unionMinutes(asleep.map((x) => [x.s, x.e]));
    const bedMin = unionMinutes(inbed.map((x) => [x.s, x.e]));
    const src = asleep.length ? asleep : inbed;
    if (!min && bedMin) min = Math.max(0, bedMin - sum("awake"));
    if (!min) continue;
    const first = Math.min(...src.map((x) => x.s)), last = Math.max(...src.map((x) => x.e));
    day(k).sleep = {
      min, deep: sum("deep"), rem: sum("rem"), core: sum("core"), awake: sum("awake"),
      inbed: bedMin, bed: healthLocal(first, tz).hm, wake: healthLocal(last, tz).hm,
      staged: asleep.some((x) => x.kind === "deep" || x.kind === "rem" || x.kind === "core"),
    };
  }

  // daily totals
  const totals = { steps: [true, 1], energy: [true, 1], exercise: [false, 1] };
  for (const key of Object.keys(totals)) {
    const acc = {};
    for (const x of healthLines(body[key])) {
      let v = healthNum(x.v, totals[key][0]);
      if (!isFinite(v) || v < 0) continue;
      if (key === "energy" && /kj/.test(x.unit)) v = v / 4.184;
      if (key === "exercise" && /^(s|sec)/.test(x.unit)) v = v / 60;
      if (key === "exercise" && /^(h|hr)/.test(x.unit)) v = v * 60;
      const k = healthLocal(x.start, tz).date;
      acc[k] = (acc[k] || 0) + v;
    }
    for (const k of Object.keys(acc)) day(k)[key] = Math.round(acc[k]);
  }

  // daily averages
  const avgs = { rhr: [25, 200], hrv: [3, 300] };
  for (const key of Object.keys(avgs)) {
    const acc = {};
    for (const x of healthLines(body[key])) {
      const v = healthNum(x.v, false);
      if (!isFinite(v) || v < avgs[key][0] || v > avgs[key][1]) continue;
      const k = healthLocal(x.start, tz).date;
      (acc[k] = acc[k] || []).push(v);
    }
    for (const k of Object.keys(acc)) {
      const a = acc[k];
      day(k)[key] = Math.round(a.reduce((s, v) => s + v, 0) / a.length * 10) / 10;
      day(k)[key + "N"] = a.length;
    }
  }
  return days;
}

function healthMerge(old, add) {
  const out = Object.assign({}, old);
  for (const k of Object.keys(add)) {
    const o = Object.assign({}, out[k] || {}), n = add[k];
    if (n.sleep && (!o.sleep || n.sleep.min >= o.sleep.min)) o.sleep = n.sleep;
    for (const f of ["steps", "energy", "exercise"]) if (n[f] != null) o[f] = Math.max(o[f] || 0, n[f]);
    for (const f of ["rhr", "hrv"]) if (n[f] != null && (n[f + "N"] || 0) >= (o[f + "N"] || 0)) { o[f] = n[f]; o[f + "N"] = n[f + "N"]; }
    o.at = new Date().toISOString();
    out[k] = o;
  }
  const keys = Object.keys(out).sort();
  while (keys.length > HEALTH_DAYS) delete out[keys.shift()];
  return out;
}

function hm(min) { return `${Math.floor(min / 60)}h ${String(min % 60).padStart(2, "0")}m`; }

async function handleHealthPush(request, env) {
  const plainJson = (o, s) => new Response(JSON.stringify(o), { status: s || 200, headers: { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" } });
  if (request.method !== "POST") return plainJson({ error: "Use POST." }, 405);
  if (!env.HEALTH_KEY) return plainJson({ error: "Add the HEALTH_KEY secret in Cloudflare first." }, 503);
  if (!sameSecret(request.headers.get("X-Health-Key"), env.HEALTH_KEY)) return plainJson({ error: "Wrong or missing X-Health-Key." }, 401);
  if (!env.SYNC) return plainJson({ error: "The SYNC KV binding is missing." }, 503);
  const text = await request.text();
  if (text.length > HEALTH_MAX_BODY) return plainJson({ error: "Too much data in one go. Use a shorter date range." }, 413);
  let body;
  try { body = JSON.parse(text); } catch { return plainJson({ error: "Body must be JSON." }, 400); }
  const tz = typeof body.tz === "string" && /^[A-Za-z_]+\/[A-Za-z_]+$/.test(body.tz) ? body.tz : (env.HEALTH_TZ || "Europe/Copenhagen");
  let add;
  try { add = healthDigest(body || {}, tz); } catch (e) { return plainJson({ error: `Could not read the samples: ${e.message}` }, 400); }
  const raw = await env.SYNC.get(HEALTH_KEY_KV);
  const rec = raw ? JSON.parse(raw) : { days: {} };
  rec.days = healthMerge(rec.days || {}, add);
  rec.updatedAt = new Date().toISOString();
  rec.pushes = (rec.pushes || 0) + 1;
  await env.SYNC.put(HEALTH_KEY_KV, JSON.stringify(rec));
  const today = healthLocal(Date.now(), tz).date, t = rec.days[today] || {};
  const bits = [];
  if (t.sleep) bits.push(`Slept ${hm(t.sleep.min)}`);
  if (t.rhr) bits.push(`resting HR ${Math.round(t.rhr)}`);
  if (t.hrv) bits.push(`HRV ${Math.round(t.hrv)} ms`);
  return plainJson({ ok: true, days: Object.keys(add).length, summary: bits.length ? bits.join(" · ") : "Saved. Nothing for today yet." });
}

async function handleHealth(env, body, allowed) {
  if (!env.SYNC) return fail(503, "Sync isn't set up — the KV binding is missing.", allowed);
  if (body.action === "health.clear") {
    await env.SYNC.delete(HEALTH_KEY_KV);
    return json({ ok: true }, 200, allowed);
  }
  const raw = await env.SYNC.get(HEALTH_KEY_KV);
  const rec = raw ? JSON.parse(raw) : { days: {} };
  return json({ ok: true, keySet: !!env.HEALTH_KEY, updatedAt: rec.updatedAt || null, days: rec.days || {} }, 200, allowed);
}

/* ── entry point ────────────────────────────────────────────────────── */

export default {
  async fetch(request, env) {
    if (new URL(request.url).pathname.replace(/\/+$/, "") === "/health") return handleHealthPush(request, env);
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
    if (act === "health.get" || act === "health.clear") return handleHealth(env, body, allowed);

    if (act === "capabilities") {
      return json({ sync: !!env.SYNC, notion: !!env.NOTION_TOKEN, health: !!env.HEALTH_KEY, model: env.QWEN_MODEL || DEFAULTS.model }, 200, allowed);
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
