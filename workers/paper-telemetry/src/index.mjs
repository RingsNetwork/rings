/**
 * First-party, cookie-free paper reading & download telemetry Worker for https://rings.rs/papers/.
 *
 * Routes:
 * - POST /api/telemetry        Ingests anonymous navigator.sendBeacon / fetch read & download events.
 * - GET  /api/telemetry/stats  Returns public aggregate counters per paper (cached briefly at edge).
 * - GET  /papers/*.pdf         Passes PDF requests through to origin while counting direct PDF downloads
 *                              vs. embedded reader previews (?mode=preview), deduplicating HTTP Range chunks.
 */

const ALLOWED_PAPERS = new Set(["hub", "rings", "dranking"]);
const PUBLIC_PAPERS = ["rings", "dranking"];
const ALLOWED_EVENTS = new Set([
  "paper_hub_view",
  "paper_nav_click",
  "paper_read",
  "paper_read_progress",
  "paper_pdf_preview",
  "paper_download",
  "paper_cite_copy",
]);
const ALLOWED_FORMATS = new Set(["pdf", "md", "bib", "tex"]);
const ALLOWED_MODES = new Set(["web", "pdf"]);
const DEPTH_MILESTONES = new Set([25, 50, 75, 100]);
const MAX_BODY_BYTES = 4096;

function resolveCorsOrigin(requestOrigin) {
  if (!requestOrigin) return "https://rings.rs";
  if (
    requestOrigin === "https://rings.rs" ||
    requestOrigin === "https://www.rings.rs" ||
    /^http:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/.test(requestOrigin)
  ) {
    return requestOrigin;
  }
  return "https://rings.rs";
}

function corsHeaders(request, extra = {}) {
  const origin = resolveCorsOrigin(request.headers.get("Origin"));
  return {
    "Access-Control-Allow-Origin": origin,
    "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type",
    "Access-Control-Max-Age": "86400",
    Vary: "Origin",
    ...extra,
  };
}

function utcDayString(now = new Date()) {
  return now.toISOString().slice(0, 10);
}

function sanitizeToken(raw, maxLen = 32) {
  if (typeof raw !== "string") return "";
  return raw.replace(/[^a-zA-Z0-9_.-]/g, "").slice(0, maxLen);
}

/**
 * Map a validated client telemetry event payload to atomic counter metric keys.
 * @param {Record<string, unknown>} payload
 * @returns {{ paper: string, metrics: string[], meta: Record<string, unknown> } | null}
 */
export function classifyBeaconPayload(payload) {
  if (!payload || typeof payload !== "object") return null;
  const event = typeof payload.event === "string" ? payload.event : "";
  const paper = typeof payload.paper === "string" ? payload.paper : "";
  if (!ALLOWED_EVENTS.has(event) || !ALLOWED_PAPERS.has(paper)) {
    return null;
  }

  const format = ALLOWED_FORMATS.has(payload.format) ? payload.format : "pdf";
  const mode = ALLOWED_MODES.has(payload.mode) ? payload.mode : "web";
  const source = sanitizeToken(payload.source, 24) || "direct";
  const depth =
    typeof payload.depth === "number" && Number.isFinite(payload.depth)
      ? Math.max(0, Math.min(100, Math.round(payload.depth)))
      : null;
  const engagedSec =
    typeof payload.engaged_sec === "number" && Number.isFinite(payload.engaged_sec)
      ? Math.max(0, Math.min(86400, Math.round(payload.engaged_sec)))
      : null;

  const metrics = [];
  switch (event) {
    case "paper_hub_view":
      metrics.push("hub_views");
      break;
    case "paper_nav_click":
      metrics.push("nav_clicks", `nav_from_${source}`);
      break;
    case "paper_read":
      metrics.push("reads", mode === "pdf" ? "reads_pdf" : "reads_web");
      break;
    case "paper_read_progress":
      if (depth !== null && DEPTH_MILESTONES.has(depth) && engagedSec === null) {
        metrics.push(`depth_${depth}`);
        if (depth === 100) metrics.push("read_complete");
      }
      if (engagedSec === 30) {
        metrics.push("engaged_30s");
      } else if (engagedSec === 120) {
        metrics.push("engaged_120s");
      } else if (engagedSec !== null && engagedSec >= 5) {
        metrics.push("engaged_sessions");
      }
      break;
    case "paper_pdf_preview":
      metrics.push("pdf_previews");
      break;
    case "paper_download":
      metrics.push("downloads", `downloads_${format}`);
      break;
    case "paper_cite_copy":
      metrics.push("cite_copies");
      break;
    default:
      return null;
  }

  if (metrics.length === 0) {
    return null;
  }

  return {
    paper,
    metrics,
    meta: { event, paper, format, mode, source, depth, engaged_sec: engagedSec },
  };
}

/**
 * Determine whether an origin PDF response represents an initial transfer (not a mid-file HTTP Range chunk).
 * @param {Request} request
 * @param {Response} response
 */
export function isInitialPdfTransfer(request, response) {
  if (request.method !== "GET") return false;
  if (response.status === 200) {
    const reqRange = request.headers.get("Range");
    if (!reqRange) return true;
    return /^bytes=0-/i.test(reqRange.trim());
  }
  if (response.status === 206) {
    const contentRange = response.headers.get("Content-Range") || "";
    if (contentRange) {
      return /^bytes\s+0-/i.test(contentRange.trim());
    }
    const reqRange = request.headers.get("Range") || "";
    return /^bytes=0-/i.test(reqRange.trim());
  }
  return false;
}

async function recordMetrics(env, paper, metrics, logMeta) {
  const day = utcDayString();
  console.log(
    JSON.stringify({
      type: "rings_paper_telemetry",
      day,
      paper,
      metrics,
      ...logMeta,
    }),
  );

  const db = env?.TELEMETRY_DB;
  if (!db || typeof db.prepare !== "function") {
    return;
  }

  const nowIso = new Date().toISOString();
  const statements = [];
  for (const metric of metrics) {
    statements.push(
      db
        .prepare(
          `INSERT INTO paper_totals (paper, metric, count, updated_at)
           VALUES (?1, ?2, 1, ?3)
           ON CONFLICT (paper, metric) DO UPDATE SET
             count = count + 1,
             updated_at = excluded.updated_at`,
        )
        .bind(paper, metric, nowIso),
    );
    statements.push(
      db
        .prepare(
          `INSERT INTO paper_daily (day, paper, metric, count)
           VALUES (?1, ?2, ?3, 1)
           ON CONFLICT (day, paper, metric) DO UPDATE SET
             count = count + 1`,
        )
        .bind(day, paper, metric),
    );
  }

  if (statements.length > 0) {
    await db.batch(statements);
  }
}

async function handleBeaconPost(request, env, ctx) {
  const contentLength = Number(request.headers.get("Content-Length") || "0");
  if (contentLength > MAX_BODY_BYTES) {
    return new Response(JSON.stringify({ error: "payload_too_large" }), {
      status: 413,
      headers: corsHeaders(request, {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": "no-store",
      }),
    });
  }

  let rawText = "";
  try {
    rawText = await request.text();
  } catch {
    return new Response(JSON.stringify({ error: "invalid_body" }), {
      status: 400,
      headers: corsHeaders(request, {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": "no-store",
      }),
    });
  }

  if (!rawText || rawText.length > MAX_BODY_BYTES) {
    return new Response(JSON.stringify({ error: "invalid_body_length" }), {
      status: 400,
      headers: corsHeaders(request, {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": "no-store",
      }),
    });
  }

  let parsed;
  try {
    parsed = JSON.parse(rawText);
  } catch {
    return new Response(JSON.stringify({ error: "invalid_json" }), {
      status: 400,
      headers: corsHeaders(request, {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": "no-store",
      }),
    });
  }

  const classified = classifyBeaconPayload(parsed);
  if (!classified) {
    return new Response(JSON.stringify({ error: "unsupported_event" }), {
      status: 422,
      headers: corsHeaders(request, {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": "no-store",
      }),
    });
  }

  const writePromise = recordMetrics(env, classified.paper, classified.metrics, classified.meta).catch((err) => {
    console.error(JSON.stringify({ type: "rings_paper_telemetry_error", message: String(err) }));
  });
  if (ctx && typeof ctx.waitUntil === "function") {
    ctx.waitUntil(writePromise);
  } else {
    await writePromise;
  }

  return new Response(null, {
    status: 202,
    headers: corsHeaders(request, { "Cache-Control": "no-store" }),
  });
}

function emptyPaperStats() {
  return {
    reads: 0,
    reads_web: 0,
    reads_pdf: 0,
    downloads: 0,
    downloads_pdf: 0,
    downloads_md: 0,
    direct_pdf_downloads: 0,
    pdf_previews: 0,
    read_complete: 0,
    engaged_30s: 0,
    engaged_120s: 0,
    cite_copies: 0,
  };
}

async function handleStatsGet(request, env) {
  const papers = {
    rings: emptyPaperStats(),
    dranking: emptyPaperStats(),
  };
  let hubViews = 0;

  const db = env?.TELEMETRY_DB;
  if (db && typeof db.prepare === "function") {
    const res = await db
      .prepare("SELECT paper, metric, count FROM paper_totals WHERE paper IN ('hub', 'rings', 'dranking')")
      .all();
    const rows = res?.results || [];
    for (const row of rows) {
      if (row.paper === "hub" && row.metric === "hub_views") {
        hubViews = Number(row.count || 0);
      } else if (papers[row.paper] && Object.hasOwn(papers[row.paper], row.metric)) {
        papers[row.paper][row.metric] = Number(row.count || 0);
      }
    }
  }

  return new Response(
    JSON.stringify({
      ok: true,
      hub: { views: hubViews },
      papers,
    }),
    {
      status: 200,
      headers: corsHeaders(request, {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": "public, max-age=60",
      }),
    },
  );
}

async function handlePdfDownloadPassThrough(request, env, ctx, url, originFetch = fetch) {
  const match = url.pathname.match(/^\/papers\/([a-z0-9_-]+)\.pdf$/i);
  const slug = match ? match[1].toLowerCase() : "";
  const response = await originFetch(request);

  if (PUBLIC_PAPERS.includes(slug) && isInitialPdfTransfer(request, response)) {
    const isPreview = url.searchParams.get("mode") === "preview";
    const isBeaconTracked = url.searchParams.get("via") === "ui";
    const metrics = isPreview
      ? isBeaconTracked
        ? ["edge_pdf_previews"]
        : ["pdf_previews", "edge_pdf_previews"]
      : isBeaconTracked
        ? ["edge_pdf_transfers"]
        : ["downloads", "downloads_pdf", "direct_pdf_downloads"];

    const writePromise = recordMetrics(env, slug, metrics, {
      event: isPreview ? "edge_pdf_preview" : "edge_pdf_download",
      source: isBeaconTracked ? "ui" : "direct_url",
    }).catch((err) => {
      console.error(JSON.stringify({ type: "rings_paper_telemetry_error", message: String(err) }));
    });

    if (ctx && typeof ctx.waitUntil === "function") {
      ctx.waitUntil(writePromise);
    } else {
      await writePromise;
    }
  }

  return response;
}

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);

    if (url.pathname.startsWith("/api/telemetry")) {
      if (request.method === "OPTIONS") {
        return new Response(null, { status: 204, headers: corsHeaders(request) });
      }
      if (url.pathname === "/api/telemetry" && request.method === "POST") {
        return handleBeaconPost(request, env, ctx);
      }
      if (url.pathname === "/api/telemetry/stats" && request.method === "GET") {
        return handleStatsGet(request, env);
      }
      return new Response(JSON.stringify({ error: "not_found" }), {
        status: 404,
        headers: corsHeaders(request, { "Content-Type": "application/json; charset=utf-8" }),
      });
    }

    if (url.pathname.startsWith("/papers/") && url.pathname.endsWith(".pdf")) {
      return handlePdfDownloadPassThrough(request, env, ctx, url);
    }

    return fetch(request);
  },
  handlePdfDownloadPassThrough,
};
