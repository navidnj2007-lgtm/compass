/**
 * Compass AI proxy — Cloudflare Worker
 *
 * Holds the Qwen API key server-side so it never reaches the browser or the
 * public repo. Forwards chat completions to Alibaba Cloud Model Studio's
 * OpenAI-compatible endpoint and streams the reply straight back.
 *
 * Cloudflare settings to add (Workers → your worker → Settings → Variables):
 *
 *   Secrets (encrypted, never readable again once saved):
 *     QWEN_API_KEY    your Model Studio API key   e.g. sk-xxxxxxxx
 *     APP_SECRET      a passphrase you invent     e.g. a long random phrase
 *
 *   Plain variables:
 *     ALLOWED_ORIGIN  https://navidnj2007-lgtm.github.io
 *     QWEN_BASE       https://dashscope-intl.aliyuncs.com/compatible-mode/v1
 *     QWEN_MODEL      qwen-plus
 *
 * APP_SECRET is the one you also type into Compass on your phone. It is never
 * committed anywhere, which is what stops a stranger who finds this URL from
 * spending your credit.
 */

const LIMITS = {
  maxMessages: 40,
  maxCharsPerMessage: 12000,
  maxTotalChars: 60000,
  maxTokensOut: 1500,
};

const DEFAULTS = {
  base: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
  model: "qwen-plus",
};

function cors(origin) {
  return {
    "Access-Control-Allow-Origin": origin || "*",
    "Access-Control-Allow-Methods": "POST, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type, X-Compass-Secret",
    "Access-Control-Max-Age": "86400",
    Vary: "Origin",
  };
}

function fail(status, message, origin) {
  return new Response(JSON.stringify({ error: message }), {
    status,
    headers: { "Content-Type": "application/json", ...cors(origin) },
  });
}

export default {
  async fetch(request, env) {
    const allowed = env.ALLOWED_ORIGIN || "";
    const origin = request.headers.get("Origin") || "";

    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: cors(allowed || origin) });
    }
    if (request.method !== "POST") {
      return fail(405, "Use POST.", allowed);
    }

    // 1. Only the Compass origin may call this from a browser.
    if (allowed && origin && origin !== allowed) {
      return fail(403, "Origin not allowed.", allowed);
    }

    // 2. Passphrase. This is the real gate — it lives only in Cloudflare and on
    //    your phone, never in the public repo.
    if (!env.APP_SECRET) {
      return fail(500, "Worker is missing APP_SECRET.", allowed);
    }
    const given = request.headers.get("X-Compass-Secret") || "";
    if (given.length !== env.APP_SECRET.length || given !== env.APP_SECRET) {
      return fail(401, "Wrong or missing passphrase.", allowed);
    }
    if (!env.QWEN_API_KEY) {
      return fail(500, "Worker is missing QWEN_API_KEY.", allowed);
    }

    const base = (env.QWEN_BASE || DEFAULTS.base).replace(/\/+$/, "");

    // 3. Validate the body so a bug on the client can't run up a bill.
    let body;
    try {
      body = await request.json();
    } catch {
      return fail(400, "Body must be JSON.", allowed);
    }

    // 3a. Ask the provider which models it actually offers, so the app can
    //     offer a picker instead of guessing at names.
    if (body.action === "models") {
      let list;
      try {
        list = await fetch(`${base}/models`, {
          headers: { Authorization: `Bearer ${env.QWEN_API_KEY}` },
        });
      } catch (e) {
        return fail(502, `Could not reach the provider: ${e.message}`, allowed);
      }
      const text = await list.text();
      return new Response(text, {
        status: list.status,
        headers: {
          "Content-Type": "application/json; charset=utf-8",
          "Cache-Control": "no-store",
          ...cors(allowed || origin),
        },
      });
    }
    const messages = Array.isArray(body.messages) ? body.messages : null;
    if (!messages || !messages.length) {
      return fail(400, "messages[] is required.", allowed);
    }
    if (messages.length > LIMITS.maxMessages) {
      return fail(413, `Too many messages (max ${LIMITS.maxMessages}).`, allowed);
    }
    let total = 0;
    for (const m of messages) {
      if (!m || typeof m.content !== "string" || typeof m.role !== "string") {
        return fail(400, "Each message needs a string role and content.", allowed);
      }
      if (!["system", "user", "assistant"].includes(m.role)) {
        return fail(400, `Unexpected role: ${m.role}`, allowed);
      }
      if (m.content.length > LIMITS.maxCharsPerMessage) {
        return fail(413, "One message is too long.", allowed);
      }
      total += m.content.length;
    }
    if (total > LIMITS.maxTotalChars) {
      return fail(413, "Conversation too long — start a new chat.", allowed);
    }

    const stream = body.stream !== false;
    const chosen =
      typeof body.model === "string" && body.model.length && body.model.length <= 64
        ? body.model
        : null;
    const payload = {
      model: chosen || env.QWEN_MODEL || DEFAULTS.model,
      messages,
      stream,
      temperature: typeof body.temperature === "number"
        ? Math.max(0, Math.min(2, body.temperature))
        : 0.6,
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
        headers: {
          Authorization: `Bearer ${env.QWEN_API_KEY}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify(payload),
      });
    } catch (e) {
      return fail(502, `Could not reach the model provider: ${e.message}`, allowed);
    }

    if (!upstream.ok) {
      const text = await upstream.text().catch(() => "");
      // Never echo the key back, and keep provider errors short.
      return fail(
        upstream.status,
        `Provider returned ${upstream.status}. ${text.slice(0, 300)}`,
        allowed
      );
    }

    return new Response(upstream.body, {
      status: 200,
      headers: {
        "Content-Type": stream
          ? "text/event-stream; charset=utf-8"
          : "application/json; charset=utf-8",
        "Cache-Control": "no-cache, no-store",
        Connection: "keep-alive",
        ...cors(allowed || origin),
      },
    });
  },
};
