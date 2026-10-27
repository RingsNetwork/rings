# Mail Gateway

A mail gateway lets any Rings node receive internet mail at `did@domain`, a browser node
included (#1117). The node does not need to listen on a socket, run an SMTP server, or be
online when the mail is sent. The gateway is a stateless SMTP receiver. It seals each mail to
its recipient's key and writes it to an ordinary DHT data topic. The recipient's node fetches the
topic, opens the mail and files it.

```text
  mail server ─MX─▶ gateway ─seal, Extend(mail:<did>, 100 min)─▶ DHT ◀─poll─ recipient node
                       ▲                                                        │
                       └──────────── μ(did) on mail_descriptor:<did> ◀──────────┘ heartbeat
```

Unlike the [DNS relay](dns-relay.md), the gateway does not forward a connection to the
recipient, so the recipient needs no IPv6 address, no served port and no running mailbox
server. The gateway keeps nothing either: each RCPT is one DHT read, and each DATA is one write
per recipient.

## Addressing

* The address is `did@domain`. The local part is the recipient's DID, `0x` and 40 hex digits
  (42 octets, within the 64 of RFC 5321), compared in lower case.
* `domain` is any domain whose MX names a gateway, for example `p2p.rings.rs MX 10 mx.example.net`.
  Any number of gateways can serve a domain, on ordinary IPv4 or IPv6 addresses, and a sending
  server's MX fallback skips one that is down.
* Every such domain reaches the same mailbox: `d@a` and `d@b` are one address.

## Opting in: the mail descriptor

A node takes mail only while it publishes a mail descriptor `μ(d)` on its topic
`mail_descriptor:<did>`. The descriptor is a signed registry lease, like the DNS relay's service
descriptor (#918), re-published on every heartbeat. It carries:

* the delegatee's secp256k1 public key `ek`, which mail is sealed to;
* in its signature, the delegation that authorizes that key.

A wallet account (EIP-191, BIP-137) cannot decrypt with its account key, so the descriptor names
the session key instead. A reader takes the descriptor only if `ek` is the key that signed it.

In Rust, a node opts in through its builder:

```rust
use rings_node::mail::registration::MailOffer;

let processor = ProcessorBuilder::from_config(&config)?
    // ... stores ...
    .receive_mail(MailOffer::default()) // a heartbeat every 30 s, live for 90 s
    .build()?;
```

The schedule obeys the registry timing law. The heartbeat interval must be below the TTL and
below 300 seconds, and the TTL at most 600 seconds. A node that stops taking mail stops its
heartbeat. Gateways then refuse its address within the TTL.

## The gateway

| Command | Rule |
|---|---|
| `RCPT TO:<l@any domain>` | `250` if `l` is a DID with a live descriptor; `550` otherwise; `451` if the DHT cannot tell |
| `DATA m` | for each distinct recipient DID, concurrently: seal the traced message, write it; `250` once an owner of every write reported holding it |
| | `552` above 32 chunks (`SIZE` advertises the bound, about 1 MiB); `451` if every owner of a write refused it, no owner reported within 60 seconds, or a write failed |

* `250` means that an owner reported storing the mail (a store report, #866). One owner is
  enough, since a recipient's fetch can then reach the mail. Waiting for every owner would let
  one slow or refusing owner turn a held mail into a retry. Read-your-writes (#948, #982)
  answers the writer's reads from its own cache, so a read-back is no such report.
* A `451` loses nothing. The sender retries, and a copy that an owner did hold is a duplicate
  that the recipient files once.
* The report wait, `MAIL_STORE_REPORT_WAIT`, is 60 seconds. That is far above an overlay round
  trip and well inside the ten minutes an SMTP client waits for the reply to its final dot
  (RFC 5321 §4.5.3.2.6).
* A mail that its recipient does not fetch within its lifetime, 100 minutes, expires and is
  lost. This is the intended semantics: the gateway keeps no queue.
* STARTTLS uses the gateway's own certificate. It is opportunistic, as between mail servers.
* The gateway prepends `Return-Path` and `Received` to the message before sealing it, as a final
  receiver does. The recipient files the message as it opens.

Run a gateway as its own node, with that node's configuration (not beside `rings run`):

```bash
cargo run -p rings-mailbox-example --bin rings-mail-gateway -- \
  --config ~/.rings/config.yaml --host mx.example.net \
  --tls-cert /etc/ssl/mx.pem --tls-key /etc/ssl/mx.key
```

It listens on `[::]:25` by default (`--smtp`). Point the MX of each domain at `--host`.

## Storage

```text
topic(d)   = "mail:" ‖ d                              EntryKind::Data
c          = AeadCiphertext(m)                         ElGamal-wrapped ChaCha20-Poly1305 key,
                                                       aad = "rings-node:mail:" ‖ d
chunk_i    = (mail_id, i, n, slice_i(c))               each element ≤ ENTRY_PAYLOAD_MAX_BYTES
manifest   = (d, mail_id = H(c), [H(chunk_i)], sig_gateway)
write      = Extend(topic(d), [manifest, chunk_0 … chunk_{n−1}], lifetime = MAX_TTL_MS)
```

A mail is one `Extend` of all its elements (`ScopedStorage::storage_extend_reported`). The write
requests the data kind's longest lifetime, `MAX_TTL_MS`, so every element lives 100 minutes.
All the elements share one version, travel in one message to each owner, and retire together.
A plain append would request the default 10 minutes and need one message per element.

Laws the recipient keeps:

* **Integrity.** It takes a chunk only if the chunk element's digest is listed in a manifest
  whose gateway signature verifies. A forged chunk that names a known `mail_id` is never read.
* **All or nothing.** The AEAD tag covers the whole of `c`. A missing or altered chunk discards
  the whole mail, and the next poll tries it again.
* **Content address.** It ignores a manifest whose chunks do not concatenate to the preimage of
  its `mail_id`. A forger therefore cannot mark a genuine mail unreadable.
* **No removal.** It never removes its chunks; each element retires at its horizon. A data
  carrier refuses removes past `DATA_TOMBSTONE_MAX_LEN`, about 10 a minute per topic.
* **Deduplication.** Encryption is randomized, so a resent mail is a new ciphertext. The
  recipient deduplicates by the decrypted `Message-ID`, kept for five days (RFC 5321 retries).

Bounds: at most 32 chunks a mail, about 1 MiB. A topic keeps `ENTRY_DATA_MAX_LEN = 1024`
elements, dropping the oldest first, so 32 mails of the largest size fit within one lifetime.

## The recipient

The recipient library (`rings_node::mail::recipient`) builds for native and browser nodes.

* **Polling.** It reads its topic when it starts, then every interval: 60 seconds by default,
  at most 50 minutes, half the horizon.
* **Storage.** It keeps its records in any key-value store: a file store natively, IndexedDB
  in a browser. It files mail into any `MailInbox` (`KvInbox` over a key-value store, or the
  mailbox example's INBOX).
* **At least once.** It files a mail before it records it, so a crash between the two files the
  mail again on restart. A mail is never lost to the recipient, at worst filed twice.
* **Key rotation.** When the node starts under a new delegatee key, the recipient retires the
  old one and keeps it for at least 100 minutes, so mail sealed to the old key still opens. The records
  hold these secrets, as the node's configuration holds its current one.

```rust
use rings_node::mail::recipient::KvInbox;
use rings_node::mail::recipient::Recipient;
use rings_node::mail::recipient::DEFAULT_MAIL_POLL_INTERVAL;

let inbox = KvInbox::new(mail_store);
let recipient = Recipient::start(processor, inbox, records, DEFAULT_MAIL_POLL_INTERVAL).await?;
recipient.run(stop).await; // or `recipient.poll().await?` on a schedule of your own
```

## Example: a mailbox through gateways

The [mailbox example](dns-relay.md#example-a-mailbox) receives through gateways when it runs a
node itself, instead of `rings run`:

```bash
cargo run -p rings-mailbox-example -- --rings-config ~/.rings/config.yaml
```

It publishes the node's mail descriptor, polls the node's mail topic, and files every mail in
its INBOX, which a mail client reads over IMAP as before. Its recipient records live in
`~/.rings/mailbox/recipient`.

## Open questions

* **Topic flooding.** Anyone can write `mail:<did>`. Its 1024 elements are dropped oldest first,
  so several writers together can evict a recipient's mail. A per-writer bound needs a core
  change.
* **Gateway throughput.** A gateway's writes are bounded by its per-origin storage quota at each
  owner. The topics of different recipients lie at different owners, so the load spreads.
* **Sending from `did@domain`.** This needs an outbound relay with SPF, DKIM and reverse DNS
  for the domain, and port 25 egress. It is a later phase.
* **Browser bindings.** The recipient library builds for the browser, but the JavaScript
  provider does not expose it yet.
