/**
 * Compass AI proxy — Cloudflare Worker
 *
 * Holds the Qwen API key server-side so it never reaches the browser or the
 * public repo. Forwards chat completions to the provider's OpenAI-compatible
 * endpoint and streams the reply straight back.
 *
 * Cloudflare settings (Workers → compass-ai → Settings → Variables and secrets):
 *
 *   Secrets (encrypted, never readable again once saved):
 *     QWEN_API_KEY    the provider API key
 *     APP_SECRET      the passphrase you also type into Compass on your phone
 *
 *   Plain variables:
 *     ALLOWED_ORIGIN  https://navidnj2007-lgtm.github.io
 *     QWEN_BASE       https://qwen.aikit.club/v1
 *     QWEN_MODEL      qwen3.8-max
 *
 * Message content may be a plain string, or an array of OpenAI-style parts
 * ({type:"text"} / {type:"image_url"}) so Compass can send photos and the text
 * pulled out of PDFs and Word documents.
 */

const LIMITS = {
  maxMessages: 40,
  maxCharsPerMessage: 45000,   // one long document plus the question
  maxTotalChars: 120000,       // whole conversation
  maxTokensOut: 1500,
  maxImages: 4,
  maxImageChars: 2400000,      // ~1.8 MB of base64 per image
  maxTotalImageChars: 6000000, // ~4.5 MB across the request
};

const DEFAULTS = {
  base: "https://qwen.aikit.club/v1",
  model: "qwen3.8-max",
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

/**
 * Validate one message's content. Returns {chars, imageChars, images} or
 * {error}. Anything not recognised is rejected rather than passed through.
 */
function measure(content) {
  if (typeof content === "string") {
    return { chars: content.length, imageChars: 0, images: 0 };
  }
  if (!Array.isArray(content) || !content.length) {
    return { error: "content must be a string or a non-empty array of parts" };
  }
  if (content.length > 12) return { error: "too many parts in one message" };

  let chars = 0, imageChars = 0, images = 0;
  for (const part of content) {
    if (!part || typeof part.type !== "string") {
      return { error: "each content part needs a type" };
    }
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
    //    the phone, never in the public repo.
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

    // 3a. Let the app ask which models the provider actually offers, so it can
    //     show a picker instead of guessing at names.
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

    let total = 0, totalImageChars = 0, totalImages = 0;
    for (const m of messages) {
      if (!m || typeof m.role !== "string") {
        return fail(400, "Each message needs a role.", allowed);
      }
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
    if (total > LIMITS.maxTotalChars) {
      return fail(413, "Conversation too long — start a new chat.", allowed);
    }
    if (totalImages > LIMITS.maxImages) {
      return fail(413, `Too many images (max ${LIMITS.maxImages}).`, allowed);
    }
    if (totalImageChars > LIMITS.maxTotalImageChars) {
      return fail(413, "Those images are too large altogether.", allowed);
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
