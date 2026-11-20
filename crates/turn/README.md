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

## How Signature + ECDH Authentication Works

Standard WebRTC clients (both browser `RTCPeerConnection` and native `webrtc-rs`) authenticate to TURN servers using the RFC 5389 / RFC 5766 long-term credential mechanism. Because STUN/TURN `USERNAME` attributes are transmitted in cleartext over UDP, `rings-turn` combines a public identity signature in `USERNAME` with a **secp256k1 Diffie-Hellman (ECDH) derived password** in `credential`:

```text
  Client (Native / Browser)                                   rings-turn Server (server_sk, server_pk)
  ─────────────────────────                                   ────────────────────────────────────────
  1. Sign TURN transcript bound to server_pk & derive ECDH password:
     - Direct DID:   username   = "<0x_did>.<base58(sig)>"
                     credential = base58(keccak256("rings-turn-credential-v1\n" || ECDH(did_sk, server_pk) || sig))
     - Delegation:   username   = "<base58(delegation)>.<base58(sig)>"
                     credential = base58(keccak256("rings-turn-credential-v1\n" || ECDH(delegatee_sk, server_pk) || sig))
     - Account:      username   = "<base58(codec(AccountSessionClaim))>.<base58(sig)>"
                     credential = base58(keccak256("rings-turn-credential-v1\n" || ECDH(session_sk, server_pk) || sig))

  2. Send STUN/TURN Allocate Request ───────────────────────▶ 3. DelegationAuthHandler::auth_handle(username, realm):
     - USERNAME = username (cleartext)                           - Decode identity & sig from USERNAME
     - MESSAGE-INTEGRITY =                                       - Verify DID/Account signature OR Delegation
         HMAC-SHA1(MD5(username : realm : credential), pkt)        (bound to server_pk) & recover client secp256k1 pubkey
                                                                 - Check TurnAdmissionPolicy (AllowAny / Allowlist)
                                                                 - Compute ECDH(server_sk, client_pk) -> credential
                                                                 - Derive MD5(username : realm : credential)
                                                                 - Verify MESSAGE-INTEGRITY & allocate relay
```

Because `credential` depends on the ECDH shared secret `x(client_sk · server_pk) == x(server_sk · client_pk)`, passive on-path UDP observers who see `USERNAME` in cleartext cannot recover `credential` or impersonate the client on a new TURN allocation.

Two long-term identity signing modes and one unlinkable ephemeral ticket mode are supported via `TurnCredentials`:

- **Direct DID / Account Signature (Non-Expiring)**:
  - **Secp256k1 `SecretKey` / `Did`**:
    - `username`: `<0x_did>.<base58(did_sig)>`
    - `credential`: `base58(keccak256("rings-turn-credential-v1\n" || shared_secret || did_sig))`
    - Signed transcript: `"rings-turn-auth-v1\nsecp256k1\n<0x_did>\n<hex(server_pubkey)>"` (`pack_turn_did_auth`)
  - **Any `Account` (`EIP191`, `BIP137`, `Secp256r1`, `Ed25519`, `Bls12381`)**:
    - `username`: `<base58(codec(AccountSessionClaim))>.<base58(account_sig)>`
    - `credential`: `base58(keccak256("rings-turn-credential-v1\n" || shared_secret || account_sig))`
    - Signed transcript: `"rings-turn-auth-v1\n<algorithm>\n<did>\n<hex(session_pubkey)>\n<hex(server_pubkey)>"` (`pack_turn_account_auth`)
  - Carries no delegation TTL and **never expires** when a node's session `DelegateeKey` rotates.
- **Session `DelegateeKey` Signature (Bounded by Delegation TTL)**:
  - `username`: `<base58(codec(delegation))>.<base58(delegatee_sig)>`
  - `credential`: `base58(keccak256("rings-turn-credential-v1\n" || shared_secret || delegatee_sig))`
  - Signed transcript: `"rings-turn-auth-v1\n<hex(delegation.digest())>\n<hex(server_pubkey)>"` (`pack_turn_auth`)
  - Ideal for browser tabs and nodes that keep their root `delegator_key` offline; when a running node renews its `DelegateeKey`, `SwarmTransport` automatically refreshes its TURN credentials and updates active peer connections while `rings-turn` keeps existing established 5-tuple allocations alive across renewal.
- **Unlinkable `SessionTicket` (`tkt-` Prefix, Single-Use)**:
  - `username`: `tkt-<base58(codec(SignedTurnSessionTicket))>.<base58(ephemeral_client_sig)>`
  - `credential`: `base58(keccak256("rings-turn-credential-v1\n" || ECDH(ephemeral_client_sk, server_pk) || ephemeral_client_sig))`
  - Acquired anonymously over an onion `invoke` loop (`turn.ticket` at `invoke<64,1024,500ms>`), exposing **zero `Did` or `Delegation` bytes** in the cleartext STUN `USERNAME` attribute. Each ticket is bound to a single reservation slot (capped by `max_reservations`) and consumed at most once on `Allocate`.

### Single-Hop Threat Model

| Party | Learns | Does NOT Learn |
| --- | --- | --- |
| TURN server (`SessionTicket` mode) | Client IP, remote peer IP (`XOR-PEER-ADDRESS`), relayed bytes (DTLS/SRTP ciphertext) | Client `Did`, client `Delegation`, client overlay address during `turn.ticket` issuance |
| TURN server (`Did` / `Delegation` mode) | Client IP, remote peer IP, client `Did` | Root private key or TURN `credential` from passive capture |
| Remote WebRTC peer (relay-only policy) | TURN server public relay address (`XOR-RELAYED-ADDRESS`) | Client IP (when ICE uses relay candidates only) |
| Passive UDP observer | Cleartext STUN `USERNAME` (`tkt-…` or identity claim) and packet sizes/timing | TURN `credential` password; client `Did` in `SessionTicket` mode |
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
      --server-key <SERVER_KEY>
          Hex-encoded 32-byte secp256k1 secret key for TURN ECDH credential derivation.
          When omitted, an ephemeral key is generated on startup and its public key is logged
          [env: RINGS_TURN_SERVER_KEY=]
      --allow-did <ALLOW_DIDS>
          Optional comma-separated allowlist of DIDs allowed to allocate TURN relays.
          When omitted, any cryptographically valid Rings signature is admitted
          [env: RINGS_TURN_ALLOW_DIDS=]
      --allow-private-peers
          Allow relay sockets to exchange UDP packets with loopback, link-local, or private
          peer addresses (disabled by default to prevent SSRF / local-network amplification)
          [env: RINGS_TURN_ALLOW_PRIVATE_PEERS=]
      --max-nonces <MAX_NONCES>
          Maximum number of active nonce challenges tracked concurrently
          [env: RINGS_TURN_MAX_NONCES=] [default: 4096]
      --max-allocations <MAX_ALLOCATIONS>
          Maximum number of active TURN relay allocations stored concurrently
          [env: RINGS_TURN_MAX_ALLOCATIONS=] [default: 1024]
      --max-reservations <MAX_RESERVATIONS>
          Maximum number of unclaimed session-ticket reservations stored concurrently (0 disables)
          [env: RINGS_TURN_MAX_RESERVATIONS=] [default: 256]
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
     --max-port 65535 \
     --server-key 0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318
   ```
   On startup, `rings-turn` prints its compressed secp256k1 public key (`pubkey=<base58(server_pubkey)>`) and ready-to-use `turn://...?pubkey=<base58(server_pubkey)>` URL.

2. **Private TURN server restricted to specific DIDs**:
   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --server-key 0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318 \
     --allow-did 0x1111111111111111111111111111111111111111,0x2222222222222222222222222222222222222222
   ```

## Configuring Rings Nodes to Use `rings-turn`

In a Rings node's `config.yaml` (or `--ice-servers` CLI flag), add the `turn://` URL with its `?pubkey=<base58(server_pubkey)>` query parameter (and no username or password):

```yaml
ice_servers: stun://stun.l.google.com:19302;turn://203.0.113.10:3478?pubkey=249495738f7s7d8f7s8d7f8s7d8f7s8d7f8s7d8f7s8d
```

- If `delegator_key` is configured in `config.yaml`, the node automatically signs the `turn://...?pubkey=<server_pubkey>` URL using a **non-expiring direct DID signature + ECDH credential** and strips `?pubkey=...` before passing the standard `turn://user:pass@host:port` URI to WebRTC.
- Otherwise, `SwarmTransport` automatically signs the `turn://...?pubkey=<server_pubkey>` URL using the node's `DelegateeKey` and refreshes it whenever `DelegateeKey` is renewed in process.
- Explicit credentials (`turn://user:pass@other-turn.example.org:3478`) are preserved as-is for compatibility with third-party TURN servers.
