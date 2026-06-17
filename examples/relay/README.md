# Rings TCP/UDP relay example

Turns a Rings overlay link into a transparent byte pipe. A **server** node exposes a
local socket under a service name; a **client** node binds a local listener that
forwards every connection/flow to that service across the overlay. TCP and UDP share
the same pure relay protocol — only the `TransportKind` differs.

```text
  socket ─▶ client tunnel ─▶ overlay ─▶ server `echo` service ─▶ local echo server
         ◀───────────────────── echoed bytes ─────────────────────────┘
```

The core flow lives in `src/lib.rs` and is shared verbatim by the runnable demos and
the integration tests, so they exercise the same code.

## Run

```sh
cargo run -p rings-relay-example --example rings-tcp-relay-example
cargo run -p rings-relay-example --example rings-udp-relay-example
```

Each demo spins up two in-process nodes, links them, and prints the bytes the relay
echoed back.

## Test

```sh
cargo test -p rings-relay-example
```

The integration tests run the full round-trip when an overlay link can be established
(WebRTC/ICE). In environments without UDP/STUN connectivity the handshake cannot
complete, so the tests **skip** (printing a notice) instead of failing spuriously.
