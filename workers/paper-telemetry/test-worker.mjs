#!/usr/bin/env node
/**
 * Unit & integration test suite for `workers/paper-telemetry/src/index.mjs`.
 * Exercises beacon ingestion, milestone classification, D1 atomic counters, CORS,
 * and server-side PDF download vs. preview / HTTP Range chunk deduplication.
 */

import assert from "node:assert/strict";
import worker, { classifyBeaconPayload, isInitialPdfTransfer } from "./src/index.mjs";

const watchdog = setTimeout(() => {
  console.error("TIMEOUT: test-worker.mjs exceeded 15s watchdog");
  process.exit(1);
}, 15_000);
watchdog.unref();

class MockD1Database {
  constructor() {
    /** @type {Map<string, number>} */
    this.totals = new Map();
    /** @type {Map<string, number>} */
    this.daily = new Map();
  }

  prepare(sql) {
    const db = this;
    return {
      sql,
      args: [],
      bind(...args) {
        this.args = args;
        return this;
      },
      async run() {
        if (this.sql.includes("INSERT INTO paper_totals")) {
          const [paper, metric] = this.args;
          const k = `${paper}:${metric}`;
          db.totals.set(k, (db.totals.get(k) || 0) + 1);
        } else if (this.sql.includes("INSERT INTO paper_daily")) {
          const [day, paper, metric] = this.args;
          const k = `${day}:${paper}:${metric}`;
          db.daily.set(k, (db.daily.get(k) || 0) + 1);
        }
        return { success: true };
      },
      async all() {
        const results = [];
        for (const [key, count] of db.totals.entries()) {
          const [paper, metric] = key.split(":");
          results.push({ paper, metric, count });
        }
        return { results };
      },
    };
  }

  async batch(statements) {
    for (const stmt of statements) {
      await stmt.run();
    }
    return statements.map(() => ({ success: true }));
  }
}

function createCtx() {
  const pending = [];
  return {
    waitUntil(promise) {
      pending.push(promise);
    },
    async flush() {
      await Promise.all(pending.splice(0));
    },
  };
}

async function runTests() {
  const db = new MockD1Database();
  const env = { TELEMETRY_DB: db };
  const ctx = createCtx();

  // 1. CORS preflight
  {
    const res = await worker.fetch(
      new Request("https://rings.rs/api/telemetry", {
        method: "OPTIONS",
        headers: { Origin: "https://rings.rs" },
      }),
      env,
      ctx,
    );
    assert.equal(res.status, 204);
    assert.equal(res.headers.get("Access-Control-Allow-Origin"), "https://rings.rs");
  }

  // 2. Hub view + paper read + scroll milestones + engaged heartbeats + cite + UI download
  const events = [
    { event: "paper_hub_view", paper: "hub" },
    { event: "paper_read", paper: "rings", mode: "web" },
    { event: "paper_read", paper: "rings", mode: "pdf" },
    { event: "paper_read_progress", paper: "rings", depth: 25 },
    { event: "paper_read_progress", paper: "rings", depth: 100 },
    { event: "paper_read_progress", paper: "rings", depth: 100, engaged_sec: 30 },
    { event: "paper_read_progress", paper: "rings", depth: 100, engaged_sec: 120 },
    { event: "paper_pdf_preview", paper: "rings", mode: "pdf" },
    { event: "paper_download", paper: "rings", format: "pdf", source: "toolbar" },
    { event: "paper_download", paper: "dranking", format: "md", source: "hub" },
    { event: "paper_cite_copy", paper: "rings", source: "cite-button" },
  ];

  for (const payload of events) {
    const res = await worker.fetch(
      new Request("https://rings.rs/api/telemetry", {
        method: "POST",
        headers: { "Content-Type": "text/plain;charset=UTF-8", Origin: "https://rings.rs" },
        body: JSON.stringify(payload),
      }),
      env,
      ctx,
    );
    assert.equal(res.status, 202);
  }
  await ctx.flush();

  // 3. Reject invalid event / invalid JSON
  {
    const badJson = await worker.fetch(
      new Request("https://rings.rs/api/telemetry", { method: "POST", body: "{not-json" }),
      env,
      ctx,
    );
    assert.equal(badJson.status, 400);

    const badEvent = await worker.fetch(
      new Request("https://rings.rs/api/telemetry", {
        method: "POST",
        body: JSON.stringify({ event: "unknown_event", paper: "rings" }),
      }),
      env,
      ctx,
    );
    assert.equal(badEvent.status, 422);
  }

  // 4. Server-side PDF pass-through:
  //    a) Direct external PDF download (200 OK) -> increments downloads + direct_pdf_downloads
  //    b) Range chunk 0-65535 (206 Partial Content) -> increments downloads + direct_pdf_downloads
  //    c) Subsequent Range chunk 65536-131071 (206 Partial Content) -> ignored (not double counted!)
  //    d) UI-tracked download (?via=ui) -> records edge_pdf_transfers without double-counting downloads
  //    e) Direct PDF preview (?mode=preview) -> increments pdf_previews without incrementing downloads
  {
    const fakePdf = () => new Response(new Uint8Array([0x25, 0x50, 0x44, 0x46]), { status: 200 });

    await worker.handlePdfDownloadPassThrough(
      new Request("https://rings.rs/papers/dranking.pdf"),
      env,
      ctx,
      new URL("https://rings.rs/papers/dranking.pdf"),
      async () => fakePdf(),
    );

    await worker.handlePdfDownloadPassThrough(
      new Request("https://rings.rs/papers/rings.pdf", { headers: { Range: "bytes=0-65535" } }),
      env,
      ctx,
      new URL("https://rings.rs/papers/rings.pdf"),
      async () =>
        new Response(new Uint8Array([1, 2, 3]), {
          status: 206,
          headers: { "Content-Range": "bytes 0-65535/654321" },
        }),
    );

    await worker.handlePdfDownloadPassThrough(
      new Request("https://rings.rs/papers/rings.pdf", { headers: { Range: "bytes=65536-131071" } }),
      env,
      ctx,
      new URL("https://rings.rs/papers/rings.pdf"),
      async () =>
        new Response(new Uint8Array([4, 5, 6]), {
          status: 206,
          headers: { "Content-Range": "bytes 65536-131071/654321" },
        }),
    );

    await worker.handlePdfDownloadPassThrough(
      new Request("https://rings.rs/papers/rings.pdf?via=ui"),
      env,
      ctx,
      new URL("https://rings.rs/papers/rings.pdf?via=ui"),
      async () => fakePdf(),
    );

    await worker.handlePdfDownloadPassThrough(
      new Request("https://rings.rs/papers/dranking.pdf?mode=preview"),
      env,
      ctx,
      new URL("https://rings.rs/papers/dranking.pdf?mode=preview"),
      async () => fakePdf(),
    );

    await ctx.flush();
  }

  // 5. Verify public stats aggregation
  {
    const statsRes = await worker.fetch(
      new Request("https://rings.rs/api/telemetry/stats", { headers: { Origin: "https://rings.rs" } }),
      env,
      ctx,
    );
    assert.equal(statsRes.status, 200);
    const data = await statsRes.json();
    assert.equal(data.ok, true);
    assert.equal(data.hub.views, 1);

    assert.equal(data.papers.rings.reads, 2);
    assert.equal(data.papers.rings.reads_web, 1);
    assert.equal(data.papers.rings.reads_pdf, 1);
    assert.equal(data.papers.rings.read_complete, 1);
    assert.equal(data.papers.rings.engaged_30s, 1);
    assert.equal(data.papers.rings.engaged_120s, 1);
    assert.equal(data.papers.rings.pdf_previews, 1);
    assert.equal(data.papers.rings.cite_copies, 1);
    // 1 UI PDF download + 1 initial byte-0 direct PDF download (chunk 2 & ?via=ui not double-counted!)
    assert.equal(data.papers.rings.downloads, 2);
    assert.equal(data.papers.rings.direct_pdf_downloads, 1);

    // 1 UI Markdown download + 1 direct PDF download (?mode=preview counted under pdf_previews!)
    assert.equal(data.papers.dranking.downloads, 2);
    assert.equal(data.papers.dranking.downloads_md, 1);
    assert.equal(data.papers.dranking.downloads_pdf, 1);
    assert.equal(data.papers.dranking.direct_pdf_downloads, 1);
    assert.equal(data.papers.dranking.pdf_previews, 1);
  }

  // 6. Pure helper edge checks
  assert.equal(classifyBeaconPayload(null), null);
  assert.equal(
    isInitialPdfTransfer(
      new Request("https://rings.rs/papers/rings.pdf", { method: "HEAD" }),
      new Response(null, { status: 200 }),
    ),
    false,
  );

  console.log("PASS: workers/paper-telemetry/test-worker.mjs verified all telemetry flows.");
}

await runTests();
clearTimeout(watchdog);
