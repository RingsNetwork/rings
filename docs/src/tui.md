# Interactive Terminal UI (`rings-tui`)

`rings-tui` is the interactive terminal workbench for operating and inspecting a running Rings
node over its authenticated internal JSON-RPC and observability endpoints. It runs on Linux,
macOS, and Windows terminals using Ratatui and Crossterm, and includes an offline `--demo` mode
for exploring the interface without a live daemon.

## Installation

Install from crates.io or build from a local checkout using the workspace's pinned toolchain:

```sh
cargo +1.97.0 install --locked rings-tui
```

From the repository root:

```sh
cargo run -p rings-tui --bin rings-tui -- --help
```

## Launching and authentication

By default `rings-tui` reads `~/.rings/config.yaml` (`-c, --config`), determines the node's
internal endpoint (`endpoint_url`, defaulting to `http://127.0.0.1:50000`), and reads the Bearer
token from `api_token_path` (defaulting to `api-token` next to the config file).

```sh
rings-tui
rings-tui --config /etc/rings/config.yaml
rings-tui --demo
rings-tui --print-preview
```

> Avoid `--api-token <SECRET>` in shared shells or scripts because command arguments appear in
> process listings. Prefer `--api-token-path` or `RINGS_API_TOKEN_PATH`, which also lets you
> hot-reload a rotated token from disk inside the TUI using `u`.

### Command-line options

| Flag | Environment variable | Default | Purpose |
|---|---|---|---|
| `-c, --config <PATH>` | `RINGS_CONFIG` | `~/.rings/config.yaml` | Node YAML configuration file used to resolve endpoint and token paths |
| `-u, --endpoint-url <URL>` | `RINGS_ENDPOINT_URL` | Config `endpoint_url` or `http://127.0.0.1:50000` | Override internal JSON-RPC and HTTP status base URL |
| `--api-token-path <PATH>` | `RINGS_API_TOKEN_PATH` | Config `api_token_path` or `api-token` beside config | Bearer token file for internal API authentication |
| `--api-token <SECRET>` | `RINGS_API_TOKEN` | — | Explicit Bearer token string (disables disk reload via `u`) |
| `--refresh-ms <MS>` | `RINGS_TUI_REFRESH_MS` | `2000` | Auto-poll interval in milliseconds (clamped at minimum `250` ms) |
| `--demo` | `RINGS_TUI_DEMO` | `false` | Run against an in-memory simulated live node |
| `--print-preview` | — | `false` | Render all six tabs non-interactively to stdout and exit |

## Views (Tabs `1`–`6`)

| Key | Tab | Contents |
|---|---|---|
| `1` | **Overview** | Process and overlay readiness, node DID and wire version, session-key remaining TTL and rotation counts, native gateway status (`/gateway/status`), and summary counters. |
| `2` | **Monitor** | Live traffic sparklines (sent, received, forwarded, stored, failed), DHT lookup latency histogram from `/status?view=observability`, and recent privacy-safe message events. |
| `3` | **Peers** | Connected transport peers, connection state, local advisory reliability (`healthy`, `unknown`, `degraded`), and aMule-style byte credit scores (`[1.0, 10.0]`). Press `Enter` on a row to inspect full measurements (`peerMeasurement`). |
| `4` | **DHT & Storage** | Press `Space` to cycle through three sub-views: **Finger Table** (160-bit ring intervals and endpoints), **Persistent Storage**, and **Cache Storage**. Press `Enter` or `/` to query a key. |
| `5` | **Directory** | Press `Space` to switch between **Online Nodes Registry** (`lookupOnlineNodes`) and **Onion Exits Registry** (`lookupOnionExits`); press `e` to toggle expired descriptors and `Enter` to dial the selected DID. |
| `6` | **Commands** | Interactive 4×4 command button workbench, scrollable Command Output inspector, and chronological Command Execution Log. |

## Keybindings

### Navigation & inspection

| Key | Action |
|---|---|
| `1`–`6` | Jump directly to tab 1–6 |
| `Tab` / `Shift+Tab` | Next / previous tab |
| `Arrow keys` or `h` `j` `k` `l` | Move selection across table rows or command buttons |
| `b` / `B` | Toggle focus on the top action toolbar |
| `Space` | Cycle sub-tabs on **DHT & Storage** / **Directory**, or invoke focused button |
| `Enter` | Context action: inspect peer (`Peers`), DHT key lookup (`DHT & Storage`), connect DID (`Directory`), or invoke focused button |
| `[` / `]` | Scroll the Command Output inspector up / down |
| `{` / `}` | Step backward / forward through Command Execution Log history |
| `PageUp` / `PageDown` | Page scroll active list or output pane |
| `?` | Toggle full keybinding help overlay |
| `Esc` | Close active modal/help overlay or clear toolbar focus |
| `q` / `Q` / `Ctrl+C` | Quit `rings-tui` |

### Direct command hotkeys

| Key | Operation | RPC / endpoint called |
|---|---|---|
| `r` | Refresh all snapshots immediately | `nodeDid`, `nodeInfo`, `listPeers`, `/status?view=observability`, `/gateway/status` |
| `p` | Toggle background auto-polling | — |
| `c` | Connect to a peer HTTP endpoint | `connectPeerViaHttp` |
| `d` | Connect to a DID over the DHT | `connectWithDid` |
| `s` | Connect using a JSON seed list | `connectWithSeed` |
| `x` | Disconnect from a peer DID | `disconnect` |
| `o` | Create manual SDP offer | `createOffer` |
| `a` | Answer manual SDP offer | `answerOffer` |
| `y` | Accept manual SDP answer | `acceptAnswer` |
| `m` | Send namespaced backend message | `sendBackendMessage` |
| `t` | Publish message to a DHT data topic | `publishMessageToTopic` |
| `f` | Fetch elements from a DHT data topic | `fetchTopicMessages` |
| `g` | Register node under a service name | `registerService` |
| `v` | Look up providers for a service name | `lookupService` |
| `/` or `w` | Look up a DID or key in local DHT storage/routing | `nodeInfo` / local DHT report |
| `i` | Inspect full peer counters and credit record | `peerMeasurement` |
| `e` | Toggle expired entries in Directory registries | `lookupOnlineNodes`, `lookupOnionExits` |
| `u` | Reload Bearer token from `--api-token-path` | Reads token file on disk |

Inside modal input editors, `Tab`/`Shift+Tab`/`Up`/`Down` move across fields,
`Left`/`Right`/`Home`/`End`/`Ctrl+A`/`Ctrl+E` move the cursor, `Ctrl+U` clears the active field,
bracketed paste inserts offer/answer payloads cleanly, `Enter` submits, and `Esc` cancels.

## Related chapters

- [Operate a Node from the CLI](cli.md) — non-interactive scripting subcommands
- [Status observability](operator-observability.md) — JSON schema behind the Monitor and Overview tabs
- [API](jsonrpc.md) — underlying JSON-RPC 2.0 methods
