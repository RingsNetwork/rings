# `rings-paper-telemetry` Worker

First-party, cookie-free reading and download telemetry service for [https://rings.rs/papers/](https://rings.rs/papers/).

## Architecture & Privacy Posture

- **Zero PII / Zero Cookies**: Never sets `Set-Cookie`, never stores IP addresses, user-agent fingerprints, or session identifiers. Only atomic per-paper and per-UTC-day integer counters are persisted in Cloudflare D1 (`paper_totals` and `paper_daily`), alongside structured Workers Observability logs.
- **Dual-Layer Instrumentation**:
  1. **Client Beacon (`POST /api/telemetry`)**: Fired via `navigator.sendBeacon` (`credentials: "omit"`) from `/papers/papers-telemetry.js` (and site-wide paper links) for:
     - `paper_hub_view`: Visits to `/papers/`
     - `paper_read`: Opening `/papers/rings/` or `/papers/dranking/` (`reads_web` vs. `reads_pdf`)
     - `paper_read_progress`: Scroll depth milestones (`25%`, `50%`, `75%`, `100%` / `read_complete`) and engaged foreground reading heartbeats (`30s`, `120s`)
     - `paper_pdf_preview`: Switching to the in-page Embedded PDF viewer
     - `paper_download`: Clicking Download PDF or Markdown (`?via=ui` prevents double-counting at the edge)
     - `paper_cite_copy`: Copying BibTeX citations
  2. **Edge PDF Interception (`GET /papers/*.pdf`)**: Intercepts direct PDF fetches (from external links, `curl`, reference managers, or shared URLs) without buffering the PDF body, deduplicates mid-file HTTP `206 Partial Content` Range chunks (`bytes 0-...` only), and separates `?mode=preview` from `direct_pdf_downloads`.
- **Public Counter Endpoint (`GET /api/telemetry/stats`)**: Hydrates the optional read/download counter pill in the `/papers/` UI with `Cache-Control: public, max-age=60`.

## Local Verification

```sh
node workers/paper-telemetry/test-worker.mjs
```

## Deployment (`wrangler`)

```sh
cd workers/paper-telemetry
npx wrangler d1 create rings-paper-telemetry
# Copy the returned database_id into wrangler.jsonc -> d1_databases[0].database_id
npx wrangler d1 execute rings-paper-telemetry --remote --file=./schema.sql
npx wrangler deploy
```
