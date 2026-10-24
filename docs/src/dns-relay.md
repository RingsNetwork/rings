# DNS Relay

A DNS relay names every node of a Rings network on the internet and relays its services there.
An operator delegates a DNS zone to a relay node, for example `rings.rs` or a user's
`p2p.example.com`. Every node then has the name `<did>.<zone>`, and the services it announces
are reachable at that name by ordinary internet software: a mail server delivering to
`me@<did>.<zone>`, an SSH client, a browser (#1049).

```text
  mail server ──DNS──▶ relay: <did>.<zone>  AAAA addr(did), MX <did>.<zone>
  mail server ──TCP──▶ relay: (addr(did), 25) ──TCP relay over Rings──▶ node did ──▶ 127.0.0.1:2525
```

Every DID gets its own IPv6 address in the relay's prefix. A protocol that names no host before
its first byte reaches the server, as SMTP and SSH do not, is therefore routed by destination
address alone: the relay parses nothing it carries, and TLS, where a service uses it, ends on
the node.

## The model

{{#include ../../crates/dns-relay/README.md:model}}

## Running a relay

A relay is a native node with a `dns_relay` section in its `config.yaml`:

```yaml
dns_relay:
  zones:
    - origin: rings.rs
      nameservers: [ns1.example.net]
      contact: hostmaster.example.net
  prefix: 2001:db8:1::/64
  ports: [22, 25, 80, 443]
  state_dir: ~/.rings/dns-relay
```

* `zones`: the zones delegated to the relay. A nameserver lies outside every relayed zone, so
  the relay never needs glue for it. Zones are pairwise disjoint.
* `prefix`: the IPv6 prefix routed to this host; every owner gets an address in it. A /64 is
  plenty.
* `ports`: the ports the ingress accepts on, for every address in the prefix.
* `state_dir`: where the relay keeps its address key, its address book and its memory of
  announcements. Keep it: a new key moves every owner to a new address.
* `dns_listen` (default `[::]:53` and `0.0.0.0:53`), `ingress_listen` (default `::`),
  `ttl_secs` and `negative_ttl_secs` (default 60), `book_retention_secs` (default eight days),
  `grace_secs` (default seven days), `max_connections` (default 4096) and
  `max_connections_per_owner` (default 64).

On the host:

1. Make the prefix local, so the wildcard listener receives every address in it:
   `ip -6 route add local 2001:db8:1::/64 dev lo`. The provider must route the prefix to the host.
2. Delegate the zone at its parent: `rings.rs. NS ns1.example.net.`, and give the nameserver an
   `A`/`AAAA` record pointing at the relay host.
3. Let the node bind port 53 and the ingress ports (on Linux, `CAP_NET_BIND_SERVICE`).

A host service on one of the ingress ports, such as the host's own `sshd`, keeps its connections
when it listens on the host's own address: the kernel prefers the exact address to the relay's
wildcard listener. A relay node may announce services of its own: its own name resolves like any
other, and the ingress dials its own targets directly, since the overlay has no hop to itself.

### What the relay answers

* `<did>.<zone> AAAA` is the owner's address, `MX` is the name itself when the owner serves
  port 25, and the TXT records the owner publishes are served at their names under its own.
* A name that is no DID, or a DID that announces nothing, is `NXDOMAIN`; a DID whose descriptor
  the relay cannot fetch is `SERVFAIL`, which resolvers retry and do not cache.
* An owner that goes offline vanishes from the DHT within minutes. The relay keeps answering for
  it from its memory for `grace_secs`, so a mail server sees a connection failure, a temporary
  error it retries, and does not bounce the mail. An owner that announces nothing (an empty
  `services` section) is forgotten at once.

## Serving a node through relays

A node announces what it serves with a `services` section:

```yaml
services:
  ports:
    - port: 25
      target: 127.0.0.1:2525
    - port: 22
      target: 127.0.0.1:22
  txt:
    - name: _acme-challenge
      values: ["<token>"]
```

* `ports`: each port of the node's relayed address, and the local address the node dials for a
  connection to it. The node registers each as the TCP relay service `dns-relay/<port>`, and its
  relay dials nothing else, so a relay can reach only what the node mapped.
* `txt`: TXT records under the node's name; `name` is relative to it, and empty for the name
  itself. An ACME DNS-01 challenge is published this way, so the node can hold a certificate for
  `<did>.<zone>` and end TLS itself.
* `heartbeat_interval_secs` (default 30) and `ttl_secs` (default 90): how often the node signs and
  publishes its descriptor, and how long each one is live.

The node is then `<did>.<zone>` in every zone a relay serves, with the DID that `rings run`
prints. With port 22 mapped to the node's SSH server, `ssh user@<did>.<zone>` reaches it.

## Example: a mailbox

`examples/mailbox` is `me@<did>.<zone>` as a program: an SMTP receiver for its own address in
every zone, a mailbox store (INBOX, Sent, Drafts, Trash, Junk and Archive, with flags and UIDs),
an IMAP4rev1 server for the user's mail client (IDLE, UIDPLUS, MOVE, SPECIAL-USE), and SMTP
submission that delivers to each recipient's mail exchanger, so one node's mailbox writes to
another's through the relays.

```bash
cargo run -p rings-mailbox-example -- --did <the node's DID>
```

It listens on loopback only: the receiver on `127.0.0.1:2525` (map it as port 25 in the node's
`services`), submission on `127.0.0.1:1587` and IMAP on `127.0.0.1:1143`, with user `me` and the
password it writes to `~/.rings/mailbox/password`. Point a mail client at the last two. The
example ends no TLS; expose IMAP through a relay only behind a service that does.
