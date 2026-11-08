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
descriptor (#918), re-published on every heartbeat. Its signature carries the delegation that
authorizes the node's session key.

Mail is sealed to that session key, the delegatee's secp256k1 public key `ek`, because a wallet
account (EIP-191, BIP-137) cannot decrypt with its account key. The descriptor carries no copy
of `ek`. The gateway recovers it from the descriptor's signature, so `ek` is always the key that
signed it.

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
heartbeat. Within the TTL, gateways then defer mail to it with `451`, and senders retry until
it is back.

## The gateway

| Command | Rule |
|---|---|
| `RCPT TO:<l@any domain>` | `250` if `l` is a DID with a live descriptor, which the session keeps for DATA; `451` if it has none or the DHT cannot tell; `550` if `l` is not a DID |
| `DATA m` | for each distinct recipient DID, concurrently: seal the traced message, write it; `250` once an owner of every write reported holding it |
| | `552` above 32 chunks (`SIZE` advertises the bound, about 1 MiB); `451` if every owner of a write refused it, no owner reported within 60 seconds, or a write failed |

* A DID with no live descriptor gets `451`, not `550`. A gateway that keeps nothing cannot tell
  a recipient that is away from one that never took mail. The sender's retries reach a
  recipient that comes back, and only a local part that is not a DID is refused.
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
chunk_i    = slice_i(c)                                each element ≤ ENTRY_PAYLOAD_MAX_BYTES
manifest   = (mail_id = H(c), [H(chunk_0) … H(chunk_{n−1})])
write      = Extend(topic(d), [manifest, chunk_0 … chunk_{n−1}], lifetime = MAX_TTL_MS)
```

A mail is one `Extend` of all its elements (`ScopedStorage::storage_extend_reported`). The write
requests the data kind's longest lifetime, `MAX_TTL_MS`, so every element lives 100 minutes.
All the elements share one version, travel in one message to each owner, and retire together.
A plain append would request the default 10 minutes and need one message per element.

Laws the recipient keeps:

* **Integrity.** It reads a chunk only through a manifest that lists the chunk element's
  digest, in that manifest's order. A forged chunk, or a manifest listing one, never alters a
  mail. The manifest is not signed: gateways are interchangeable, and the content address
  below already fixes what a manifest can claim.
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
  mail again on restart. A crash never loses a mail; at worst it files it twice.
* **Two keys.** An opened or unreadable verdict belongs to the delegatee key that tried to open
  the mail. Tabs sharing a store cannot hide mail from a tab that holds its key. Filed
  `Message-ID` records stay shared, so a resend encrypted to another key is still a duplicate.
* **One key.** Mail opens only under the node's current delegatee key. Mail sealed to a key the
  node no longer holds is lost, like mail not fetched within its lifetime. A browser node takes
  a new key at every load, so mail that a gateway accepted under the old key, within the
  descriptor's freshness window around a reload, never opens.

```rust
use rings_node::mail::recipient::KvInbox;
use rings_node::mail::recipient::Recipient;
use rings_node::mail::recipient::DEFAULT_MAIL_POLL_INTERVAL;

let inbox = KvInbox::new(mail_store);
let recipient = Recipient::start(processor, inbox, records, DEFAULT_MAIL_POLL_INTERVAL)?;
recipient.run(stop).await; // or `recipient.poll().await?` on a schedule of your own
```

## Receiving in a browser

Receiving is off in the existing provider constructors. Choose it before the processor is
built with `Provider.new_provider_with_mail`:

```javascript
const provider = await Provider.new_provider_with_mail(
  config,
  "my-rings-node",
  new BrowserMailOptions(),
  (id) => console.info("Mail filed:", id),
);
const listener = provider.listen();
await listener.started();

const messages = await provider.list_mail();
for (const { id, message, filed_at_ms } of messages) {
  // message is a Uint8Array of raw RFC 5322 bytes; filed_at_ms is a bigint.
  const stored = await provider.read_mail(id); // the same fields, or null
  await provider.remove_mail(id);
}
```

`config` is the existing `ProcessorConfig`; neither its shape nor the existing constructors
changes. `new BrowserMailOptions()` uses a 30-second heartbeat and a 90-second descriptor TTL.
For another schedule, pass heartbeat and TTL in milliseconds, for example
`new BrowserMailOptions(15000, 60000)`. Each supplied schedule is checked by `MailOffer::new`.
The Rust browser constructor is `Provider::new_browser_provider_with_mail(config,
storage_name, options, on_mail)`.

The provider polls at construction and every 60 seconds. Listening publishes the descriptor;
the usual overlay connection setup still applies. The poll runs as a node task, so
`Provider::close` stops it. The optional callback receives the mail id after the inbox write
commits, without the embedder polling IndexedDB. A thrown callback error is logged and does
not undo the filed mail. A crash or a failed recipient-record write can repeat a notification
on retry, so callback handlers should be idempotent by mail id.

The inbox is a `KvInbox<IdbStorage>` under `<storage_name>/mail-inbox`; its recipient records
are under `<storage_name>/mail-records`. Both stores use `RecordAuthority::Retained`: opening
them must succeed, a full store fails the poll for a later retry, and neither store evicts a
row. Closing and reopening with the same storage name preserves filed mail and deduplication
records. Removing mail removes only its inbox row; its deduplication record stays until its
retention expires.

Polls of one recipient are serialized. Separate tabs share the records but do not atomically
file and remember together: simultaneous polls can both file before seeing the other's record,
consistent with the recipient's at-least-once behavior.

**One-key loss:** a browser takes a new delegatee key on every load. Mail accepted under the
previous key during the descriptor's freshness window around a reload never opens under the
new key. This accepted loss window is at most 93 seconds with the default offer (`TTL + 3 s`
clock tolerance), or the configured TTL plus 3 seconds. Already filed mail remains readable
after reload. Two open tabs may share the stores: each key's unreadable verdict is separate,
while filed `Message-ID` records remain shared. This API receives mail; browser sending is
not implemented.

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
