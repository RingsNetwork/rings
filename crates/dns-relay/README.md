# rings-dns-relay

`rings-dns-relay` names every node of a Rings network on the internet and relays its services
there (#1049). An operator delegates a DNS zone, such as `rings.rs` or `p2p.example.com`, to a
relay node. Every node then has the name `<did>.<zone>`, and the services it announces are
reachable at that name by ordinary internet software: an SMTP server delivering to
`me@<did>.<zone>`, an SSH client, a browser.

The crate is native-only and knows no Rings type. The node that embeds it hands it two
capabilities: a [`Directory`] that reads what an owner announces (in Rings, the owner's signed
descriptor in the DHT) and a [`Forward`] that carries a connection to one of an owner's ports
(in Rings, the TCP relay to the owner's node).

<!-- ANCHOR: model -->
```text
  Z        the zones delegated to the relay (NS records at their parents)
  o        an owner; label(o) is its canonical DNS label, a DID's "0x" and 40 hex digits
  σ(o)     what o announces: its ports, each with or without a PROXY header, and its TXT
           records, or ⊥
  P, k     the relay's IPv6 prefix and secret address key
  addr(o)  = P ∨ (HMAC-SHA256_k(label(o)) ∧ hostmask(P))
  B        the address book: addr(o) ↦ o for every owner the relay answered for
  μ        the memory: the last σ(o) seen; it answers without a lookup for a freshness window,
           and for a grace period g after o vanishes

  DNS      label(o).z   AAAA  addr(o)                 when σ(o) ≠ ⊥ (or μ(o) is within g)
           label(o).z   MX    10 label(o).z           when σ(o) serves port 25
           s.label(o).z TXT   txt(σ(o))(s)
           any other name in z: NXDOMAIN, with z's SOA

  ingress  a TCP connection to (a, p) is relayed to port p of B a when σ(B a) serves p,
           a PROXY v2 header first when σ says so, else closed
```

Each owner has its own IPv6 address, so a protocol that names no host before its first byte
reaches the server, as SMTP and SSH do not, is routed by destination address alone: the relay
parses nothing it carries. The address key stops anyone from grinding an owner onto another
owner's address, and the book keeps the map's inverse, injective, across restarts. The memory
keeps an owner that goes offline named for the grace period, so a mail server retries a
delivery instead of bouncing it; an owner that announces nothing is forgotten at once. The book
and the memory hold at most `max_owners` owners, and at most `max_lookups` directory lookups run
at once, so neither made-up names nor made-up owners grow the relay's state or its work without
bound.
<!-- ANCHOR_END: model -->

## Module layout

- `owner`: the [`Owner`] trait, the identity a label names, with its round-trip laws.
- `announcement`: [`Announcement`], the validated content of an owner's descriptor.
- `zone`: [`Zones`] and [`Placement`], where a queried name lies.
- `address`: the pure address map ([`AddressPlan`]) and its inverse ([`AddressBook`]).
- `memory`: [`AnnouncementMemory`], the pure memory of announcements and its grace period.
- `authority`: [`answer`], the authoritative answer as one pure function.
- `capability`: the [`Directory`] and [`Forward`] capabilities the node provides.
- `state`: the relay's state files: append-only journals and the address key.
- `registry`: the address map's shell, keeping its book in the state directory.
- `recall`: [`Recall`], the relay's front of the directory: its memory, kept there too, and the
  lookup budget.
- `proxy`: the PROXY protocol v2 header a forward sends first.
- `server`: the DNS shell over `hickory-server`.
- `ingress`: the TCP shell, routing by destination, with two-step [`Admission`].
- `socket`: the listening sockets of DNS and the ingress, IPv6 only on an IPv6 address.
- `config` and `relay`: the configuration and the [`DnsRelay`] that opens and runs it.

## Operating a relay

1. Route an IPv6 prefix (a /64 is plenty) to the relay host, and make it local:
   `ip -6 route add local 2001:db8:1::/64 dev lo`.
2. Delegate the zone to the relay: at the parent, `NS` records naming the relay's nameserver,
   and that nameserver's `A`/`AAAA` records in its own zone.
3. Configure the zone, the prefix, the ports to relay and a state directory. The ingress listens
   IPv6 only, so a host service on the same port's IPv4 wildcard keeps every IPv4 connection,
   and an IPv6 host service keeps its connections when it listens on the host's own address:
   the kernel prefers the exact address to the relay's wildcard listener.
4. DNS listens on `[::]:53` and `0.0.0.0:53` by default, the IPv6 one IPv6 only. Where a local
   stub resolver holds port 53 on loopback, as `systemd-resolved` does, the IPv4 wildcard cannot
   bind: list the host's own addresses in `dns_listen` instead.
