# rings-tui

Interactive Terminal User Interface (TUI) for monitoring, inspecting, and controlling a `rings-node` daemon.

## Installation

Build and install from the workspace root with:

```sh
cargo install --path crates/tui
```

Or run directly against a local node configuration:

```sh
cargo run -p rings-tui -- --config ~/.rings/config.yaml
```

Preview without a running node using `--demo` (in-memory simulated daemon) or dump a non-interactive snapshot with `--print-preview`:

```sh
cargo run -p rings-tui -- --demo
cargo run -p rings-tui -- --demo --print-preview
```

## Tabs

- `1:Overview`: node readiness, delegation TTL gauge, relay mailbox counters, and native TUN gateway status.
- `2:Monitor`: live message counters (`sent`, `received`, `forwarded`, `stored`, `failed`), throughput sparklines, DHT lookup latency histogram, and bounded recent message activity feed.
- `3:Peers`: connected peers, transport states, reliability ratings, and byte credits.
- `4:DHT & Storage`: Chord ring topology and sub-views (`Finger Table`, `Persistent Storage`, `Cache Storage`).
- `5:Directory`: registry browser with sub-views (`Online Nodes`, `Onion Exits`).
- `6:Commands`: interactive command button workbench, modal forms, PubSub/Service outputs, and execution log.

## CLI Options & Environment Variables

- `-c, --config <CONFIG>` (`RINGS_CONFIG`, default `~/.rings/config.yaml`): node YAML configuration path.
- `-u, --endpoint-url <ENDPOINT_URL>` (`RINGS_ENDPOINT_URL`): override the internal API URL (defaults to `http://127.0.0.1:50000` or the port from config).
- `--api-token-path <API_TOKEN_PATH>` (`RINGS_API_TOKEN_PATH`): override the Bearer token file guarding the internal API.
- `--api-token <API_TOKEN>` (`RINGS_API_TOKEN`): pass the Bearer token explicitly (prefer `--api-token-path` to avoid shell history exposure).
- `--refresh-ms <REFRESH_MS>` (`RINGS_TUI_REFRESH_MS`, default `2000`, minimum `250`): status polling interval in milliseconds.
- `--demo` (`RINGS_TUI_DEMO`): run against an in-memory simulated node with live telemetry.
- `--print-preview`: render a non-interactive text snapshot of the TUI views and exit.

## Keybindings

- `1`–`6`, `Tab`, `BackTab`: switch between tabs.
- `r`: refresh immediately; `p`: pause/resume automatic polling; `u`: reload the API token.
- `c` / `d` / `s` / `x`: connect by URL, DID, or seed, or disconnect a peer.
- `o` / `a` / `y`: manual 3-step handshake (`createOffer`, `answerOffer`, `acceptAnswer`).
- `m` / `t` / `f`: send a backend message, publish a topic message, or fetch topic messages.
- `g` / `v` / `/` or `w` / `i` / `e`: register service, lookup service, DHT key lookup, inspect peer, or toggle expired directory entries.
- `` `[` `` / `` `]` ``, `` `{` `` / `` `}` ``, `PageUp` / `PageDown`: scroll command output and execution log.
- `?`: toggle help overlay; `q` or `Ctrl-C`: quit.
