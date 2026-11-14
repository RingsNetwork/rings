# TURN Server (`rings-turn`)

`rings-turn` is a standalone, stateless RFC 5766 TURN server for Rings Network that authenticates WebRTC relay allocations using Rings cryptographic signatures (**"signature as password"**) instead of a username/password database or shared HMAC secret.

{{#include ../../crates/turn/README.md:model}}

## Running `rings-turn`

Build or install the `rings-turn` binary from the workspace:

```bash
cargo build --release -p rings-turn --bin rings-turn
# or
cargo install --path crates/turn
```

### Command-line interface

```text
Usage: rings-turn [OPTIONS]

Options:
      --listen <LISTEN>
          UDP socket address to listen on for STUN/TURN requests
          [env: RINGS_TURN_LISTEN=] [default: 0.0.0.0:3478]
      --realm <REALM>
          TURN authentication realm advertised in 401/438 challenges
          [env: RINGS_TURN_REALM=] [default: rings.network]
      --public-ip <PUBLIC_IP>
          Public IPv4/IPv6 address advertised in XOR-RELAYED-ADDRESS.
          Defaults to the listener IP (or 127.0.0.1 when listening on 0.0.0.0)
          [env: RINGS_TURN_PUBLIC_IP=]
      --relay-bind-ip <RELAY_BIND_IP>
          Local IPv4/IPv6 address bound when allocating relay UDP sockets.
          Defaults to the listener IP
          [env: RINGS_TURN_RELAY_BIND_IP=]
      --min-port <MIN_PORT>
          Inclusive lower bound for allocated relay UDP ports
          [env: RINGS_TURN_MIN_PORT=]
      --max-port <MAX_PORT>
          Inclusive upper bound for allocated relay UDP ports
          [env: RINGS_TURN_MAX_PORT=]
      --channel-bind-timeout-secs <CHANNEL_BIND_TIMEOUT_SECS>
          ChannelBind timeout in seconds (0 uses the RFC 5766 default of 600s)
          [env: RINGS_TURN_CHANNEL_BIND_TIMEOUT_SECS=] [default: 0]
      --allow-did <ALLOW_DIDS>
          Optional comma-separated allowlist of DIDs allowed to allocate TURN relays.
          When omitted, any cryptographically valid Rings signature is admitted
          [env: RINGS_TURN_ALLOW_DIDS=]
  -h, --help
          Print help
  -V, --version
          Print version
```

### Deployment examples

1. **Public relay with bounded UDP port range** (open UDP `3478` and `49152-65535` in your firewall):

   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --relay-bind-ip 0.0.0.0 \
     --min-port 49152 \
     --max-port 65535
   ```

2. **Private relay restricted to an allowlist of DIDs**:

   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --allow-did 0x1111111111111111111111111111111111111111,0x2222222222222222222222222222222222222222
   ```

## Configuring nodes to use `rings-turn`

In a node's [`config.yaml`](advanced-topic/config.yaml.md) (or via `rings run --ice-servers`), list the `turn://` server without credentials:

```yaml
ice_servers: stun://stun.l.google.com:19302;turn://203.0.113.10:3478
```

- When `delegator_key` is configured in `config.yaml`, the node signs bare `turn://` entries directly with its DID key (`<0x_did>.<sig>:<sig>@`), which **never expires**.
- When only `delegatee_key` is configured, the node signs bare `turn://` entries with its `DelegateeKey` (`<delegation>.<sig>:<sig>@`) and refreshes them automatically whenever `DelegateeKey` is renewed.
- Explicit `turn://user:pass@host:port` entries are left untouched for third-party TURN servers.
