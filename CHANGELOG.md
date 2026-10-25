# Changelog

## Unreleased

- Every frame names its wire version (#1104), so a node of another release is refused by
  name, not by a codec error. A frame now opens with `RINGS\0` and the wire version, a
  big-endian `u16`: the one part of the wire every version decodes alike. A frame of another
  version is refused before its body is decoded, with `Error::IncompatibleWireVersion` naming
  both versions. Before, a 0.34 node joining a 0.36 seed failed with "Answer offer info failed:
  Codec deserialization error", and the other way round with "Decode error". The handshake
  strings of `createOffer`, `answerOffer` and `acceptAnswer` and every link frame carry the
  header, so both join paths do. A frame from before wire versioning, which opened with
  `RINGS-PAYLOAD\0` or `RINGS-LINK\0`, is named as such. Over JSON-RPC the refusal is error
  `210`, with data `{"ours": n, "theirs": m}`, where `theirs` is null for a node before wire
  versioning. `nodeDid` returns `wire_version`, and `connectPeerViaHttp` refuses an endpoint of
  another wire version, or of none, before it creates an offer. The native API sets the
  `x-node-version` header, the crate version, on every response. The wire version starts at
  `1` (`rings_core::message::WIRE_VERSION`).
  - **Bump rule.** `WIRE_VERSION` goes up by exactly one with every change that a node of the
    previous wire version would reject or misread: a frame, a message, a signature domain, or
    a handshake rule. A change no peer can observe never bumps it, and the crate version moves
    independently. Each bump is a total cutover: nothing decodes another version's frames,
    and nothing is negotiated.
  - **Library API.** `WIRE_VERSION`, `PeerWireVersion`, `WireVersionMismatch`,
    `Error::IncompatibleWireVersion` (core and node), `INCOMPATIBLE_WIRE_VERSION_CODE` and
    `NodeDidResponse::wire_version` are new. `Error::UnmarkedFrame` now also covers a header of
    this version with an unknown kind. The header is 9 bytes against the old 14-byte marker,
    so `MAX_CHUNK_ENVELOPE_OVERHEAD` drops from 912 to 907 and `MAX_PAYLOAD_ENVELOPE_OVERHEAD`
    from 3697 to 3692, both still witnessed with equality.
  - **Upgrade consequence.** Breaking on the wire: a node of this release and a node before it
    cannot connect. The earlier one fails with its old codec error; this one names the earlier
    one as a node before wire versioning. Seeds, browsers and embedders move together.
  - **Operator step.** None beyond upgrading every node that must reach another.

- A node can be closed before it is dropped (#1098). `Swarm::close`, `Processor::close` and
  `Provider::close` disconnect every peer, admitted or still handshaking, awaiting each physical
  close, and refuse every later connection with the new `Error::SwarmClosed`.
  `rings_node_provider_destroy` closes its provider this way before the runtime shuts down. A
  dropped node is freed (#1093), but a peer connection that was never closed kept its ICE
  sockets bound for the life of the process, so an embedder that stops and starts nodes in one
  process leaked them on every restart. An embedder should call `close` before dropping a node
  it stops. Each peer then sees the connection close at once, as on `disconnect`, instead of
  waiting for an ICE timeout. Awaiting the release of the stores and the end of the node's
  background tasks is #1085.
  - **Library API.** `Swarm::close`, `Processor::close`, `Provider::close` and
    `Error::SwarmClosed` are new, all additive.
  - **Upgrade consequence.** None: the wire format is unchanged.
  - **Operator step.** None.

- A node that had a peer is freed when its owner drops it (#1093): at once if its runtime is
  dropped with it, and otherwise once the work it has in flight ends (awaiting that end is
  #1085). Each connection's callback held the swarm transport that holds the connection, and the
  connection's inbound actor held it as long as the callback lived, so a dropped swarm that had
  ever connected was never freed: not its transport, its connections, its DHT, nor its stores.
  Since the per-store lock (#1039), a node started again in the same process on the same store
  failed with `Error::StorageDirectoryLocked`. The callback and its actor now hold the transport
  weakly and bind it for each event; an event that arrives after the transport is gone is
  dropped, and an actor that finds it gone stops. Nothing changes on the wire. A peer connection
  that was never closed keeps its ICE sockets bound; close the node first (#1098).
- aarch64 builds use the ARMv8 AES and POLYVAL instructions (#848). `aes` 0.8
  and `polyval` 0.6 take them only under `--cfg aes_armv8` and `--cfg polyval_armv8`, so until
  now every aarch64 build (Apple silicon, ARM servers) ran AEZ and DTLS on the software fixslice
  backend, which cost 13.8% of the #843 loop's CPU (#1087). `.cargo/config.toml` now sets both
  cfgs for every aarch64 target. The crates still detect the extension at runtime (Linux,
  Android and every Apple target, iOS included) and fall back to software where it is missing or
  undetectable. x86_64 already detected AES-NI at runtime and is unchanged. CI
  runs the `rings-aez` known answers on both backends, and the cluster image copies
  `.cargo/`.
  - **Operator step.** A build that sets `RUSTFLAGS` in the environment replaces the cargo
    config's flags. To keep the hardware path, add `--cfg aes_armv8 --cfg polyval_armv8` to
    such a `RUSTFLAGS` on aarch64.

- A delegation slot that resolves by reference skips its delegator's signature (#1089). Every
  message carries two delegations, and recovering each delegator's ECDSA key cost 8.4% of the
  #843 loop's CPU (#1087). A referenced slot resolves only against the link's lessor table, whose
  every entry was leased from a frame that verified in full, so its delegator authorization is
  established: it is now verified for its expiry and the delegatee's signature only (the new
  `DelegatorAuthorization`, `Delegation::verify_under`). An inline slot, and every frame off a
  link, is verified in full as before. One proof costs 66 µs instead of 133 µs when its slot is
  referenced (release, M1 Max). No operator step.

- The WebRTC SCTP write loop marshals each gathered batch of packets in one blocking task
  instead of one per packet (#1090). `webrtc-sctp 0.17.1` ran every outbound SCTP packet
  through `tokio::task::spawn_blocking`, which on a multi-thread runtime cost about 38% of the
  #843 onion loop's busy CPU in the blocking pool's lock, against 1.5% for the marshalling
  itself. rings now carries `webrtc-sctp 0.17.1` in `crates/transport/vendor/webrtc-sctp` with
  that one change, through `[patch.crates-io]`: the write loop still suspends into the blocking
  pool once per wakeup, so the read loop keeps its turn at the association lock, and packets
  are sent in the order they were gathered. The patch is removed with the move to webrtc-rs
  0.21 (#1071). CI runs the crate's tests, including a new TSN-order test, on Linux and macOS.
  No operator step.

- The replay receiver table and the removal store are shared by connection slot, not by
  origin (#907; load-bounds paper, Laws 1 and 2, Sec. V). An origin is a free key pair, so
  anyone could fill the receiver table with one transaction from each of 16384 fresh origins and
  refuse every new origin for good. A node has `R(s) = 2(160 + s + 1)` connection slots; an
  admitted connection takes the least free one and frees it when it is retired. Each item of
  either table is now charged to the slot of the handshake-authenticated connection that
  delivered it, to this node for its own writes, repairs and connection signaling, or to a
  dedicated share: the relay inbox for mail delivered out of it, and custody for an ownership
  hand-off of the range a node owns from the node its view names as the range's previous owner.
  It is admitted within that slot's floor, its fair claim `⌈P / n⌉` of a shared pool, or the
  share of its principal. An item stays charged to its slot after its connection leaves, so a
  table never holds more than `R(s)·f + P + L + S` items however many connections rotate; a
  connection admitted into a slot inherits what the slot still holds. The replay receiver
  table has `f = 32`, `P = 3328`, `L = 1024`, inbox `768` (15616 streams at three successors,
  within its hard capacity of 16384 up to 15 successors); the removal store `f = 128`,
  `P = 16384`, `L = 7168`, custody `16384` (81920, filed in at most about 19.6 MB, so the native
  `dht-removals` budget rises from 16 MiB to 24 MiB). A replay stream is retired once it is spent:
  `σ` after its last proof expires, and for application streams another relay-inbox retention
  plus `σ` later; a refusal therefore lasts until a stream of the refused slot is spent, never
  for good. Handlers receive a `HandlerContext`, which carries the payload and the principal
  that delivered it.
  - **Breaking (library API).** `HandleMsg::handle` takes a `&HandlerContext`; its
    `payload()` is the `MessagePayload`. `RemovalCharge` and `REMOVAL_ORIGIN_SHARE` are gone: a remove's charge
    is a `Principal`. `Error::TransactionReplayStreamCapacityExceeded` and
    `Error::RemovalCapacityExhausted` carry the introducer and the `ShareRefusal`.
    `ChordStorageRepair::read_repair_entry` takes the principal that delivered the entry.
    `KvStorageScan` gains the required method `remove_batch`, which removes records with one
    flush, as `put_batch` writes them; the replay store retires spent streams in the batch
    their admission joins.
  - **Upgrade consequence.** A receiver replay record and a removal record each gain their
    charge, so records of an earlier build do not restore (no protocol versioning before 1.0).
  - **Operator step.** Stop the node and remove the `transaction-replay` store, the documented
    reset of the replay guarantee, and the `dht-removals` store (browser: the IndexedDB stores
    `<storage name>/transaction-replay` and `<storage name>/dht-removals`), then start it.
    Replication rebuilds the removals a node holds.

- Held relay mail expires 7 days after its hold (#907; load-bounds paper, Def. 3). A held
  message retires `MAX_RELAY_INBOX_TTL_MS` after the instant its holder signed: every read, join
  and sync of its inbox drops it, and a recipient never delivers it. Before, a held message
  lived as long as its inbox, which every newer hold extended, so mail could linger without
  bound and a member could deliver an old message after the recipient's replay window had
  forgotten its stream. The rule reads the signed hold instant, not the element's dot, so every
  replica drops the same messages and rewriting an old hold gains nothing.
  - **Upgrade consequence.** Mail held more than 7 days ago is dropped at the first read of its
    inbox.

## 0.37.0

- Link-local onion cells no longer take a durable replay write (#1081; durable-records, Law
  Window by classification). A transaction is link-local when it is Application traffic, it
  arrived on the authenticated connection of its own origin, and its namespace declared delegated
  replay. Such a transaction is admitted in a volatile replay window of that connection, which is
  released with it; every other transaction, a relayed copy included, keeps the durable window.
  A protocol declares delegated replay through the new `Protocol::delegates_replay` (default
  `false`), published to core with the new `Swarm::set_replay_delegation`. The onion data plane
  declares it: its replay store and process epoch already admit each cell at most once, across
  links and restarts.
  - **Library API.** `Swarm::set_replay_delegation`, `Swarm::delegates_replay`, `ReplayWindow`
    and `Protocol::delegates_replay` are new, all additive: existing code compiles unchanged.
  - **Upgrade consequence.** None: the replay record format is unchanged, and a node that
    declares nothing behaves as before.
  - **Operator step.** None.

- `PeriodicMeasure::close` stops a measurement and waits until it has released its stores (#1083).
  A drop releases them only after the persistence worker has written the final snapshot on a later
  poll, so a store directory reopened in the same process right after a drop could still be held
  (`Error::StorageDirectoryLocked`, #1039). `close` resolves once the worker has persisted any
  pending snapshot and dropped both stores.

- A node stamps its own writes in issue order (#1075, #1033; folds in #1076). Every operation a
  node stamps is issued strictly after every version it stamped or passed through before and
  every version it read for the key (its cached carrier, and the replica an owner holds), with
  that floor capped one tick below `now + σ` (`PeerRing::stamp_operation`, a hybrid logical
  clock). Before, a write was stamped at its millisecond alone, so two writes by one node in one
  millisecond were ordered by their digests: an append right after an overwrite could fall
  below the overwrite's reset floor and be dropped at every owner (#1075), and two appends could
  be listed out of issue order (#1033). A node's writes now keep their issue order, even across
  a step back of its clock, a write follows every value its writer read, and no stamp leads the
  node's clock by more than the tolerance, however far ahead a value it read or forwarded was.
  `EntryOperation::stamped_after` stamps above a given floor. `EntryVersion` gains the HLC
  counter `counter`, ordered after the millisecond and before the issuer: a stamp after a floor
  at the same or a later millisecond raises the counter instead of the millisecond, so a burst
  of writes ascends strictly without running ahead of the clock.
  - **Upgrade consequence.** The `EntryVersion` encoding changes (every dot, remove and
    register), so stored and replicated DHT carriers of an earlier build do not decode (no
    protocol versioning before 1.0); the carrier suprema grow by 5 bytes per dot and remove.
  - **Operator step.** Remove the DHT data store while the node is stopped; replication
    rebuilds it, and an undecodable stored carrier is otherwise retired on its next read. Relay
    carriers held under an earlier build do not decode either and are retired (and logged), so
    mail held across the upgrade is lost; drain held mail before upgrading.

- Replay writes are group-committed (#916; durable-records, Law Group commit). A replay
  transition still runs whole under the one replay lock, but the lock no longer covers the record
  write: the transition applies its effect to the in-memory tables and stages its record, then
  waits for its batch. The transitions that run while one batch is written gather into the next
  batch. A native authoritative store writes a batch with one `F_FULLFSYNC` barrier on Apple
  targets and one directory flush. IndexedDB writes it as one strict transaction. No result is
  returned before its batch is durable, refusals included. A failed batch fails and undoes every
  pending transition. One run on an M1 Max measured 922 admissions/s with 64 in flight, against
  49–57 before. A lone admission still pays one batch write. The onion WebView test runs on the
  authoritative IndexedDB replay store again.
  - **Breaking (library API).** `KvStorageScan` gains the required method `put_batch`, so an
    implementor of that trait must add it. `Error::TransactionReplayPersistence::source` is an
    `Arc<Error>`, shared by the transitions of a failed batch.
  - **Upgrade consequence.** None: the record format is unchanged.
  - **Operator step.** None.

- Registry heartbeats re-append a signed lease (#918; removal-stability paper, Laws 8 and 9).
  An online-node, onion-exit or DNS-relay service descriptor no longer signs a heartbeat time:
  it signs a lease end (`expires_at_ms`, 10 minutes after it is issued) and a freshness window
  (`fresh_for_ms`, the configured TTL), so its bytes stay fixed for the lease and every heartbeat
  appends the same element, which raises its dot and writes no tombstone. A heartbeat no longer
  reads the registry or removes anything. A reader takes `heartbeat_at_ms` from the element's
  dot, unsigned, and lists a descriptor while the time is at most the lease end and at most
  `heartbeat_at_ms + fresh_for_ms + σ`, and never lists a lease ending more than 10 minutes
  and 3 s ahead or a freshness window over 10 minutes, whatever the signer chose; of two leases
  of one registrant the later one wins. A registrant renews its lease once fewer than two
  heartbeats remain in it, or when a signed field changes. A heartbeat interval must now be below 300 seconds (half the lease), down from
  595, and below the TTL, and a TTL must be at most 600 seconds (the lease). The
  `lookupOnlineNodes` and `lookupOnionExits` results gain `fresh_for_ms`.
  - **Upgrade consequence.** The descriptor encoding changes, so a node reads no descriptor an
    earlier build published, and an earlier build reads none of this one's: registries list
    only nodes on the same build (no protocol versioning before 1.0). A descriptor someone
    re-appends after its registrant stopped stays listed until its lease ends, at most 10
    minutes, where before it expired with its TTL.
  - **Operator step.** None; earlier descriptors are ignored and expire within the element
    horizon. A configuration with a heartbeat interval of 300 seconds or more, or a TTL over 600
    seconds, is refused at start and must be lowered.

- Data-topic removals are never evicted (#920; removal-stability paper, Laws 2–4 and 6). A
  data carrier is stored as two records: its values in the data store, which its byte budget
  may evict, and its tombstones and overwrite floor in a new retained store, `dht-removals`
  beside the data store (browser: `<storage name>/dht-removals`, strict IndexedDB), which
  flushes and never evicts. A write stores the remove side first and the values after; reads
  glue the two without hashing, and the first operation after a node opens normalizes any
  value a crash rolled back. Each uncollected remove is charged to the authenticated sender of
  the write that raised it (a predecessor, a successor, or the node itself is the holder
  class): a write that would pass 1024 for its sender (`REMOVAL_ORIGIN_SHARE`), or 65536 in
  all (`REMOVAL_STORE_MAX_REMOVES`), fails with `RemovalCapacityExhausted` and changes nothing,
  and a sync batch leaves such an entry unacknowledged. `SwarmBuilder::removal_storage`,
  `ProcessorBuilder::removal_storage` and `PeerRing::with_removal_storage` set the store;
  without one the removes are kept in memory. `ChordStorageRepair::read_repair_entry` takes
  the node that supplied the entry.
  - **Upgrade consequence.** A topic stored before this release keeps its tombstones in the
    data store until it is next written, and is read as before meanwhile.
  - **Operator step.** None required. An operator who wants the eviction law to hold from the
    first eviction removes the data store while the node is stopped; replication rebuilds it.

- Data-topic tombstones are bounded (#921; removal-stability paper, Law 5 and Prop. 4). A data
  carrier holds at most `DATA_TOMBSTONE_MAX_LEN` (1024) uncollected removes. A delta carrying
  more is not admitted, and a join, operation or repair whose result would hold more fails with
  `EntryTombstonesExceedMax` and changes nothing; removes are never truncated, since a dropped
  remove could resurrect a value it still covers elsewhere. A storage sync batch refuses such an
  entry without acknowledging it and persists the rest, so its owner keeps it. The carrier law
  now counts every field: a carrier at every bound encodes in exactly `DATA_CARRIER_WIRE_MAX`
  (33 815 728) bytes, asserted to fit one transport message, with each encoding supremum
  witnessed by a test. `EntryKind::max_tombstones` is replaced by `EntryKind::tombstone_bound`,
  which says whether a kind refuses (data topics) or truncates (relay inboxes).
  - **Upgrade consequence.** A topic that removes more than about 10 values a minute,
    sustained, has its further removals refused until earlier ones are collected; shard such a
    topic. Registries must run a build with #918, whose heartbeats remove nothing.
  - **Operator step.** None.

- Relay carriers are replicated to the first `r` nodes from their key's owner that hold relay
  mail, down a chain (#777, durable-records Section X): `r` is configurable
  (`RelayReplication`, `SwarmBuilder::relay_replication`, `ProcessorBuilder::relay_replication`)
  and 8 by default, so held mail survives its holder's tab closing or private session ending as
  long as another member survives the repair delay.
  - The holder forwards a hold to its next member (the new `ReplicateRelayHold` message), each
    member forwards it on, and acknowledgements return from the chain's end: the writer is
    acknowledged (`RelayCarrierHeld`) only once every member holds the hold.
  - A member's next member past its successor list is found by the owner query of #1045, whose
    answer (`RelayHolderFound`) now also names the owner's last placed node (`horizon`) when it
    names no holder, and is cached until the successor list changes.
  - Each repair pass a member sends one digest set to each of its two chain neighbours (the new
    `RelayCarrierDigests` and `RelayCarriersWanted` messages) and copies only the carriers that
    differ; losing a chain neighbour runs that exchange at once, outside the repair schedule.
  - A node declares that it holds relay mail only after 2 minutes online (at least four
    stabilization periods), and then tells its connected peers (the new `DeclareRelayCustody`
    message).
  - `AnnounceRelayHolder` carries the sender's `r` nearest previous holders (`previous`), from
    which every node decides whether, and at which rank, it is a member of a key.
  - A recipient fetches its inbox from the holder and then down the chain, and removes what it
    delivered at the member it read it from; only the holder's report restricts relay custody.
  - A node keeps up to `r` times the relay carriers it kept before.
  - **Upgrade consequence.** `AnnounceRelayHolder`, `RelayHolderFound` and `ReplicateRelayHold`
    change shape and messages are added, with no version: nodes of this version and of an
    earlier one cannot exchange relay custody, so every node of an overlay upgrades together.
  - **Operator step.** Upgrade every node of the overlay at once. No store needs clearing: the
    first repair passes copy every carrier down its chain.

- **Breaking:** 33-byte public keys follow SEC1 §2.3.3 (#1072): the tag `0x02` marks an even
  y-coordinate and `0x03` an odd one. Rings had both inverted, in `PublicKey::<33>::from_u8`
  (compressing a 64-byte `x ‖ y`) and in the P-256 decoder `ct_into_secp256r1_affine`, so it
  round-tripped only with itself; any standard SEC1 producer or consumer, such as Web Crypto,
  WebAuthn or a wallet, got the negated point `(x, −y)`. P-256 keys are now decoded by the
  library's SEC1 decoder, as secp256k1 keys already were, and a tag other than `0x02` or `0x03`
  is refused. The prefix `0x00` keeps one meaning, the Ed25519 embedding `0x00 ‖ k` of a
  32-byte key; it is not a point of unknown parity.
  - **Upgrade consequence.** Keys secp256k1 derives itself (node, delegatee, E2E, ElGamal and
    onion keys, and their DIDs) were already SEC1 and do not change. What changes is every
    secp256r1 account key, whose 33 bytes, and so its DID, are new; every delegation signed by a
    secp256r1 account, which no longer verifies; and every key given as 64 bytes `x ‖ y` (a
    secp256r1 or secp256k1 account entity, an RPC `recipient_public_key`), which now denotes
    `(x, y)` instead of `(x, −y)`. There is no compatibility path: an old encoding is dropped,
    not converted, and nodes upgrade together.
  - **Operator step.** While the node is stopped:
    - native: delete the data and measure stores and every store beside them: the `data`,
      `relay-inbox`, `dht-removals`, `transaction-replay`, `onion-entry-guards`, `measure` and
      `provisional-evidence` directories (under `storage_path` for an FFI provider);
    - browser: delete the site's IndexedDB databases `rings-node`, `rings-node/relay-inbox`,
      `rings-node/dht-removals`, `rings-node/transaction-replay`,
      `rings-node/onion-entry-guards`, `rings-node/measure` and
      `rings-node/provisional-evidence`;
    - a delegatee key file whose delegator is a secp256r1 account: issue a new one, since its
      delegation no longer verifies. A key file `rings init` wrote needs no step.

## 0.36.0

- A relay carrier no longer strands at its old holder when two relay-capable nodes join one
  storage-less run longer than a successor list within one convergence window (#1045). The owner
  of a relay key always places it, as the key lies in its own arc, so an old holder whose view
  cannot place a carrier's key asks the key's owner for its holder (the new `FindRelayHolder` and
  `RelayHolderFound` messages, routed as a lookup) on each repair pass, and offers the carrier
  there too, taking only the first answer to a query it asked, at most one query per such key
  and pass. The holder still takes only a key its own view makes it hold, so a wrong answer
  moves nothing. No operator step beyond upgrading the overlay together: the message set grows
  with no version.

- Fetch-cache linearization (#922): a cache read is pure and every cache change is one
  transition under the cache transition (removal-stability paper, Law 7), so a read that
  retires a dead entry can no longer drop a concurrent put. The behaviour has held since #913;
  this release adds the law to `ChordStorageCache` and a test that parks a read between its load
  and its return while a put completes.
  - **Upgrade consequence.** None.
  - **Operator step.** None.
- DNS relay (#1049): every node is named `<did>.<zone>` in each zone a relay serves, and the
  services it announces are reachable there from the internet. The new crate `rings-dns-relay`
  is the relay: an authoritative DNS server (`hickory-server`) for the zones delegated to it, an
  address map giving every DID its own IPv6 address in the relay's prefix (keyed by a secret,
  its inverse kept across restarts), a memory of announcements that answers without a lookup
  for a freshness window and keeps a vanished owner named for a grace period, and an ingress,
  IPv6 only, that relays a TCP connection to `(address, port)` to the owner's announced port.
  The book and the memory are append-only journals in the state directory, bounded by
  `max_owners`; at most `max_lookups` DHT lookups run at once, and an `ANY` query gets one
  record set (RFC 8482). On the node, the `services` section of `config.yaml` maps ports to
  local addresses, each optionally preceded by a PROXY protocol v2 header naming the client
  (`proxy_protocol: v2`): the node registers each as the TCP relay service `dns-relay/<port>`
  and publishes a signed service descriptor (`ServiceDescriptor`, domain tag
  `rings-node:service-descriptor`) on the DHT topic `dns_relay:<did>` every heartbeat
  (`ProcessorBuilder::announce_services`). The `dns_relay` section runs a relay
  (`rings_node::dns_relay::{DhtDirectory, RelayForward}`); a relay node's own served ports are
  dialed directly, since the overlay has no hop to itself. Both sections are absent by default.
  The new `RelayHandle::carry_tcp_stream` relays an accepted stream like `relay_tcp_stream`, a
  given preamble first, and returns only once the relay is done with it, so a caller bounding
  its live connections (the ingress's per-owner admission) holds its slot for the connection's
  life. The new `examples/mailbox` is `me@<did>.<zone>` as a program: SMTP receiver (reading the
  PROXY header with `--receiver-proxy-protocol`), mailbox store, IMAP4rev1 server and SMTP
  submission with an outbound queue and its own SMTP client.
  - **Upgrade consequence.** None: the descriptor is a new registry, and a node without the new
    sections publishes and runs nothing new.
  - **Operator step.** None, unless running a relay: see the DNS Relay chapter.
- Sender sequences are reserved in leases (part of #1043). A reservation writes its stream's
  replay record only when it outruns the stream's durable lease, which then covers at least the
  next 64 sequences (`TRANSACTION_SEQUENCE_LEASE`). Reservations inside the lease are handed out
  from memory. A stream sending one transaction at a time writes once per 64 transactions
  instead of once per transaction. That removes about half the replay writes of a browser onion
  request: about 1,030 instead of about 2,040. A restart resumes past the lease, so it may skip
  up to 64 sequences, which the receiver takes as one `Advance`, and it never reuses a sequence.
  - **Upgrade consequence.** None: the record bytes are unchanged. The sender record's value now
    reads as the last leased sequence, and a record written by an earlier release holds the
    last reserved sequence, which is a valid lease.
  - **Operator step.** None.

- RUSTSEC-2026-0387 (`crypto-bigint`: secret-dependent branches in constant-time operations) is
  ignored in `deny.toml` for now (#1051). The fix exists only in `crypto-bigint` 0.7.6, while
  0.5.5 reaches us through `elliptic-curve` 0.13 (`k256` in rings-core, `p256` via webrtc's
  `dtls`), which no 0.5.x release fixes; #1051 tracks the `elliptic-curve` 0.14 upgrade.

- A node given no transaction replay store refuses every replay transition (#1022): a
  reservation or an admission fails with the new `Error::TransactionReplayStorageUnavailable`,
  so it neither sends nor accepts a transaction, instead of keeping its replay windows in memory
  and reopening every stream at each restart. `SwarmBuilder`, `ProcessorBuilder` and `Provider`
  no longer default the replay store to `MemStorage`; the native daemon and the browser provider
  open an authoritative one, as before. Breaking for `rings-core` and `rings-node` library
  callers that relied on the in-memory default: pass `replay_storage(...)`. A relay-inbox drain
  on such a node defers its mail.
- **Breaking (C ABI):** `rings_node_new_provider_with_callback` takes a new `storage_path`
  argument, before `signer` (#1022). The FFI provider used to keep every store in memory, its
  replay windows included; it now opens the native node's stores under that directory, each at
  its authority, through the new `rings_node::native::stores::NodeStores`, which the daemon uses
  too: `data`, `transaction-replay` (authoritative), `relay-inbox` (retained),
  `onion-entry-guards` (durable cache), `measure` and `provisional-evidence`. Pass the same path
  on every run of one node. `rings.h`, `docs/src/ffi.md` and the Python, Kotlin and Swift
  wrappers take the path too (`create_provider(runtime, acc, storage_path)`,
  `createProvider(account, storagePath, signer)`, `RingsProvider(runtime:account:storagePath:)`).

- Relay custody moves past nodes without a relay store (#1037). Every node declares whether it holds
  relay mail: in its connection handshake (the new `relay_custody` field of `ConnectNodeSend` and
  `ConnectNodeReport`, `rings_core::dht::RelayCustody`) and in its online-node descriptor
  (`OnlineNodeCapabilities::relay_custody`; `holds_relay_mail` in the JSON-RPC
  `OnlineNodeCapabilitiesInfo`). The holder of a relay inbox key is the first node at or after the
  key's owner along the successor path that holds relay mail, and one function (`dht::relay_holder`)
  names it on every relay path, so mail for an inbox whose owner keeps no relay store is held by the
  next node that does, instead of being refused and lost. A recipient without a relay store fetches
  its inbox from its holder (the new `FetchRelayInbox` and `RelayInboxReport` messages) and removes
  what it delivered there. Each repair pass, every node passes the previous holder it knows to its
  successor head (the new `AnnounceRelayHolder` message), so a holder places the keys owned on the
  storage-less run behind it. A hold with no holder within the writer's successor list is refused to
  the writer with the new `Error::RelayHolderUnavailable`; a relay operation sent to a node whose
  view does not make it the holder is refused there with the new `Error::RelayCarrierNotHeldHere`. A
  writer keeps every hold it sends until the holder acknowledges it (the new `RelayCarrierHeld`
  message) and sends every kept hold again on each repair pass until a holder takes it or its sender
  proof expires; a target's refusal (the new `RelayCarrierRefused` message) only lets it send
  sooner. At most `UNACKNOWLEDGED_RELAY_HOLDS_MAX` (64) holds are kept; another is refused to its
  local sender with the new `Error::RelayHoldsUnacknowledgedFull`. A holder takes a relocated
  carrier from any node that holds relay mail, for a key its own view makes it hold. Online
  messaging and data-topic ownership are unchanged.
  - **Upgrade consequence.** The handshake and the message set change with no version: a node of
    this version and one of an earlier version cannot connect, so every node of an overlay
    upgrades together.
  - **Operator step.** Upgrade every node of the overlay at once. No store needs clearing: relay
    carriers keep their keys, and a carrier whose holder moved is handed to it by the first
    repair passes.

- Each file store locks its own directory at open (#1039): `FileStorage` takes an exclusive lock
  on a `.lock` file in the store's directory (`flock` on unix, `LockFileEx` on Windows) and holds
  it while the store lives, so a second open of a store that is open, in the same process or
  another, fails with the new `Error::StorageDirectoryLocked` instead of interleaving writes with
  it. This covers the native daemon and the FFI provider alike: a second daemon or provider given
  an open node's stores now fails to start. The lock is per store, so stores under one root open
  together. A file system that offers no such lock fails the open. A lock is released when its
  process exits, so a crashed node leaves nothing to clean up; the `.lock` file may stay in place.

## 0.35.0

- `tokio` is bumped to 1.53.3 for RUSTSEC-2026-0386 (`AsyncFd::new` and related constructors
  violate I/O safety) (#1047), in the workspace, `frontend` and `examples/dweb` lockfiles.

- A node that rejoins its seed over HTTP is no longer refused for up to three minutes (#1040).
  An offer carried out of band (an HTTP join through `answerOffer`) now prevails over the seed's
  own sent, unanswered overlay offer to that node; offers carried alike keep the DID tie-break.
  `answerOffer` no longer returns a counter-offer; the HTTP client still answers one from a peer
  of 0.34.2 or earlier.

- Relay inbox carriers have a store of their own (#1021): `RecordAuthority::Retained`, the new
  lattice point (durable, retained), flushes every write, evicts nothing (a full store refuses
  the hold with `Error::StorageBudgetExhausted`) and retires an undecodable record with a log.
  The data store's budget therefore no longer evicts held mail, and a crash no longer loses or
  tears it. The native daemon keeps it in `relay-inbox` beside the data store (64 MiB); a browser
  provider in the IndexedDB database `<storage name>/relay-inbox` (1024 carriers).
  `SwarmBuilder::relay_storage` and `ProcessorBuilder::relay_storage` set it. Without one, a node
  takes custody of no relay carrier (the new `Error::RelayStorageUnavailable`): it refuses to
  hold mail and to take its own inbox's hand-off, so that mail stays with its holder. This is the
  default of both builders, of the FFI provider and of a browser whose IndexedDB cannot open;
  relay carriers are never kept in memory. `Provider`'s internal constructors take the store
  after the entry storage.
  - **Upgrade consequence.** Mail held by an earlier version is lost once: it stays in the data
    store, where it is no longer read, so it is never delivered, and the data store's budget
    removes it in time. An operator of a node that held relay mail at the upgrade should expect
    those messages not to arrive.
  - **Operator step.** None.

- Relay-inbox mail survives a restart between its admission and the application's take (#936).
  A relay delivery enters the window and a relay custody set in one write of its stream's
  receiver record; a delivery still in custody is queued again on redelivery instead of being
  refused as a replay, and is released from custody once the application takes it, before its
  element is retired. Custody follows the carrier (each drain pass drops the deliveries whose
  element left it) and holds at most `RELAY_INBOX_MAX_LEN` deliveries over the whole store; a
  full custody defers the drain (the new `Error::RelayCustodyFull`). A crash after a take and
  before its release delivers the message once more, with the same `ApplicationMessage::tx_id`,
  which an application deduplicates by. The live path is unchanged, so relay mail more than the
  replay window behind live traffic of its origin is still refused as the live path refuses it.
  `TRANSACTION_REPLAY_RECORD_MAX_BYTES` is now 3850 and `TRANSACTION_REPLAY_STORE_MAX_BYTES`
  28,314,240.
  - **Operator step.** The receiver record format changes, so a replay store written by an
    earlier version does not decode and fails every receiver stream closed. Before upgrading,
    stop the node and remove the replay store: the `transaction-replay` directory beside the
    native data store, or the browser IndexedDB database `<storage name>/transaction-replay`
    (`rings-node/transaction-replay` for the default provider). This resets every stream's
    replay window once, as clearing a stream does.

- The onion entry-guard store is a durable cache (#911): the new `RecordAuthority::DurableCache`
  flushes every write as an authoritative store does (native: file and directory; browser:
  IndexedDB `durability: "strict"`), so a crash no longer loses or tears the guard set, while a
  record that is absent or does not decode is still retired, now logged at `warn`, and the
  guards are re-selected instead of failing onion routing. Every authority now names its point of
  the durable-records lattice (`RecordAuthority::components`, a `Crash` and a `Meaning`), and
  every storage law is a function of those components, not of the variant. No operator step: the
  store's layout is unchanged.

- An authoritative file store (the native replay store) marks every record it made durable with
  an empty `<name>.p` file, so a record removed, renamed or lost outside the store fails its
  stream closed as missing instead of restarting it from `First` (#915). A scan reports such a
  record as the new `ScannedRecord::Missing`, a `get` refuses it with the new
  `Error::StorageRecordMissing`, and the root vanishing while the node runs makes every marked
  record missing. On Windows every rename of an authoritative store is write-through
  (`MoveFileExW` with `MOVEFILE_WRITE_THROUGH`), since a directory cannot be flushed there.
  - No operator step at upgrade: an existing store's records are marked at its first open.
  - Clearing one failed stream now takes two deletions with the node stopped: the record file
    and its marker `<name>.p` in `transaction-replay`. Deleting the file alone leaves the stream
    failed closed.

- The browser replay store commits every write with IndexedDB's `durability: "strict"` (#912), so
  a completed admission survives a system crash, not only a browser crash: the default hint is
  relaxed in Chromium since 121, and Gecko and WebKit do not flush a WAL commit by default.
  `IdbStorage` now talks to IndexedDB through `web-sys` directly instead of `rexie`, which cannot
  pass transaction options; its durability is its authority's (`RecordAuthority::flushes`, now
  shared by both backends). Breaking for `rings-core` callers on wasm:
  `Error::IDBError` carries the engine's error as a `String` instead of a `rexie::Error`.

- An authoritative IndexedDB store (the browser replay store) refuses instead of evicting
  (#923): a new row beyond its capacity, or an open under a lowered capacity, fails with
  `Error::StorageBudgetExhausted` and changes nothing, as the native authoritative store does.
  An evicted replay record used to restart its stream from `First`. Its `get` is now a read-only
  transaction, since a store that never evicts orders nothing by recency. The error's counts are
  in the store's budget unit (bytes for `FileStorage`, rows for `IdbStorage`).

- A config file that fails to load names itself and the parser's message (#1016): a missing or
  unknown key, a wrong type or malformed YAML no longer prints a bare `Encode error.`. Reading
  fails with the new `Error::ConfigParse` (code 821) and writing with `Error::ConfigSerialize`
  (code 822); both carry the config path and the `serde_saphyr` error as their source.

## 0.34.2

- `rustls-webpki` is bumped to 0.103.16 and `webpki-roots` to 1.0.10 for RUSTSEC-2026-0382
  (distrusted intermediates may be accepted) and RUSTSEC-2026-0381 (bundled roots trust
  distrusted emSign intermediates) (#997). Both are admitted ahead of the dependency cooldown,
  like the rustls advisory bump of #956.

- Removed options have no tombstones (#990): `gateway.onion_service`, `gateway.onion_hop_count`
  and `gateway.onion_allow_short_paths` fail as an unknown gateway key, and the removed
  `ONION_HTTP_PROXY_HOP_COUNT` and `ONION_HTTP_PROXY_ALLOW_SHORT_PATHS` variables are no longer
  read, like the other variables the onion loop cutover removed.

- A provider installs its onion runtime in one slot, native and browser alike (#990):
  `Provider::onion_circuit` returns the native handle of that runtime, installing it on first
  use, and replaces `NativeOnionCircuitHandle::install`.

- The browser `BrowserOnionProxy.exit_service()` export, which had no caller, is removed: the
  proxy always selects `https` (#990).

- Onion and gateway API cleanup (#990). Breaking for `rings-node` library callers:
  - `NativeOnionGatewayConnector::new(processor, onion)` drops its `proxy` parameter, which
    could only be `OnionProxyProtocol::TcpConnect`;
  - `NativeOnionCircuitHandle::relay_tcp_stream` is replaced by `open_tcp_target(processor,
    target)`, which selects the `tcp` route and opens the session (then call `relay`);
  - `OnionProxyRoute` drops its unread `protocol` field.

  `rings-gateway` gains a `test-support` feature exposing `rings_gateway::testing`, the
  in-memory packet fixtures; the `gateway-bench` example requires it.

## 0.34.1

- `onion_exit_policy` refuses unknown keys (#989). This is breaking for a config that still
  carries the removed `max_circuits` or `max_streams_per_circuit`: it now fails to load instead
  of loading silently with the session limit unspecified (`max_sessions: 0`). Replace them by
  `max_sessions`.

- Stores written by 0.32 or earlier are no longer converted (#989). Operator step, while the node
  is stopped, before upgrading from such a release:
  - browser: delete the site's IndexedDB data. Its schema-1 databases (0.31 and earlier) are no
    longer migrated to the access clock, and its replay store may hold the pre-#898
    shared-stream snapshot;
  - native: delete the `transaction-replay` directory beside the data store if it holds the
    pre-#898 shared-stream snapshot (`rings-core:transaction-replay`), which is no longer
    deleted at load.

  Either deletion resets every replay window once. A store written by 0.33 or 0.34.0 needs no
  step. `Defect::Misplaced` is removed from the replay store: a record restores iff it decodes
  and holds a valid window.

- Documentation (#989): `docs/src/jsonrpc.md` is the one JSON-RPC reference, documenting every
  method of `rings_rpc::method::Method` with its params, result, listener and Bearer
  authorization, and `crates/rpc/README.md` links to it; the gateway and node README examples
  drop `onion_service`; the reflection-bound docs state that only `FindSuccessorReport` returns
  through `reply_via` and connect signaling retraces its recorded path (#947); the 0.34.0 notes
  below state the `SwarmEvent` variants and core API additions correctly.

## 0.34.0

- Onion circuits are client-sealed loops of registered operation symbols (#834, Onion Kleisli
  pipelines; the specification is `papers/onion-kleisli-pipeline.tex`). This is a total
  network-wide cutover: nodes of 0.33 and 0.34 do not interoperate on onion traffic, and the seed
  must be upgraded with the release. Breaking changes:
  - Wire: the Sphinx header `χ = (α, β, γ)` of 14 uniform layers (`|χ| = 2919`) replaces the
    per-layer envelope (#840); relays are stateless, with no return tables, circuit ids,
    per-edge AEAD or exit reply signatures; the carry is AEZ v5 from the new in-repo crate
    `rings-aez` (#836), and replies are deniable (no exit signature). A loop leaves the client
    through its guard, crosses `s = 2` relays per segment and its symbol hops, and returns to the
    client as position `H + 1`; `n ≤ 4` symbols, `H ≤ 14` positions (#842).
  - Admission (L9, #841): every received cell is charged, by class, before any ECDH; replay is
    an x-keyed sliced Bloom store; ledgers are keyed by live authenticated link and bounded by
    the connection-registry capacity `𝓡`; a node's process epoch is fresh at every start and
    reset clears the admission state.
  - Sessions (#843, D2′): `tcp` and `https` are session symbols over the frames
    `data(n, φ, w)`, `fin(n, φ)`, `credit(m, υ…)` and `abort(n)`. The target is bound by its digest
    and travels under flag `T` until the first reply; the open ack is `data(0, 0, ε)` and a
    refusal is `abort(0)`; `abort` fails a session without a reason. The exit reads its world
    only while it holds the session's reply blocks (SURBs, up to `Q_max = 256`), so an
    unreplenished session pauses its target instead of dropping bytes.
  - Emission (#880): every link emits at the constant rate `r = ρB/V` while active and a cover
    floor `r_idle = 1` unit/s while idle, real cells substituting for cover, with a dwell of
    `D = 30 s`; the emitter paces on a read-only view of the Application lane's transport credit
    (`Swarm::application_credit`, `LaneCreditView`) and never reserves it.
  - Liveness (#975): the client sends a keep-alive loop every `V/2`; the exit answers a loop that
    finds no reply of its session for `V/2` with an empty `data`; the client fails a session
    silent for `Δ = 2V`, so a dead link on either segment is observed at both endpoints. Every
    credit frame carries the forward high-water mark `m`, the client's next forward sequence
    (#981), so a lost last forward frame is known missing at the exit on the next credit loop,
    and the exit fails the session with `abort` after `V`.
  - Circuit fate (#963, #964, #966, #969, #971): a retired link, a message dropped at dispatch
    or in the core application inbox from an admitted peer (`SwarmEvent::MessageDropped`,
    `LinkFact::Dropped`), or a full inbound stream lane fails every session riding it closed at
    that endpoint, which then tells the other endpoint with `abort`. A cancelled application
    send is reported as `Err(OutboundTransferCancelled)`, never as sent.
  - Configuration: `https` is a fetch only (CONNECT and SOCKS use `tcp`); exit targets accept
    `*:<port>`; `onion_http_proxy_service`, `gateway.onion_service` and the `<|>` service
    alternative are removed; the exit policy's `max_circuits` and `max_streams_per_circuit`
    are replaced by `max_sessions`; services outside `{tcp, https}` are rejected (#839); a relay
    requires `advertise_presence`, and an exit is always a relay (`OnionRole`, #842);
    `ProcessorBuilder::onion_idle_floor` may only lower the idle floor.
  - Core API: `Swarm::connection_registry_capacity()` is added.

- User DHT sends (`storage_fetch`, `storage_store`, `append`, `tombstone`) recompute their next
  hop from fresh topology after a refusal before acceptance, woken by topology, link or capacity
  events, never by a timer (#859, #862). A refused single-attempt write is
  `Err(SingleAttemptRefused { refusal })`.

- Connection establishment (#945, #946, #947, #953, #954, #960): connect signalling returns
  along its recorded path, so a simultaneous open always ends with one edge; routing no longer
  evaluates the link predicate under the topology lock; a yielded crossing offer is replaced by
  the answering generation in one transition, so it reports `ConnectionAttemptSuperseded`; the
  origin-quota clock is read under the admission lock, so no legitimate message is refused as
  `MonotonicTimeRegressed`; a browser ICE gathering ends when its connection is closed locally.

- The gateway names the cause of every captured-flow reset and degraded status (#950).

- `rustls` is bumped to 0.23.46 for RUSTSEC-2026-0346..0349 (#956).

- A delegation reference is a bounded, renewable lease, and nothing is held (#908). A frame
  whose reference missed was held for a repair round trip and released out of order, so later
  frames of its class could pass replay admission first and the released frame was lost as
  stale; the hold also had its own node-wide budget and sweep. Breaking changes:
  - Wire cutover: `LinkControl::{Request, Announce}` are removed, and the tags change.
    `Known { digest, remaining_ms }` grants a lease of `min(exp, now + 5 min) − now` and
    carries it as a remaining duration, so no clock is compared with another; the sender
    counts it from the instant it receives it. `Unknown(digest)` is a stateless disclaimer.
    The laws are in
    [Delegation references on a link](docs/src/advanced-topic/delegation-references.md).
  - The receiver keeps at most 128 leases per link, 8 of them reserved for the link peer's own
    delegation. It never reclaims a lease before its grace (one delivery bound past its end)
    ends, and reclaims ended ones only when it needs room for a new lease, so a frame that
    waits in its lane still resolves. When every lease is still in its grace it refuses a new
    one, and the sender keeps sending that delegation inline.
  - Only a frame that verified grants or renews a lease. A reference that finds less than half
    a lease left renews it.
  - Core: the session-link hold, its sweep, its release task and the node-wide hold budget are
    removed. The frames of one lane reach the logical stage in arrival order. A link-control
    send waits for credit at most one delivery bound.
  - A miss is dropped uncharged, and the sender told `Unknown(digest)`, after which it sends
    the delegation inline again.
  - The sender refuses to send a frame with a delegation that expires within one delivery
    bound (25 s), with the new local error `Error::DelegationExpiresWithinMargin`, which is not
    charged to the next hop. An origin delegation that expired in flight, within one delivery
    bound of arrival, is dropped without charging the hop that carried it, only when the rest
    of the frame verifies: the hop's proof now, and the origin's proof at its expiry.
  - `message::REORDER_DEADLINE` is re-derived as four delivery bounds, assuming at most four
    hops left to cross on a superseded path when a route or a hop's connection generation
    changes mid-stream; its value is unchanged (100 s).

- Read and write the node's YAML configuration with `serde-saphyr` instead of the deprecated
  `serde_yaml`, whose `unsafe-libyaml` RustSec now lists as unmaintained (RUSTSEC-2024-0449).
  Configuration documents are unchanged. `Error::SerdeYamlError` wraps a boxed `serde_saphyr::Error`.

- The built-in stream consumers detect a lost message instead of passing on a stream with a
  silent gap (#939). Since #935 application delivery is lossy by design, so the TCP relay
  wrote a byte stream with a hole in it and an E2E stream waited forever for a lost frame.
  Breaking changes:
  - Relay wire: a TCP `Frame::Data` carries its direction's `seq`, and `Frame::Shutdown` the
    number `sent` of segments before it; a UDP datagram travels as the new `Frame::Datagram`.
    A TCP receiver writes segments to its local socket in `seq` order only, reordering at most
    `REORDER_WINDOW` segments ahead, and applies a FIN once every segment it
    counted is written. A gap still open after `REORDER_DEADLINE`, a segment past the window,
    or segments that contradict the stream reset the session: the peer gets `Close`, and the
    local socket an abortive close. Every abrupt end (a reset, an overlay or socket failure, a
    peer `Close`, idleness) now closes the local socket abortively (RST) instead of with a FIN,
    so a local client sees a clean end of stream only once it received every byte. UDP flows
    stay unsequenced and lossy. `RelayEffect::Write` gains `seq`, `RelayEffect::Shutdown` gains
    `sent`, and `RelayEffect::SendDatagram` is new.
  - E2E: `E2eStreamDecryptor::new` takes the creation time `now_ms` and `with_reorder_window`
    becomes a builder; `decrypt_next` takes the frame's arrival time, and the new
    `check(now_ms)` reports a stream
    that made no progress for its stall deadline (`REORDER_DEADLINE`, or
    `with_stall_deadline`) as `Error::E2eStreamStalled`, whether the lost frame is the first, a
    middle or the final one. The default reorder window is `REORDER_WINDOW`, 32, and
    `DEFAULT_E2E_REORDER_WINDOW_FRAMES` (64) is removed. Node: `Processor::check_e2e_stream` polls a stream with the node's clock.
  - Core: `message::REORDER_WINDOW` (the replay window) bounds how many later frames of one
    stream may overtake a frame that still arrives, and `message::REORDER_DEADLINE` (four
    delivery bounds, 100 s) how late it may arrive after a later one.
  - Cutover: a relay wire change, incompatible with the previous release.

- Core never calls, awaits or consults application code (#935). Since 2022 core awaited
  application hooks inside its own processing (`custom_message`/`builtin_message`, then
  `SwarmCallback::on_validate`/`on_inbound`/`on_event` under a 30 s per-payload deadline on every
  lane, lane 0 included), and since #924's lossless credit an application stall held the sender's
  credit, coupling nodes through the application. Breaking changes:
  - `SwarmCallback`, `SharedSwarmCallback`, `Swarm::set_callback` and `SwarmBuilder::callback`
    are removed, with the application validation veto, the inbound callback deadline and its
    errors (`InboundValidationFailed`, `InboundValidationTimeout`, `InboundCallbackFailed`,
    `InboundProcessingTimeout`, `InboundTimerUnavailable`) and the callbacks' error type
    `error::CallbackError`.
  - New `swarm::application`: `Swarm::application_inbox()` claims, once, a bounded ordered inbox
    of `ApplicationDelivery` items: `Message(ApplicationMessage)`, only application data
    (`ApplicationData::{Custom, E2eHandshakeRequest, E2eHandshakeResponse, E2eStreamFrame}`),
    and `Event(SwarmEvent)`. No control message has a representation there. The application
    reads it with `ApplicationInbox::next`, or `try_next` without waiting.
  - Application delivery is lossy by design. Core never waits on the application, not even
    through a full inbox: a live application message that does not fit is dropped, counted
    (`ApplicationInbox::dropped`) and logged at `warn`, as a best-effort network drops it, and
    no inbound lane waits for the application to read. Nothing tells the sender: a protocol
    that needs a reliable stream must detect and recover loss end to end (#939). The bound,
    `APPLICATION_INBOX_WEIGHT` (64 MiB), weighs each message by its own size plus the byte
    length of its data, one rule for every kind, so empty messages are bounded too. Before the
    application claims the inbox, live messages are discarded and peer facts kept.
  - Relay-inbox mail is offered to the application only through a reservation of room in the
    inbox, and retired from storage once the application has taken it from the inbox. Taking
    ends core's custody: what the application does with the message afterwards is its own,
    and the node `Backend` may drop it beyond its own namespace queues, counted. Delivery is at
    least once within a run; a restart between the taking and the retirement loses the mail,
    as the replay window refuses its redelivery (#936). Mail that finds no room or no reader
    stays stored for a later pass, as does mail whose origin's quota is spent for now (that
    origin's later mail is skipped and other origins' still offered) and mail whose admission
    failed in the local replay store. A drain pass verifies only the elements it has not
    handed over yet and retires every element it is done with by one removal. An element
    whose witness fails for any reason but the clock gate is logged at `warn` and left
    stored.
  - `Swarm::send_message` and `Swarm::send_direct_message` take `ApplicationData` instead of
    any `Message`, so the application cannot originate core's protocol messages; `Message`
    implements `From<ApplicationData>`, the inverse of the inbound projection, and
    `ApplicationData::custom` builds a custom message.
  - `SwarmEvent` carries only logical facts, `PeerAdmitted`, `PeerRetired` and (#969)
    `MessageDropped`; `ConnectionStateChange`, `PeerTransition` and `peer_transition` are
    removed. Messages and facts share the inbox's one queue, in the order core produced them,
    and an unread `PeerAdmitted` is withdrawn by its `PeerRetired`. A connection's admission is
    announced in the critical section that activates it and its retirement in the one that
    removes it, so every admitted generation is announced both ways, in the order decided, and
    the per-peer delivery turns are removed. `Swarm::is_peer_admitted` reads the active record.
  - `Swarm::lookup_successor` returns `SuccessorLookup::Routed(SuccessorReport)`, a handle that
    resolves to the reported successor; a report no live handle awaits is dropped by core.
    `SuccessorReport::tx_id`, `FindSuccessorReport::is_application_lookup` and the unused
    `FindSuccessorReportHandler::CustomCallback` are removed.
  - Delegated admission is data: `Swarm::set_admission_delegation(prefix, MessageLimit)`
    registers an encoded namespace prefix as `Delegated` (or `Enforced` again), which core
    matches on admission; the node publishes each protocol's declaration when it installs the
    protocol.
  - Node: `Backend::serve(inbox, stop)` consumes the inbox until the swarm is gone or `stop` is
    requested; `Backend::handle` is removed. The backend dispatches each registered namespace on
    its own queue, bounded by 256 envelopes and by 16 MiB of encoded envelopes (an empty queue
    admits any one envelope), in arrival order, so one slow protocol holds back neither another
    namespace nor the peer facts. An envelope that cannot be decoded, names no registered
    namespace, finds its queue full or its namespace unserved is dropped, logged at `warn` and
    counted by reason: `Provider::dispatch_drop_counters` and the FFI request
    `dispatchDropCounters` read the counts. Facts and E2E messages reach the `BackendObserver`
    at once, so they may overtake custom messages still queued for a namespace. Register
    protocols before claiming the inbox: from the claim on, an envelope of a namespace not yet
    registered is dropped. `Provider::application_service(observer, stop)` claims the inbox and
    returns the future that serves it with the `Backend`; `Provider::set_backend` runs that
    future on a detached task, never stopped. `BackendObserver`'s methods have default bodies;
    it loses `lookup_report` (the bootstrap probe awaits its `SuccessorReport`) and gains
    `e2e_message`. The FFI provider serves the inbox with the extension `Backend` on its runtime
    until the handle is destroyed, and retains at most 4096 E2E events for its caller, dropping
    further ones, counted (`takeE2eEvents` reports the cumulative `dropped`).

- Refuse a delegation signature that is not its algorithm's exact length (#933). BIP-137
  recovery sliced the first 65 bytes of a peer-supplied signature and panicked on a shorter one;
  it now converts exactly. Every signer refuses any length but its algorithm's
  (`SignatureAlgorithm::signature_len`) by one gate: recovering a key from a recoverable
  signature (Secp256k1, EIP-191, BIP-137), and taking an explicit key from a signature, fail
  with the new error `InvalidSignatureLength`, while verifying a Secp256r1, Ed25519 or
  BLS12-381 signature returns `false`. So a verified delegation's signature is at most
  96 bytes.

- Remove the WebTransport-backed browser relay (#805). The browser `RelayHandle` with
  `register_wt_service` and `register_wt_udp_service`, its interpreter, and its session engine are
  gone, and the relay protocol and `extension::transport` now build for native targets only, where
  their one interpreter runs. Native TCP/UDP relay, WebRTC transport, and browser onion HTTPS are
  unchanged. Wasm builds no longer set `--cfg=web_sys_unstable_apis`, and `rings-node` drops the
  WebTransport and stream `web-sys` features.

- Rewrite SECURITY.md, README.md, ROADMAP.md, and llms.txt to be short and current: SECURITY.md
  adds private vulnerability reporting and states the Sybil boundary once (#780); README.md leads
  with what Rings offers, how to try it, and the pre-1.0 status; ROADMAP.md states the 1.0 and 2.0
  milestones; llms.txt gains a Status section. Install commands name the pinned toolchain with
  `--locked`, because the workspace denies warnings.

- Fail a bad transaction replay record closed on its own stream only, and restore the replay store
  once (#910). A record that does not restore (torn, corrupt, unreadable, misplaced, or holding an
  invalid window) used to leave the whole replay store unloaded: every later reservation and
  admission re-read the whole store (up to 32,768 records) under the replay lock and failed again,
  so the node could neither sign nor accept traffic. Now the store is restored from one scan and
  cached; each bad record is kept, logged once, and counted (`ReplayCounters::unrestorable_record`),
  and only the stream it is filed as refuses reservation and admission, with
  `Error::TransactionReplayStreamUnavailable { key, record }` (counted in
  `ReplayCounters::unavailable_stream`). Every other stream in the store runs normally; each bad
  record holds one slot of both tables' stream bounds. An operator clears one stream by removing the
  named record with the node stopped (see the replay chapter). An absent record still restarts its
  stream from `First` (#915), as does a stream whose record was moved or renamed away; the file it
  lands in is undecodable for its own name. Each reservation and admission now runs detached from
  its caller (`rings_runtime::run_detached`): a caller cancelled mid-transition no longer lets a
  stale record write land after a newer one, and its transition commits (its admission is charged
  and its sequence consumed) exactly as if the caller had dropped the verdict.

- Open the native transaction replay store as an authoritative `FileStorage` (#909). On unix its
  `put` flushes the temporary file before the rename and the directory after it (removals, and the
  store's directory and its ancestors at open, are flushed too), so a crash leaves each record whole
  at its previous or its new value; the write and removal plans are data the store interprets, and a
  model checks every crash point of them. A record file that cannot be read or is not the whole
  record of its own key is reported (`Error::StorageRecordUndecodable`, naming its file; a scan
  reports each record as `ScannedRecord::Filed` or `Undecodable`) and never deleted, so replay fails
  closed on it instead of reopening its stream. A disposable store now also retires a record holding
  another key's record on the read that finds it, instead of returning it from `get_all`. An
  authoritative store evicts nothing: a write beyond its budget, or an open under a lowered one,
  fails with `Error::StorageBudgetExhausted`. The flush bounds replay transitions to about 50 per
  second on an Apple M1 Max SSD (`F_FULLFSYNC`), against about 4,500 unflushed; group commit is
  tracked in #916. `FileStorage::new_with_cap_and_path` keeps the disposable behaviour (no flush,
  oldest records evicted, undecodable records retired) for the DHT, measurement, evidence and onion
  entry-guard stores (the entry-guard store's policy is #911);
  `FileStorage::new_with_cap_path_and_authority` opens a store of either `RecordAuthority`.

- `rings_runtime::run_blocking` (#919): fallible scheduling on the native blocking pool, with the
  ownership law of `run_detached` (the work starts at the call and outlives a dropped waiter);
  `Unavailable` outside a runtime, `Abandoned` when the work panicked or its runtime shut down
  first.

- Keep up to `OUTBOUND_LANE_WINDOW` (8) transfers in flight per outbound class lane instead of
  waiting for each delivery before the next (#899). On native, a delivery is the peer's SCTP
  SACK, which the receiver delays by up to 200 ms; a stop-and-wait lane paid that per message.
  On native loopback, 512 awaited 16 KiB sends go from 13.9 to about 1360 messages/s with no
  send over 150 ms. Each class lane is pinned to one data channel, so a class's frames arrive in
  lane order, and the window stays below the 32-sequence replay window. Transfers still start in
  FIFO order, chunked transfers stay contiguous, and each transfer keeps its capacity permit and
  delivery outcome. The window pipelines whole transfers: a chunked message still admits its
  next frame only once the previous one is delivered, so it pays the per-frame delayed-ACK tail
  and holds its lane's wire until its last frame is admitted. A frame's delivery deadline now
  measures a stall: it expires only after a full deadline with no confirmed delivery on the
  connection since its admission, so a slow link draining a full window never expires the frames
  queued behind the head.

- Confirm WebRTC delivery on the data channel's `bufferedamountlow`, `close` and `error`
  events instead of polling `bufferedAmount` every 300 ms (#887). Each channel multiplexes its
  single low-water threshold over every pending send, so an awaited send resolves on the event
  that flushes it: on native loopback the p50 of an awaited 16 KiB send drops from about 302 ms
  to about 5 ms. On native, "delivered" still means acknowledged by the peer's SCTP SACK. No
  wire or API change.

- Rename the relay transport identifier from `SessionId` to `RelaySessionId` to distinguish
  relayed TCP connections and UDP flows from delegated-signing identities. The wire value and
  frame encoding are unchanged.

- Bound outbound command ingestion to a permit-bounded batch and one coalesced
  cancellation scan. Control submitted behind bulk becomes visible before frame
  selection; shutdown releases the batch's permits before completion publication.

- Remove the counter-only measurement bridge and threshold-only classifier. Measure
  implementations must record attributed events and atomic batches; retained peer
  projections always contain credit. Existing measurement RPCs and provisional
  receipt evidence remain available.
- Remove unused IndexedDB visit/creation metadata and its index from new databases.
  Reads retain their atomic LRU touch. Existing databases keep their unused index
  without a schema upgrade or data deletion; raw transaction access is now private
  and construction requires a database name.

### Breaking changes

- Connect signaling returns along its recorded path, so crossing offers always end with an edge
  (#947, the root cause of the flaky gateway test in #945). A wire cutover:
  - `MessageRelay` gains the public field `path: Option<SignalingPath>`. `ConnectNodeSend`
    records the relays it crosses there; its `ConnectNodeReport` answer, and a counter-offer,
    retrace them. Inbound validation refuses a path on any other message, its absence on
    connect signaling, and a path that repeats a node.
  - Of two crossing offers, one still being created yields; of two sent ones the smaller DID's
    prevails and is sent back as a counter-offer. A DHT answer echoes its offer's transaction
    and applies only to that offer. `ConnectNodeReport` no longer honours `reply_via`.
  - `SwarmTransport::answer_remote_connection` returns an `OfferReply` of the answer or the
    counter-offer, and `Swarm::answer_offer` returns `OfferReply<MessagePayload,
    MessagePayload>`. The JSON-RPC `answerOffer` (and `rings connect answer`) may return a
    counter-offer: answer it with the caller's `answerOffer` and return that answer with the
    peer's `acceptAnswer`.
  - A WebRTC step of an attempt that lost its slot reports `ConnectionAttemptSuperseded` when the
    peer's handshake took it, which is what surfaced as `Local connection closed` from a creating
    offer, and the new `ConnectionAttemptCancelled` when anything else released it.
  - The envelope reserves account for the path: `MAX_CHUNK_ENVELOPE_OVERHEAD` is 912 (a chunk
    frame carries no path, one byte for its absence) and `MAX_PAYLOAD_ENVELOPE_OVERHEAD` is 3697,
    which covers the widest path an honest connect-signaling carrier records (a full hop budget
    of relays); a whole payload within about 2.7 KB of the data-channel limit is now chunked.

- `FileStorage`'s `KvStorageInterface<V>` (and its new `KvStorageScan<V>`) require `V: Send` as
  well as `Sync` (#909), since values cross to the blocking pool; `RecordAuthority` is
  `#[non_exhaustive]`.
- The native daemon's replay store flushes every write (#909), so the node-wide ceiling of replay
  transitions (every inbound admission and every sender reservation) falls from about 4,500 to about
  50 per second on an Apple M1 Max SSD (`F_FULLFSYNC`); group commit is tracked in #916.
- `ReplayCounters` gains the public fields `unrestorable_record` and `unavailable_stream` (#910); it
  is not `#[non_exhaustive]`, so a downstream struct literal or exhaustive pattern must add them.
- `ReplayStorage` boxes a `KvStorageScan<ReplayRecord>` (#910), a new trait over
  `KvStorageInterface` with two required methods: `scan` (every record as a `ScannedRecord`,
  `Filed` under its own key's name or `Undecodable` for the name it was found under; deleting
  nothing) and `record_name` (the name a key's record is filed under), bound by the agreement law
  on the trait. `MemStorage`, `FileStorage` and `IdbStorage` implement it; a custom replay storage
  passed to `SwarmBuilder::replay_storage` or `ProcessorBuilder::replay_storage` must implement it
  too.
- Every `FileStorage` operation, disposable stores' included, runs its file I/O on the runtime's
  blocking pool (#909, #919) and fails with `Error::StorageWorkUnscheduled` outside a Tokio
  runtime.

- Pin each outbound class lane to one data channel of a connection (#906). The transport pool
  selects a channel by lane, `channel(lane) = pool[lane mod |pool|]`, instead of rotating over
  the channels, so the messages of one lane reach the remote handler in send order and a
  receiver handler stalled on one channel holds only that channel's lane. Core sends DHT control
  (and link control), storage, end-to-end and application traffic on lanes 0 to 3.
  `ConnectionInterface::send_message_with_permit` takes a `ChannelLane`; `send_message` uses the
  default lane. `RoundRobin` and `RoundRobinPool` become `LanePool` and `ChannelPool`, `select`
  takes the lane, and `Error::RoundRobinPoolEmpty` becomes `Error::ChannelPoolEmpty`. No wire
  change.

- Key transaction replay by traffic class: `StreamKey = (network, origin, destination, class)`
  (#898). One stream shared by every class let DHT control traffic overtake an application
  backlog by more than the replay window, so honest transactions were rejected as
  `TransactionSequenceStale`. The class is implied by each transaction's signed message, so the
  wire format is unchanged, but replay is not interoperable across the upgrade: an upgraded
  sender's per-class sequences are stale to a node that has not upgraded. **This release is a
  mandatory network-wide upgrade, seeds included.** The replay store keeps one record per stream
  under `rings-core:transaction-replay:stream:{sender|receiver}:<key>` instead of one snapshot, so
  an admission writes only its stream's record (at most `TRANSACTION_REPLAY_RECORD_MAX_BYTES`,
  1161 bytes) rather than rewriting every stream; `ReplayStorage` stores the opaque
  `ReplayRecord`, which replaces `ReplaySnapshot`. The first load deletes the former
  `rings-core:transaction-replay` snapshot without decoding it, resetting every replay window
  once as deleting the store does; the deletion is best effort (a failure is counted and logged,
  admission continues, and the next load retries it). Each replay table now holds
  `TRANSACTION_REPLAY_STREAM_CAPACITY` = 4 × 4096 streams, every class stream of 4096
  account-destination pairs; a full store is at most `TRANSACTION_REPLAY_STORE_MAX_RECORDS`
  records and `TRANSACTION_REPLAY_STORE_MAX_BYTES` (about 27 MiB), and the native file store's
  budget (40 MiB) and the browser store's row capacity are sized so they never evict a replay
  record. `StreamKey::new` takes the class, `Transaction::stream_key` returns a `Result`,
  `Transaction::class` is new, and `PayloadSender`'s send and originate methods take a `Message`
  instead of any `Serialize` value, with `reserve_transaction_sequences` taking the class.

- Collect data-topic tombstones at a retention horizon and remove compaction (#867, #872, #874,
  #871). Each data-topic element now expires on its own, a value is kept only by writing it again,
  a removal wins over every earlier write of its value, and removals are collected by the clock
  instead of by a compaction. The rule is stated once, in the DHT chapter's
  [data-topic storage](docs/src/advanced-topic/chord.md#data-topic-storage) section, and in
  `dht::entry::retention`.
  - `Entry::retired_at` applies the rule on every read and stored value, a read serves that
    projection while it is live, and `SyncedEntryAck::confirms_local_value` takes the clock it
    compares at.
  - `EntryOperation::CompactData`, `Entry::compact_data` and `storage_compact_data` (core and
    node) are removed: a compaction floor erased concurrent adds its owner never received (#867).
    Both registries are bounded by the horizon alone (#871).
  - `EntryTombstone` holds a Keccak-256 `ElementDigest` and the greatest dot it covers, at most
    one per value (#874).
  - A carrier past its bound answers lookups as absent, from a replica and from a fetch cache,
    and a read that retires part of a stored carrier writes the projection back. The new
    `Swarm::storage_fetch_mark` and `Swarm::storage_fetch_answered_since` are the fetch's
    reply marker: a fetcher reads a mark before fetching and learns when its key was answered
    after it, which a projected cache value cannot show.
  - `PeerRing::storage` and `PeerRing::cache` are crate-private, so every read outside core goes
    through the projection (`Swarm` lookups, `inspect`).
  - Registry heartbeat intervals must be below 595 s; a node configured above it refuses to
    start (see [config.yaml](docs/src/advanced-topic/config.yaml.md#presence-and-onion-registries)).
  - `RecordAuthority` moves from `rings_core::storage::file` to `rings_core::storage`, one type
    whose shared law is the decode law. IndexedDB stores take one too:
    `IdbStorage::new_with_cap_and_name` is disposable and the new
    `IdbStorage::new_with_cap_name_and_authority` takes the authority; the browser replay store
    is authoritative, as on native (#909). An IndexedDB `scan` deletes nothing.
  - Cutover: the storage wire format changes, so a network upgrades together. It is
    incompatible with 0.32.x; this targets 0.33.0.
    - Native: an old carrier holding a tombstone no longer decodes and is retired on first
      read. One without still decodes, its elements read as the bytes of their old base58
      text. A data topic serves those as values until the element horizon, `lookupService` and
      `fetchTopicMessages` included. A relay inbox's held messages fail its witness: they are
      never delivered, and their carrier is never accepted by a hand-off. Core keeps no code
      for stores from before the cutover, so **operators clear each native node's DHT data
      store before upgrading**: the `data_storage` path of the configuration, or the `data`
      directory under `rings run --storage-path` when that flag overrides it. Data topics and
      registries are repopulated by their next writes, and every message held at the upgrade
      is lost. The same clear is due on every later change of the codec of `HeldMessage`,
      `MessagePayload` or `Message`: core keeps no compatibility code, and a held message
      under another codec fails its witness for good.
    - Browser: every non-empty old carrier is refused and retired on first read.
    - A storage hand-off now skips an entry the receiver cannot admit, without acknowledging
      it, instead of rejecting its batch; only a placement outside the entry's replica set
      rejects the batch. So an old carrier holds back no other entry.

- Make receive-side admission lossless with per-lane credit flow control in the transport (#924).
  The transport's node-wide raw-frame gate (256 frames, blind to traffic class) silently dropped
  frames once full, so a storage burst could drop liveness probes and falsely disconnect healthy
  peers (38 of 49 probes in the N=50 hotspot sync storm). Now each of a connection's four lanes is
  credit flow controlled: no honest frame is refused, overload reaches the sender as
  backpressure, and both waits (a sender's for credit, an arrival's for the mailbox) are logged
  at `warn`. Liveness judges peers by probes alone, and an inbound lane's progress depends only
  on local resources, so a peer that withholds credit is evicted and costs no other link. The
  node-wide receive bound is soft; a hard one needs credit that can be taken back (#934). The
  laws, deadlines and bounds are stated once, in
  [Transport flow control](docs/src/advanced-topic/transport-flow-control.md). Breaking changes:
  - Wire: the new `TransportMessage::Credit(u64)` carries a lane's cumulative credit. A frame
    beyond the advertised credit is refused and reported as invalid, as is a frame whose content
    belongs to another lane than the one it arrived on (`AdmittedInboundMessage::lane`,
    `Error::InboundLaneMismatch`; a chunked message's chunks must share its lane, and
    `MessageCategory::channel_lane` is the one category-to-lane mapping). A remote-created data
    channel is admitted only by its lane label, once per lane.
  - Transport: `ConnectionInterface::reserve_send_credit`, `SendPermit::with_credit` and
    `SendCreditWait` let a caller wait for credit apart from a send; a wait fails with
    `LinkCreditClosed` when the connection generation ends, a send carrying another connection's
    credit with `ForeignCredit`, and a native connection that cannot spawn its credit pump with
    `CreditPumpUnavailable`. `LANE_CREDIT_WINDOW`, `NodeReceiveLoad` and
    `ChannelLane::PRIORITY` are new. `TransportMessage` is no longer public, so only the
    transport builds a credit frame: `ConnectionInterface::send_message` and
    `send_message_with_permit` take the payload `Bytes`, and `custom_frame_wire` encodes the
    frame a custom send puts on the channel, for callers that size or replay frames.
    `InboundFrameCapacity`, its
    node-wide bounds and `TransportInterface::inbound_frame_capacity` are removed;
    `InboundFrameAdmission::CapacityExceeded` becomes `CreditExceeded` and gains a `Credit`
    variant; `admit_inbound_frame` and `prepare_inbound_frame` take the frame's lane;
    `InnerTransportCallback::for_transport` becomes `InnerTransportCallback::new`, taking the
    `NodeReceiveLoad`; `InboundFrameCapacityLease` becomes `InboundCreditLease`.
  - Core: `PEER_LIVENESS_IDLE_MS` is renamed `PEER_LIVENESS_PERIOD_MS`. `SwarmTransport` is no
    `PayloadSender`; every sender is an `Egress` of one discipline, and the waiting
    (application) egress is constructible only inside `crate::swarm` (the Swarm API, including
    the storage API, which moves to `swarm::storage_api`).
    `SwarmTransport::send_storage_sync_or_defer` sends under the egress it is given. A send a
    handler or maintenance makes is dropped as local backpressure when its connection is not
    ready, it lacks outbound capacity, or its first frame waits past its deadline; a send
    through the Swarm API still returns once its first frame is admitted. `fix_fingers` and
    `correct_stabilize` now queue their queries, and a connection a handler starts
    (`connect_detached`) or answers is negotiated and sent on a detached task.
  - Tests: the sync-storm scenarios derive their backlogs from the credit law, the
    barrier-exemption ablation no longer violates its proposition (#927 evaluates removing the
    layer), and new boundary and effect witnesses cover a saturated lane and a peer withholding
    credit from a relay.
  - Cutover: a wire change and part of the storage-entry cutover above: incompatible with
    0.32.x, targeting 0.33.0.

- Store and send data-topic and relay-inbox elements as bytes, not base58-check text (#926).
  `Entry::data` is `Vec<Bytes>`; an element is the application's own bytes, and its
  `ElementDigest` is the Keccak-256 of those bytes. A held relay message is its wire encoding
  (`HeldMessage::to_element`), and an online-node or onion-exit descriptor its codec encoding
  (the node-internal `RegistryElement`). The `Encoder`/`Decoder` impls of `OnlineNodeDescriptor`
  and `OnionExitDescriptor` are removed. `storage_append_data` and `storage_tombstone_data`
  (core `ChordStorageInterface` and node `Processor`) take `Bytes`, and the entry constructor
  `(String, Encoded)` becomes `(String, Bytes)`. `ENTRY_PAYLOAD_MAX_BYTES` now bounds element
  bytes. JSON-RPC `publishMessageToTopic`, `fetchTopicMessages` and `lookupService` still
  exchange strings, now as the elements' UTF-8. The inspect `StorageValue.data` lists each
  element as base64 instead of its base58-check text. The browser
  `Provider.storage_check_cache` returns the whole serialized `Entry`, so its `data` elements
  are now byte arrays instead of base58 strings, and each of its removes (`crdt.tombstones`),
  formerly a bare dot, is an `EntryTombstone` of the removed element's `ElementDigest` and its
  dot. What an element written before this change becomes on each backend is stated in the
  storage-entry wire cutover above.

- Derive the chunk envelope reserve from the widest frame (#925). `MAX_CHUNK_ENVELOPE_OVERHEAD`
  drops from a 4096-byte guess to 911 bytes, and `TRANSPORT_CUSTOM_OVERHEAD` from 64 to 4: each
  is the supremum of what the chunk framer emits, with every field it leaves free at its widest
  (both delegation slots inline with the widest account and signature, every varint at its
  maximum), witnessed with equality through the framer itself. The new
  `MAX_PAYLOAD_ENVELOPE_OVERHEAD` (941) bounds a whole payload's envelope, `reply_via` and the
  relay aim included, and is the storage hand-off batch's headroom. At a negotiated 8 KiB limit,
  a chunk now carries about 7.3 KB of data instead of 4032 bytes. The wire format is unchanged.

- Add delegated admission of direct-edge application traffic (#888). A namespace declares it
  through the new `Protocol::delegates_admission` (node) or `SwarmCallback::delegates_admission`
  (core), both defaulting to `false`. For its traffic from the authenticated neighbour that
  originated it, core skips the per-origin message-count limit and charges at least
  `DELEGATED_MIN_CHARGE` (16 KiB) against the same per-origin byte bucket, whose limits and
  record bound still apply. Relayed and foreign-origin traffic keeps the enforced limits.
  `OriginQuota::admit` takes a new `MessageLimit` argument, and `MessageLimit` and
  `DELEGATED_MIN_CHARGE` are exported from `rings_core::message`. No shipped protocol declares
  delegation yet.

- Deliver messages toward a node DID by a dedicated rule instead of the owner lookup (#865, #873).
  Greedy hops forward to the linked known peer, successors included, that lies nearest the
  destination, and never pass it. A hop that knows no such peer hands the message once to the
  first linked node after it, which routes on greedily but refuses a second crossing, ending the
  route with `RelayDestinationUnreachable`. This ends the cycles that dropped replies with
  `RelayHopBudgetExhausted` on unconverged rings: a route takes at most `2|V|` hops. Whenever
  a node has no predecessor, its requests carry a signed `Transaction::reply_via` naming its
  nearest linked successor, and successor and connection answers to it return through that peer, so joins
  work under fail-fast. Every locally originated request is built by
  `PayloadSender::originate`. The owner lookup `find_successor` is unchanged. The relay carrier
  gains a `stage` field and the transaction hash covers `reply_via`, so the wire format is
  incompatible with 0.31.x; this targets 0.32.0.

- Order IndexedDB LRU eviction by a store-wide logical access clock instead of wall-clock
  milliseconds (#853). Rows touched within one timer tick could tie and evict a recently
  read row. Rows now store `access_stamp`; the database schema moves to version 2 and a
  version-1 database is migrated in place on first open, keeping every row and its former
  eviction order. Older builds cannot evict from a migrated database.

- Enforce IndexedDB row capacity when a database opens. Reopening an existing store with a
  lower capacity now deletes the least recently accessed excess rows, which can remove
  persisted data that older versions retained until later writes. This storage behavior
  change targets 0.31.0.

- Consolidate native/browser runtime adaptation into the new `rings-runtime` crate (#811):
  one fallible `sleep` (sub-millisecond durations round up, browser timers chain across the
  `setTimeout` ceiling instead of clamping, durations of `UNBOUNDED_SLEEP` or more never
  resolve), a `Spawner` whose absence is reported before any work starts, detached
  `spawn_detached`/`run_detached` contracts, and the `MaybeSend`/`MaybeSendSync` bounds.
  Removed without aliases: `rings_core::utils::js_utils` (use `rings_runtime::sleep`),
  `rings_transport::js_global` (now `rings_runtime::global`), `rings_transport::PlatformSendSync`
  and `rings_node::extension::ext::MaybeSend` (both `rings_runtime::MaybeSendSync`). Node error
  code 1503 is now `DetachedTask`, with new codes 1504 `RuntimeUnavailable` and 1505 `Timer`;
  relay and onion work refuses with these errors instead of panicking when no Tokio runtime is
  current (measurement construction already refused with a typed error).
  `rings_node::prelude` re-exports `rings_runtime`.

- Finish the runtime consolidation (#829). `Notifier::set_timeout`/`set_timeout_ms` are
  removed; `Notifier::notified_within(timeout)` races the notifier against
  `rings_runtime::sleep` in place, so no timer task is spawned, no native path panics without
  a runtime, and a timeout no longer wakes a shared notifier. The dedicated notifier timer
  thread is gone. Trait-object aliases that were split per target (`SharedSwarmCallback`,
  `EntryStorage`, `ReplayStorage`, `CallbackError`, `MeasureImpl`, `MeasureStorage`,
  `EvidenceStorage`, `OnionEntryGuardStorage`, `BoxedTransportCallback`, …) are now written
  once through `rings_runtime::maybe_send_sync!`/`maybe_send!`; their types are unchanged.
  `rings_runtime::global` is private, the browser delivery poll stops on a timer failure
  instead of spinning, and `rings_transport::Error` gains `Timer`.

- Replace session-key terminology in the delegated-signing API with `Delegation`,
  `DelegateeKey`, `DelegationBuilder`, and `DelegationDigest` under
  `rings_core::delegation`. Rename the related DIDs, authorization accessors,
  message-proof fields, WASM exports, node configuration key, and CLI command to
  `delegator`/`delegatee`/`delegation` names. There are no compatibility aliases;
  callers must update imports and persisted configuration/key data. Transport
  `SessionId` remains the connection identifier. This cutover targets 0.30.0.

- Subtraction round, `rings-node` section (#787), targeting protocol cutover 0.29.0. Onion-exit
  descriptors now sign one canonical service name instead of a `services` list containing a
  second transport axis; the unused transport enum, descriptor and entry-guard schema numbers,
  and `buildOnionRoute` RPC are removed. HTTPS remains a reserved service handled by the native
  TCP exit runtime. Node configuration is total-cutover too: `session_sk` is a file path, the raw
  session fallback and `ecdsa_key`/`session_manager` fields are rejected, and semantic role and
  heartbeat validation runs once on the final processor builder state. Mixed 0.28/0.29 onion
  descriptors are not supported.

- Subtraction round, DHT section (#787). Pre-CorrectChord defaults that #604 replaced are
  removed: the `NotifyPredecessorReport` reply (message variant 5; later wire indices shift down
  by one, a total cutover), the periodic predecessor notify to every successor (the stabilize
  step notifies the head, as the paper specifies), and the `CorrectChord`/`LiveDid`/`TryConnect`
  trait shell whose operations live on as `admit_connected`, `TopologyEvent::Notify` and
  `begin_stabilization`. `EntryOperation::Touch` is folded into `Extend` (a lattice join since
  #628): `storage_touch_data` is gone and callers use `storage_append_data`. The const-generic
  `ChordStorage<_, REDUNDANT>` and `ChordStorageInterface<REDUNDANT>` become the plain
  `ChordStorageInterface` over the transport's configured redundancy; the API mismatch error is
  no longer reachable from it. Discovery of an unknown closer predecessor now waits for a
  `TopoInfo` query/reply and connection admission instead of an immediate notify reply; the
  removal does not preserve the old discovery latency. Replica placement uses `PlacedEntry`
  with a separate storage key and unchanged resource identity; the obsolete `Entry::affine`,
  `clone_with_did`, single-string conversion, and duplicate storage-head wrapper are removed.

### Fixed

- Preserve cancellation commands that arrive while the outbound worker is cancelling queued
  transfers (#789). Cancellation scans no longer drain the mailbox themselves, so a stopped
  successor releases its capacity without waiting for its lane head to finish delivery.

### Removed

- Subtraction round, `rings-node` implementation residue (#787): the extension dispatch
  re-injection carrier; extension-mutated online capability storage; unreachable onion-exit
  failures; duplicate route-selection wrappers and constant filters; the `/ws` sink and book
  page; custom registration-task and old publisher surfaces; relay constructor service maps;
  polling-based registration stop; duplicate engine replacement/cancellation; node gzip helpers;
  unused timeout, gateway, provider, onion constructor, and circuit-security APIs; and the
  `X-NODE-VERSION` response header. TCP and HTTPS exit adapters now share one forward-replay
  witness, and duplicate engine opens preserve the already committed resource.

- Subtraction round, swarm section (#787). The send path no longer retires a peer whose data
  channel does not open within its wait: an admitted connection that is not ready is a
  transiently disconnected transport, and stabilization owns its retirement (the disconnected
  grace, then the liveness probe); the send fails and is retried by its caller. The outbound
  worker's second cancellation carrier (`cancel_requested`) and its two-lane command mailbox
  with a per-drain budget are gone: submission permits bound each FIFO drain to 256 transfers,
  and a separate single-slot cancellation channel coalesces repeated scan requests. The worker
  collects that batch before releasing permits, then makes its control transfers visible before
  selecting a frame; submissions racing the empty read enter the next iteration. With runnable
  lower-class work, the transfer queues yield after at most four control attempts and rotate
  lower-class service. Cancellation and shutdown release permits before publishing completion.
  The inbound ticket chain that serialised same-lane frames between core admission and the
  transport-lease release is gone:
  lane order is the sequence-numbered `Pending`/`Ready` protocol alone, and a lane's frames
  decode in parallel. Also removed: the duplicate data-lane check inside
  `ReassemblyHandoffBarrier::blocks` (the actor's barrier sequence carries it), the
  `OriginQuotaLane` enum (the quota lane is the public `MessageCategory`), the tracked/detached
  storage-sync outcome twins (one `StorageSyncOutcome`), `Swarm::connected_peer_dids`
  (identical to `peer_dids`), the cross-profile inequalities in the transport timeout profile,
  `TRANSPORT_MTU` (the frame size is negotiated per connection since #601; `TRANSPORT_MAX_SIZE`
  is the literal 60 MB ceiling), `BACKEND_MTU`, and the second storage-lookup eviction loop.

- Subtraction round, message / ecc / utils section and `rings-derive` (#787). Forty-six
  `rings_core::error::Error` variants that nothing constructs since the transport split (#471),
  the report-return removal (#745) and the base58 `Encoder` (#122), with the `url`, `flate2` and
  `webrtc` dependencies that existed only for them; the gzip helpers on the payload module; the
  ElGamal string adapter (`encrypt(&str)`, `decrypt -> String`, `str_to_field`, `Plaintext`,
  `MessagePoints`), leaving the byte and AEAD API that #608 required; the `poll!` macro,
  `js_utils::spawn_interval` and the `js_func::of!` wrappers; the core copy of the JavaScript
  global-scope detection, now `rings_transport::js_global` for both crates; the eager
  `ChunkList` (`Chunk::stream` is the one framing function; `Chunk::from_wire` stays for #741);
  the `Then` trait; the `impl_dht_protocol_advertisement!` macro and the three flattened fields
  on `ConnectNodeSend`/`ConnectNodeReport`, which now embed `DhtProtocolMode` (the codec writes
  a struct as its fields in order, so the wire bytes are unchanged; a test pins this);
  `Transaction::origin` delegates to `MessageVerificationExt::signer`; the hop-budget argument
  of `MessageRelay::report` (a report is a fresh carrier at `HopBudget::MAX`),
  `TryFrom<Vec<u8>> for Encoded`, `Encoder for &str`, `PublicKey::try_from_b58m_uncheck`,
  `SessionSkBuilder::validate_account` and `ServiceReceiptDigest::new`; and the
  `JudgeConnection` derive in `rings-derive`, whose trait #561 removed.

- `rings_core::dht`: the fresh-connection storage repair grace (closed by the #745 pre-admission
  hold), `RelayMessageSet` (`DataTopicBuffer` carries the tombstone set),
  `peer_may_share_storage_responsibility` (the removal transition itself reports whether the
  peer was referenced, as `TopologyRemoval`, and only that outcome requests the repair round;
  the pre-removal read is gone), the `SuccessorReader` trait and `SortRing` (the successor
  list is the committed value of `topology::step`), the dead `Did`/`BiasId` algebra (only
  `Did::cmp_from_observer` survives), the mutable `StorageVirtualNodes` registry API,
  `FingerTable::closest_predecessor`, and the #770 residue (`FingerAttemptStatus`, the twin
  claim phases and connection plans, unread outcome payloads, restore-width normalisation and
  serde on finger convergence state). One `ClaimPhase` and one `ConnectionPlan` serve both the
  stabilization and successor-sync token machines.

## 0.28.0

### Breaking changes

- A link sends each session delegation once and references it afterwards (#738). The payload
  wire encoding carries, in each of its two session slots, either the `Session` inline or its
  20-byte content address (`SessionDigest`, the trailing bytes of `keccak256` over the encoded
  session). Payload frames begin with the `RINGS-PAYLOAD` marker and link-control frames with
  `RINGS-LINK`; every 0.27.x frame fails closed before decoding. Mixed-version overlays are
  unsupported.
- The protocol is not versioned before 1.0. The version suffixes on wire markers (payload,
  provisional service claim and receipt), signing
  domains (`rings-core:message-verification:transaction`, `...:payload`, the inbox, receipt,
  descriptor and onion domains), AEAD namespaces, HKDF labels and storage keys
  (`rings-core:transaction-replay`, `rings-node:onion-entry-guards`) are removed: each name is
  one domain, and a change to it is a total cutover. Every signature domain therefore changes in
  this release; a replay window, entry-guard set, measurement ledger or provisional-evidence
  store persisted by 0.27.x is not read (the node's `MeasurementLedger` and
  `ProvisionalEvidence` storage keys lose their suffix too).

  The reference protocol, in detail:
  - A steady-state relayed payload carries two 20-byte digests and two 65-byte signatures of
    identity material, where it carried two full delegations.
  - References are scoped to one direction of one admitted connection generation. The sender
    references a session only after the receiver confirmed it with an unsigned `Known` frame;
    the receiver confirms every inline session it verified, so loss or reordering on the link
    costs inline frames and never a stall. The link is treated as a datagram link: nothing
    relies on ordered or reliable delivery. Both tables are scoped to the connection
    generation, so a restart needs no resynchronisation.
  - A forwarding hop re-encodes the origin slot for its own next link, so a destination that
    never met the origin receives the origin's session inline from its last hop. No node asks
    the origin for a session.
  - The receiver's table holds 128 sessions to the sender's 64 under one least-recently-
    referenced order over the frames both ends saw, so on a lossless link the sender goes
    back to inline before the receiver could have forgotten. A reference the receiver cannot
    resolve holds that frame alone, at most 16 per connection and for at most twice the
    transport's delivery timeout plus one period of the inbound actor's periodic sweep, while
    the link repairs the miss with the unsigned link-control frames (`RINGS-LINK`): one
    request per missing session of a held frame, answered by exactly one announcement or
    disclaimer. Link-control frames are emitted on the connection generation they were judged
    on, in a task of their own, straight on the data channel rather than through the transfer
    lanes, at most 128 in flight to one peer (twice the transport's per-peer raw-frame bound).
    Frames that resolve are never queued behind held ones; a frame the peer was asked about
    and did not back (disclaimed or invalid announcement, hold timeout, failure on release) is
    charged to the peer as a receive failure, and a frame that finds the hold full, or whose
    question was never sent, is dropped uncharged. A reference resolves only on the link a
    connection's callback is bound to; a frame from any other peer is judged self-contained,
    and a frame released after its generation was superseded is dropped.
  - An expired session is evicted from both tables; a reference to it is a miss, and
    re-announcing the expired delegation is refused as it is inline.
  - `MessagePayload::to_wire`/`from_wire` remain the self-contained encoding (both sessions
    inline), used for handshake payloads, chunked payloads, and inbox carriers; a referenced
    session there is `Error::SessionReferenceUnresolved`.

### Added

- `Session::digest` and `SessionDigest` in `rings_core`: the content address of one exact
  delegation.

## 0.27.0

### Added

- `rings run` owns a set of managed bootstrap targets for the life of the process. The new
  `bootstrap.peers` config section (written empty by `rings init`) and `rings run --bootstrap-seed
  <url>` name them in the seed-document shape; `rings connect node|seed` keep their one-shot
  semantics.
  - Reachability is a routed successor lookup for the target's DID. A target whose admission
    was announced, ready or recovering, is left to the swarm; a target other peers can route to
    is never forced into a direct edge; a key in the local successor interval is refuted
    without a probe.
  - An unreachable target is redialed through its HTTP endpoint five times, each attempt at
    least two seconds after the previous one settled, then every five minutes plus up to thirty
    seconds of jitter.
  - A redial succeeds only once the swarm admits the peer. An endpoint answering as a different
    DID is refused before any offer is created. A redial refused because the target's slot is
    owned by another attempt (a handshake in flight, whichever side started it, or an admission
    not yet announced) is not an error, but consumes a turn of the burst like a failed dial,
    so a handshake the peer keeps in flight cannot hold the target in the rapid cadence. A
    failed exchange or an admission timeout cancels the generation the redial reserved, and
    only while it is still pending.
  - A target leaving the local DHT, whatever caused it, triggers an immediate reassessment and
    restarts the burst. At most one handshake per target is in flight, targets retry
    independently, and shutdown cancels any handshake in progress.
  - Invalid targets stop `rings run` before it listens: an unparsable DID, a URL failing the
    remote endpoint policy, one DID listed with conflicting entries, one endpoint listed under
    two DIDs (`SeedPeerError`, error 820). An entry repeated verbatim is merged, and an entry
    naming the node itself is skipped, as `connect seed` skips it.
- `SwarmEvent::PeerRetired` reports the retirement of an admitted connection whose admission
  was announced as `ConnectionStateChange { Connected }`, whichever path retired it (remote
  terminal state, data-channel close, liveness or stabilization removal, eviction, explicit
  disconnect), through the same ordered per-peer delivery and before any terminal state event.
  A topology prune that keeps the record emits nothing.
- `Swarm::offer_connection`, `Swarm::accept_answer_for` and `Swarm::cancel_connection_attempt`
  run a handshake pinned to the connection generation the offer reserved, so an answer is never
  applied to, and a cancellation never hits, a handshake the peer started meanwhile; the
  cancellation releases the generation only while it is still pending. `Swarm::is_peer_admitted`
  (an admission announced to the application, whose retirement is therefore promised) and
  `Swarm::has_unadmitted_connection` expose admission and handshake facts;
  `Swarm::lookup_successor` takes the local step of a successor lookup and routes it, through
  the hop that step chose, when the local topology cannot decide; `SwarmEvent::peer_transition`
  reads the admission and the retirement off the event stream as one `PeerTransition`.
- `RemoteRpcEndpoint` is the validated form of a remote node's public RPC URL; native clients
  are built from it with a request and resolution timeout. `SeedPeerError` (error 820) is the
  typed refusal of a seed entry or of an ambiguous seed set, shared by `connect seed` and the
  managed targets. `HandshakeInFlight` (error 819) reports a handshake refused because the
  peer's slot is owned by another attempt.
- `Processor::connect_peer_via_http` performs the HTTP handshake with the answering DID
  optionally pinned, refuses a pinned handshake the local core already holds, and returns the
  connection generation it reserved; the JSON-RPC handlers delegate to it.
- The node `Backend` accepts a `BackendObserver` that receives application lookup reports from
  its single payload decode, and admissions and retirements from its swarm events.

### Changed

- `rings connect seed` validates the document as a set before dialing any entry (a verbatim
  repeat is dialed once, conflicting entries are refused), skips peers that are admitted,
  treats a handshake the local core finds in flight as the peer being taken care of rather than
  as a failure, and pins each handshake to the entry's DID, so an endpoint answering as another
  node is refused before an offer is created.

## 0.26.0

### Breaking changes

- Finger-table lookup reports now carry a fresh UUID correlation token instead of a bare slot
  index, and topology query requests/reports carry a mandatory UUID correlation token. The wire
  format is incompatible with 0.25.x, so every node in an overlay must upgrade together.
- `FingerTable` equality now covers its complete serialized maintenance and convergence state.
  Callers that need hint-only equality should compare `FingerTable::list()` instead.
- `CorrectChord::stabilize` is removed. A successor's topology report changes the successor list
  only when it echoes the correlation token issued by `pre_stabilize`; there is no token-less path
  by which an arbitrary `TopoInfo` can refine successors.
- `FingerTable::set`, `remove`, and `set_fix` are no longer public. Finger hints change only
  through the topology transition, which is the single owner of the hint-join and removal laws;
  the in-place mutators remain as test fixtures.
- `FingerTable::fix_finger_index` and the `fix_finger_index` field of `TopologyState` are
  removed, and `TopologyState::new` no longer takes a cursor. The finger table has one cursor,
  owned by the convergence state, at which both lookup selection and periodic revalidation resume.

### Added

- Finger-table convergence distinguishes inferred hints from verified ranges. One lookup proves
  every consecutive slot owned by the reported successor; topology changes invalidate only the
  affected slots, losing the last successor invalidates all membership evidence, and correlated
  request epochs reject stale or expired in-flight results at commit time. Lookups are issued for
  unverified ranges in cyclic order, resuming after the range the previous attempt proved or
  failed, so a range whose successor never completes a handshake costs one attempt per rotation
  instead of starving every range above it, and periodic revalidation is deferred by pending work
  only while that work is succeeding.
- Automatic convergence permits one lookup per node at a time, spreads simultaneous fleet starts
  over a node-lifecycle-randomized 10-second phase window, reuses that phase across repeated browser
  listener restarts, rephases deadlines left stale by browser suspension, and has no catch-up
  bursts. Send or handshake failure, invalid reports, lookup timeout, and admission-lease expiry use a
  2/4/8/16/32/60-second exponential retry floor plus a full jitter window; only an applied range
  proof resets the failure level. Normal topology churn invalidates stale evidence without being
  counted as a network failure. Timely lookup proofs may wait up to 210 seconds for transport
  admission (longer than the 180-second handshake generation that owns them once reserved), after
  which the lease is released into the same bounded retry schedule. A due
  convergence turn may yield to at most two topology or storage phases before it is reserved.
- Stabilization and successor-list reports are bound to one current reporter and UUID and atomically
  claimed before connection effects. A claim is released on every exit path of its handler, a new
  round does not supersede a report that is still being processed, and a claimed report keeps its
  remaining candidate budget while its reporter stays a successor (its own admissions extend the
  list without revoking it). Candidates are deduplicated and ordered by transport quality;
  successor-list sync is capped by successor capacity, while stabilization may additionally admit
  one predecessor. Browser listener generations are serialized so rapid `stop`/`listen` cycles
  cannot run duplicate maintenance daemons.
- Release metadata for workspace crates, examples, the npm package, and lockfiles now records
  `0.26.0` as one protocol generation so path consumers do not describe a mixed 0.25/0.26 tree.

### Security

- Updated `rustls` to the RUSTSEC-2026-0285 fixed release and documented the cooldown allowlist
  entry as a security exception rather than a dependency pin.

## 0.25.0

### Breaking changes

- The timestamp-only peer-liveness request/report wire is replaced by the three-step `Probe`
  request, offer, and acknowledgement transcript. There is no fallback or dual decoder, so every
  node in an overlay must upgrade together.

### Added

- Providers can retain jointly signed provisional service receipts with bounded durable replay
  markers. Admission validates both roles, the embedded transactions, epoch and session liveness,
  and commits the receipt and replay marker before reporting success.
- Native and browser evidence stores restore only records bound to the configured overlay and local
  provider. Executable protocol and persistence models cover bounded network schedules, capacity,
  eviction, restart, and persistence failure.

## 0.24.0

### Breaking changes

- Every signed `Transaction` now carries a mandatory destination-scoped `u64` sequence and uses
  the `rings-core:message-verification:transaction:v2` signature domain. Payload wire bytes carry
  an explicit `RINGS-TX-V2` marker; 0.23.x payloads fail closed before decoding. Mixed-version
  overlays are unsupported.

### Added

- Final destinations persist a fixed 32-slot replay window per
  `(network_id, origin_account_did, destination_did)` before application validation and dispatch.
  Duplicate, fork, and stale sequence verdicts are typed and observable; gaps and bounded late
  delivery are admitted. Sender allocators persist reservations before signing, session rotation
  preserves the account stream, relays remain stateless, and both native and browser runtimes use
  dedicated durable replay stores without LRU eviction.
- Final destinations enforce runtime-local message and byte token buckets per verified origin,
  destination, overlay, and logical lane. Replay and quota decisions commit atomically before
  logical mailbox admission; chunked transactions are charged once after reassembly, relay paths
  share the origin allowance, table pressure reuses only fully replenished idle records, and
  aggregate drop counters avoid origin labels.

## 0.23.0

### Breaking changes

- The relay carrier no longer records the route. `MessageRelay` is `{ next_hop, destination,
  hop_budget }`: every forward spends one unit of the budget and a payload whose budget is
  spent is dropped with `RelayHopBudgetExhausted`, which replaces the history-based loop
  detection. A fresh budget is `MAX_RELAY_HOPS` (64), and decoding rejects a carrier that
  claims more.
- A verified message that arrives on a connection before this end has admitted it is held and
  delivered, in arrival order, once admission commits; it used to be dropped.
- Reports are always Chord-routed to the transaction origin. `ReportReturnPolicy` and the
  `report_return` field of `Transaction` are removed, together with the `*_with_report_return`
  senders; the transaction hash covers `destination || tx_id || data`.
- The relay-derived origin accessors (`MessageRelay::origin_sender`, `try_origin_sender`,
  `sender`) are removed. Handlers take the origin from `Transaction::origin`, the account
  behind the signing session.
- Wire format and transaction hash are incompatible with 0.22.x; every node in an overlay must
  upgrade together.

## 0.22.0

### Breaking changes

- The license is now the GNU Affero General Public License, version 3.0 only (AGPL-3.0-only)
  for every crate, the frontend, and the npm package. Works derived from Rings, and services
  that offer it over a network, must publish their source; a commercial license for use
  outside those terms is available from Rings Network on request. The name, logo, and the
  hosted rings.rs service are not covered by the software license.

### Added

- Frontend: a guide page (`#guide`) with one card per runtime (native, browser, extension, C
  FFI), the first commands for each, and links into the matching book chapters; a site footer
  on the landing and guide pages carrying the license, commercial-use, warranty, security, and
  privacy declarations (hidden under the mobile breakpoint, where the bottom navigation bar
  owns the space); the header is Home, Guide, Docs, and Whitepaper, since the node console and
  GitHub are entered from the landing page.
- Frontend: the browser node's seed inputs (the Seed URL setting and the link dialog's HTTP
  endpoint) default to the public seed `https://node.rings.rs` and suggest it beside the local
  daemon endpoint.
- Documentation: the book is restyled to the site's palette with a site bar back to rings.rs,
  and gains chapters on operating a node from the CLI, the browser node and extension, the C
  FFI, and `llms.txt`; the install chapter covers the prebuilt release binaries.
- `llms.txt` at the repository root, published at `https://rings.rs/llms.txt`: a map of the
  project for coding agents (authoritative documents, crate layering, build and test commands,
  conventions).

## 0.21.2

### Breaking changes

- A `gateway:` section that omits `enabled` is now disabled. Earlier builds started the native
  TUN gateway whenever the section was present; a hand-written section from one of them needs
  `enabled: true` to keep starting its gateway. `rings run --gateway` still enables the section
  for one run.

### Added

- `rings init` writes a complete `gateway:` section with `enabled: false` and every field stated,
  so plain `rings run` behaves as before while `rings run --gateway` works on the generated file
  without editing. The plan and its defaults come from the gateway crate:
  `GatewayPlan::interface_only` (one RFC 6598 host address, no capture routes,
  `Mtu::IPV6_MINIMUM`) and `GatewayConfig::with_default_limits`; the node composes them in
  `NativeGatewayConfig::disabled_default` and selects a runner through `Config::enabled_gateway`.
- `rings run --gateway` on a config without the section now names `rings init` and prints the
  exact section to append.

## 0.21.0

### Breaking changes

- Message, transaction, and descriptor signatures now sign a domain-separated transcript that
  binds the signer's `network_id` and a per-message-family tag. Signatures issued by earlier
  builds no longer verify, and a signature issued inside one overlay does not verify inside
  another. Every signature is issued by a `MessageSigner` (a session key acting inside one
  overlay; `MessageSigner<&SessionSk>` borrowed, `MessageSigner<SessionSk>` owned) and verified
  under the receiver's overlay: `Transaction`, `MessagePayload`, `PayloadSender`, and the node
  descriptors take a `MessageSigner`, `MessageVerificationExt::verify` and the descriptor
  `verify_signature` / `is_live_at` / `latest_valid_by_*` take the receiver's `network_id`, and
  signing a descriptor whose body states another overlay is refused. Domain tags are built with
  `domain_tag!`.
- DHT entries carry a retention bound (`expires_at_ms`) stamped at the operation boundary with
  `DEFAULT_TTL_MS` and bounded at admission by `MAX_TTL_MS`. Stored values without a bound, or
  whose bound has elapsed, are retired on their next read and are never replicated or served.
  The bound joins by `max` and is refreshed by what is held (adds, overwrites, compaction
  floors), never by a removal: a tombstone leaves the carrier's bound unchanged, and a removal
  against nothing held stores nothing.
  The native file store also retires a record the current schema cannot decode on the read that
  discovers it, so values written by earlier builds are dropped instead of failing every later
  write to their key.
- Storage admission rejects peer-supplied CRDT versions whose logical time is more than
  `TS_OFFSET_TOLERANCE_MS` ahead of the receiver's clock, so a forged register floor can no longer
  pin a key, and rejects any payload element larger than `ENTRY_PAYLOAD_MAX_BYTES` (32 KiB), so a
  carrier holds at most `ENTRY_DATA_MAX_LEN * ENTRY_PAYLOAD_MAX_BYTES` encoded bytes. Admission is
  applied to the peer-supplied delta, never to the receiver's join result.
- `Entry::operate`, `overwrite`, `extend`, `touch`, `compact_data`, and `EntryOperation::stamped`
  take the operation-boundary time explicitly; the clock is read only at the storage boundary.
  `Entry::extend` applies to relay inboxes as well as data topics. `Session::verify_at`,
  `MessageVerification::{verify_at, verify_live, verify_live_at}`, and
  `MessageVerificationExt::verify_at` judge a signature as of a given instant;
  `MessageVerification::verify_unexpired` is `verify_live`. A full carrier fits one transport
  message by a compile-time law over `ENTRY_DATA_MAX_LEN` and `ENTRY_PAYLOAD_MAX_BYTES`.
- `rings_core::storage::sled::SledStorage` is `rings_core::storage::file::FileStorage`, the name
  of what it has been since the byte budget landed: one file per key under a budget. The three
  poisoned-lock errors (`DHTSyncLockError`, `CallbackSyncLockError`, `StorageLockPoisoned`) are
  one `Error::LockPoisoned`.

### Added

- Relay inbox for offline peers. A `CustomMessage` that reaches the node responsible for its
  destination's ring position while the destination has no connection is *held*: wrapped in a
  `HeldMessage` under the holder's signature (domain
  `rings-core:relay-inbox:held-message:v1`, timestamp = hold instant) and written into the inbox
  carrier `destination + 1` (`EntryKind::RelayMessage`), which is placed by the ring geometry in
  every storage mode. Storage admits an inbox element only under a witness the owner verifies
  itself: it is a held `CustomMessage` addressed to the inbox's peer, held no later than the
  receiver's clock, whose holder signature verifies inside the local overlay and whose payload
  verifies *as of the hold instant* (`MessageVerificationExt::verify_at`; a sender's session
  expiring later does not unverify a held message, and a holder can hold only inside the sender's
  proof lifetime). Authority is checked at the write: a hold only from the node the owner routes
  the destination to and only while the message's sender proof is still live by the owner's own
  clock, a removal only from the recipient, a relocation only as an ownership hand-off from the
  authenticated predecessor; a relay carrier is never fetched, cached, replicated, or returned to
  a lookup. Removal is per element by its add dot (never a reset floor), the
  carrier keeps at most `RELAY_INBOX_MAX_LEN` (64) newest messages, and inboxes are retained for
  `DEFAULT_RELAY_INBOX_TTL_MS` (24 h) after the last hold, up to `MAX_RELAY_INBOX_TTL_MS` (7 d).
  When the peer returns its inbox key falls into its own storage interval and the predecessor's
  repair pass hands it over; every storage maintenance pass the peer drains its local inbox
  through the inbound pipeline (application validation, dispatch, `on_inbound`, each under the
  inbound deadline) and tombstones each element as it is delivered. Delivery is at least once.

### Fixed

- Storage ownership hand-off is now part of the storage repair pass: the pass drains the local
  inbox, offers the live local entries placed beyond `(self, successor head]` to the head (the
  receiver's acknowledgement removes the local copy), and republishes to missing affine owners,
  all through the repair delivery window with its fresh-connection grace. Every topology
  transition that moves the successor head (`TopologyAction::SuccessorHeadChanged`) requests a
  repair round instead of sending. Previously the hand-off ran only when the old successor
  replied with `NotifyPredecessorReport`, so a head moved by a topology query or by a directly
  connected peer left entries at a node that no longer owned them until they expired.
- The native file-backed storage now enforces its configured byte capacity: a write beyond the
  budget retires the least recently written keys first, a value larger than the whole budget is
  rejected with `Error::StorageValueExceedsCapacity`, the index is updated only after the file
  system operation succeeded, and the budget is restored on open.
- The local fetched-entry cache is bounded at `LOCAL_CACHE_CAPACITY` entries, evicting the least
  recently written entry.
- Every read-modify-write of a DHT storage slot (an operation, a join, an acknowledged removal,
  and the retirement a read performs) runs under one storage transition per ring, so a hold
  arriving from the inbound actor while the stabilizer drains the same inbox, or a hand-off
  joining while a repair pass reads, can no longer overwrite each other's write.
- A node that has successors but no known predecessor is no longer responsible for the whole
  ring: it forwards messages instead of holding them, and its responsibility interval is
  `(predecessor, self]` exactly.
- Storage is partitioned by entry kind: a data topic and a relay inbox at the same position are
  distinct slots (relay slots render as `relay:<placement>`, data slots keep the bare placement,
  so values written by earlier builds stay addressable). Previously any node could park a data
  topic at `d + 1` through `storage_store` and every hold for `d` then failed with
  `EntryKindNotEqual` for as long as the topic was refreshed. A lookup reads the data slot only.
- A relay carrier has exactly one placement, its DID, under any configured storage redundancy:
  a placed operation or a synced entry naming a rotated replica key for a relay carrier is
  refused, so a hold can no longer be admitted at a rotated position under the authority for
  that position's own inbox.
- Node descriptors verify only when the body states the receiver's overlay, and onion backward
  payloads are verified under the receiver's overlay rather than the overlay stated by the exit.

## 0.20.2

### Added

- Bound the number of peers holding a logical connection record at twice the topology
  reference slots. A reservation against a full table evicts one admitted peer that no local
  topology slot references, preferring a generation already revoked by a send failure and then
  the peer silent longest among those older than the retention grace, and otherwise fails with
  `Error::ConnectionCapacityExceeded`.

### Fixed

- Chord `find_successor` now forwards to the successor head when no finger precedes the target
  instead of addressing the local node, so finger fixing and lookups over a sparse finger table
  make progress rather than failing on a self hop.

## 0.20.0

### Breaking changes

- Secret signing keys are no longer `Copy`; signing helpers now borrow keys and return explicit
  errors instead of silently substituting invalid signatures or scalars.
- Secret key containers now zeroize their long-lived scalar or seed storage on drop.
- Secp256r1 verification now rejects high-s signatures, so persisted high-s secp256r1 session
  proofs from older builds no longer verify. Ed25519 verification now uses strict signature
  validation.

### Added

- Add the opt-in native IPv4/TCP gateway for explicitly selected destination prefixes, with
  Linux TUN, macOS utun, Windows Wintun, and a separately launched Unix privilege helper.
- Add Swift and Kotlin/JNA desktop FFI examples aligned with the existing Python ownership and
  provider lifecycle.

## 0.19.0

### Breaking changes

- The unused Subring DHT model and `SubringInterface` are removed. This shifts the Postcard
  discriminants for `EntryKind::RelayMessage` and the operations after the former
  `EntryOperation::JoinSubring` slot. Mixed-version networking with nodes running `v0.17.x` or
  earlier is unsupported. Native and browser persistent DHT storage must be wiped before upgrading:
  old Subring entries can otherwise decode as relay messages, while old relay-message entries fail
  to decode from native Sled storage.
- The repository-maintained Nova proof application is removed, including `rings-snark`,
  `rings-snark-extension`, its examples, and the browser proof workbench. The last tagged Rings
  source release containing that application is `v0.17.0`; the last published `rings-snark` crate
  is `0.12.0`, while `rings-snark-extension` was not published on crates.io. Existing users can pin
  or fork those versions. Rings does not select a replacement proof system and does not yank the
  historical crate.
- `rings_transport::core::transport::TransportMessage::Custom` now stores `bytes::Bytes` instead
  of `Vec<u8>`.
  Migrate owned vectors with `TransportMessage::Custom(data.into())`. Its wire encoding remains
  compatible with 0.17.
- Native `rings_core::swarm::callback::SwarmCallback` methods now return
  `rings_core::swarm::callback::CallbackError`, whose error source must be `Send + Sync`.
  Implementations may also return `Box<dyn std::error::Error + Send + Sync>`; wasm callbacks keep
  the thread-local error bound.
- `rings_transport::core::callback::TransportCallback::on_message(&str, &[u8])` is replaced by
  `on_admitted_message(AdmittedInboundMessage<'_>)`. Callback implementations should read the
  connection identifier and payload through `cid()` and `payload()` (or consume the value with
  `into_parts()`); this admission token proves that envelope decoding and raw-frame capacity
  reservation already succeeded.
- Custom `rings_transport::core::transport::TransportInterface` implementations must own one
  stable `Arc<rings_transport::callback::InboundFrameCapacity>` and return it from
  `inbound_frame_capacity()`. Construct the accountant once with `InboundFrameCapacity::new()` and
  share that same allocation across every connection created by the transport.
- Custom `rings_transport::core::transport::ConnectionInterface` implementations must update
  `send_message_with_permit`: at the final cancellable backend boundary, call
  `permit.try_mark_irrevocable()` and stop unless it yields a proof. After the backend queue accepts
  the bytes, consume the proof with `mark_accepted()` before returning success. If work fails or is
  abandoned after claiming the proof but before acceptance, retire and close that connection
  generation before returning.
- `rings_transport::core::pool::MessageSenderPool` is removed. Send admission now requires
  connection-generation retirement state, so custom backends should implement
  `ConnectionInterface::send_message_with_permit` on the connection instead of on a channel pool.
- Construct `rings_transport::callback::InnerTransportCallback` with
  `for_transport(&transport, cid, callback, notifier)` instead of `new(cid, callback, notifier)`, so
  callback admission shares the transport's frame accountant. The connection identifier field is
  now private; read it with `cid()`.

### Changed

- Default CLI and SDK logging now emit only error-level events unless callers explicitly select a
  more verbose level; high-frequency transport diagnostics moved to debug or trace.

### Fixed

- Bound inbound and outbound scheduling, chunk reassembly, and data-channel delivery lifetimes.
- Preserve typed callback and native send task error sources.
- Keep connection admission, cancellation, and same-class transfer ordering coherent under races.
- Emit the observable `Connecting` state before connection establishment completes.
- Run native and wasm transport tests with production timeout profiles; dummy-native transport
  tests keep a reduced profile for bounded failure witnesses.
