-- First-party cookie-free aggregate telemetry counters for https://rings.rs/papers/
-- Stores only aggregate per-paper / per-day counters (zero IPs, zero cookies, zero user identifiers).

CREATE TABLE IF NOT EXISTS paper_totals (
  paper      TEXT    NOT NULL,
  metric     TEXT    NOT NULL,
  count      INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  PRIMARY KEY (paper, metric)
);

CREATE TABLE IF NOT EXISTS paper_daily (
  day    TEXT    NOT NULL,
  paper  TEXT    NOT NULL,
  metric TEXT    NOT NULL,
  count  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (day, paper, metric)
);

CREATE INDEX IF NOT EXISTS idx_paper_daily_paper_day
  ON paper_daily (paper, day DESC);
