# rings-turn

Stateless RFC 5766 TURN server for Rings Network using **"signature as password"** authentication instead of traditional username/password databases or shared HMAC secrets.

<!-- ANCHOR: model -->
## When to Use `rings-turn`

WebRTC peers normally establish direct UDP datachannels via STUN hole-punching (`typ host` / `typ srflx`). You only need `rings-turn` when direct hole-punching cannot succeed or when you want zero-secret relay access control:

1. **Symmetric NAT & Carrier-Grade NAT (CGNAT)**:
   When both peers are behind Symmetric NATs (common on 4G/5G mobile networks, residential CGNAT, or strict cloud NAT gateways) where external UDP port mappings change per destination, STUN cannot punch a direct path and a TURN relay (`typ relay`) is required.
2. **Strict Corporate / Enterprise Firewalls**:
   Environments that block arbitrary peer-to-peer UDP traffic and only allow UDP packets to known infrastructure IPs/ports (such as port `3478` and a bounded relay port range `--min-port` / `--max-port`).
3. **Zero-Database, Zero-Shared-Secret Operations**:
   Traditional TURN deployments (`coturn`, etc.) require maintaining either a static `username:password` table or a shared REST HMAC secret (`use-auth-secret`) synchronized across auth servers and TURN instances. `rings-turn` requires **no database, no user registration, and no shared secret**—any node holding a valid Rings `DelegateeKey` or `Did` private key self-signs its own TURN credentials locally, and any `rings-turn` instance verifies them statelessly.
4. **Private / Permissioned Overlay Relays**:
   Operators running private or consortium Rings overlays can pass `--allow-did <did1,did2,...>` (`RINGS_TURN_ALLOW_DIDS`) so only authorized DIDs (or session delegations issued by those DIDs) may allocate TURN relay sockets.

## How Signature-as-Password Works

Standard WebRTC clients (both browser `RTCPeerConnection` and native `webrtc-rs`) authenticate to TURN servers using the RFC 5389 / RFC 5766 long-term credential mechanism:

```text
  Client (Native / Browser)                                   rings-turn Server
  ─────────────────────────                                   ─────────────────
  1. Sign TURN transcript with DelegateeKey or DID key:
     - Direct DID:   username   = "<0x_did>.<base58(sig)>"
                     credential = "<base58(sig)>"
     - Delegation:   username   = "<base58(delegation)>.<base58(sig)>"
                     credential = "<base58(sig)>"
     - Account:      username   = "<base58(account)>.<base58(sig)>"
                     credential = "<base58(sig)>"

  2. Send STUN/TURN Allocate Request ───────────────────────▶ 3. DelegationAuthHandler::auth_handle(username, realm):
     - USERNAME = username (cleartext)                           - Decode identity & sig from USERNAME
     - MESSAGE-INTEGRITY =                                       - Verify DID/Account signature OR Delegation
         HMAC-SHA1(MD5(username : realm : credential), pkt)        (delegator sig + TTL window + delegatee sig)
                                                                 - Check TurnAdmissionPolicy (AllowAny / Allowlist)
                                                                 - Derive MD5(username : realm : sig_b58)
                                                                 - Verify MESSAGE-INTEGRITY & allocate relay
```

Two signing modes are supported via `TurnCredentials`:

- **Direct DID / Account Signature (Non-Expiring)**:
  - **Secp256k1 `SecretKey` / `Did`**:
    - `username`: `<0x_did>.<base58(did_sig)>`
    - `credential`: `<base58(did_sig)>`
    - Signed transcript: `"rings-turn-auth-v1\nsecp256k1\n<0x_did>"` (`pack_turn_did_auth`)
  - **Any `Account` (`EIP191`, `BIP137`, `Secp256r1`, `Ed25519`, `Bls12381`)**:
    - `username`: `<base58(codec(account))>.<base58(account_sig)>`
    - `credential`: `<base58(account_sig)>`
    - Signed transcript: `"rings-turn-auth-v1\n<algorithm>\n<did>"` (`pack_turn_account_auth`)
  - Carries no delegation TTL and **never expires** when a node's session `DelegateeKey` rotates.
- **Session `DelegateeKey` Signature (Bounded by Delegation TTL)**:
  - `username`: `<base58(codec(delegation))>.<base58(delegatee_sig)>`
  - `credential`: `<base58(delegatee_sig)>`
  - Signed transcript: `"rings-turn-auth-v1\n<hex(delegation.digest())>"` (`pack_turn_auth`)
  - Ideal for browser tabs and nodes that keep their root `delegator_key` offline; when a running node renews its `DelegateeKey`, `SwarmTransport` automatically refreshes its TURN credentials.
<!-- ANCHOR_END: model -->

## Standalone Binary (`rings-turn`)

`rings-turn` ships as a standalone CLI binary in `crates/turn/bin/rings-turn.rs`.

### Build & Install

```bash
# Build from the workspace
cargo build --release -p rings-turn --bin rings-turn

# Or install directly
cargo install --path crates/turn
```

### CLI Options & Environment Variables

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

### Example Commands

1. **Public TURN server on a cloud VM (with 1:1 public IP `203.0.113.10` and firewall port range `49152..=65535`)**:
   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --relay-bind-ip 0.0.0.0 \
     --min-port 49152 \
     --max-port 65535
   ```

2. **Private TURN server restricted to specific DIDs**:
   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --allow-did 0x1111111111111111111111111111111111111111,0x2222222222222222222222222222222222222222
   ```

## Configuring Rings Nodes to Use `rings-turn`

In a Rings node's `config.yaml` (or `--ice-servers` CLI flag), add a bare `turn://` URL without username or password:

```yaml
ice_servers: stun://stun.l.google.com:19302;turn://203.0.113.10:3478
```

- If `delegator_key` is configured in `config.yaml`, the node automatically signs the bare `turn://` URL using a **non-expiring direct DID signature** (`<0x_did>.<sig>:<sig>@`).
- Otherwise, `SwarmTransport` automatically signs the bare `turn://` URL using the node's `DelegateeKey` (`<delegation>.<sig>:<sig>@`) and refreshes it whenever `DelegateeKey` is renewed in process.
- Explicit credentials (`turn://user:pass@other-turn.example.org:3478`) are preserved as-is for compatibility with third-party TURN servers.
