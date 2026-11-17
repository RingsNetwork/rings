# rings-tui

Interactive Terminal User Interface (TUI) for monitoring, inspecting, and controlling a `rings-node` daemon.

## Features

- **Real-time Monitoring (`Monitor`)**: Live message counters (`sent`, `received`, `forwarded`, `stored`, `failed`), throughput sparklines, DHT lookup latency histogram, and bounded recent message activity feed.
- **Node & Gateway Status (`Overview`, `DHT & Storage`, `Directory`)**: Process & overlay readiness propositions, session delegation TTL gauge, relay mailbox counters, TUN gateway status, Chord DHT ring topology (predecessor, successors, finger table), storage inspector, and online-node / onion-exit registries.
- **Interactive Button Commands (`Peers`, `Commands`, Toolbar)**: Mouse-clickable and keyboard-driven command buttons and modal forms for connecting peers (HTTP URL, DID, Seed), disconnecting peers, 3-step manual WebRTC handshakes (`createOffer`, `answerOffer`, `acceptAnswer`), sending namespaced backend messages, publishing/fetching PubSub topic messages, and registering/looking up DHT services.

## Usage

```bash
cargo run -p rings-tui -- --config ~/.rings/config.yaml
```
