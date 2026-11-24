# rings-network-policy

Shared fail-closed admission predicates for public-network egress targets in Rings.

`rings-network-policy` is used by native exit, gateway, TURN, and webview crates to reject non-public IP literals and local hostnames before opening outbound connections:

- `is_public_ip(address)` rejects private, loopback, link-local, carrier-grade NAT, documentation, benchmarking, multicast, reserved, transition (`6to4`, Teredo, ORCHID), and IPv4-embedded IPv6 non-public addresses, failing closed outside public IPv6 unicast (`2000::/3`).
- `validate_public_url_host(url)` checks URL IP literals with `is_public_ip` and rejects missing hosts, single-label names, and local domain suffixes (`localhost`, `local`, `internal`, `lan`, `home`, `home.arpa`), returning `PublicHostError`.
- Native transports remain responsible for resolving admitted domains once, filtering every candidate through `is_public_ip`, and connecting directly to the pinned snapshot to prevent DNS rebinding.
