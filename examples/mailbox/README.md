# Rings mailbox example

The mailbox `me@<did>.<zone>` as a program a Rings node serves through a DNS relay (#1049), or
the mailbox `<did>@<domain>` of a node it runs itself, receiving through mail gateways (#1117):
mail from anywhere arrives over SMTP, and the user reads it with any mail client over IMAP. The
example also holds the mail gateway, `rings-mail-gateway`.

```text
  mail server ─▶ DNS relay ─▶ TCP relay over Rings ─▶ node ─▶ receiver (127.0.0.1:2525) ─▶ INBOX
  mail server ─▶ gateway ─▶ DHT topic mail:<did> ─▶ node's poll ─▶ INBOX       (--rings-config)
  mail client ─▶ IMAP       (127.0.0.1:1143) ─▶ store
  mail client ─▶ submission (127.0.0.1:1587) ─▶ outbound queue ─▶ recipient's MX
```

- **Receiver.** SMTP for the mailbox's own address, `me@<did>.<zone>`, in every zone that
  relays the node; any other recipient is refused. Each message is filed in the INBOX with its
  `Return-Path` and a `Received` trace header naming the client, and accepted only once it is on
  disk. Behind a node that serves port 25 with `proxy_protocol: v2`, run it with
  `--receiver-proxy-protocol`: it then reads the client from the PROXY header before anything
  else, and refuses a connection without one.
- **Store.** Mailboxes on disk: INBOX, Sent, Drafts, Trash, Junk and Archive (with their
  RFC 6154 special uses) and any the user creates; messages with UIDs, a UID validity and flags.
  Every change is a transaction persisted by atomic renames.
- **IMAP.** IMAP4rev1 with IDLE, UIDPLUS, MOVE, UNSELECT, SPECIAL-USE, NAMESPACE, ID and ENABLE;
  FETCH of ENVELOPE, BODYSTRUCTURE and every BODY section, SEARCH, STORE, COPY, APPEND and
  EXPUNGE. A session's view of its mailbox follows the store, so an idling client hears of new
  mail at once. A session whose selected mailbox is deleted is told BYE and closed, and one
  silent for 30 minutes is logged out (RFC 3501 §5.4).
- **Submission.** SMTP from the user to anyone, delivered to each recipient's mail exchanger by
  the example's own small client: STARTTLS when offered, opportunistic (any certificate, as
  between mail servers without DANE), every wait bounded as RFC 5321 §4.5.3.2 says. A 4yz reply
  or an unreachable exchanger is retried on a growing schedule for three days; a 5yz reply, a
  domain that does not exist, or one that takes no mail (null MX, RFC 7505) is bounced to the
  INBOX at once, and a DNS server failure is retried, never taken for a missing MX. A recipient
  `me@<did>.<zone>` resolves through the relay's DNS, so one node's mailbox writes to another's
  like any mail server.

- **Through gateways.** With `--rings-config`, the mailbox runs the Rings node of that
  configuration itself, instead of `rings run`: it publishes the node's mail descriptor, polls
  the node's mail topic, and files every mail it opens in the INBOX
  (`rings_node::mail::recipient`).
- **Gateway.** `rings-mail-gateway` runs a node of its own and an SMTP receiver for every
  domain whose MX names it. RCPT is admitted only for a DID with a live mail descriptor: `451`
  for a DID without one, which may come back, and `550` for a local part that is no DID. DATA
  seals the traced message to each recipient's delegatee key, found at RCPT, and writes it to
  the recipient's topic. It answers `250` once an owner of each write reports storing it,
  and `451` when no owner does within 60 seconds (`552` above 32 chunks, about 1 MiB). It keeps
  nothing, and it offers STARTTLS with its own certificate (`--tls-cert`, `--tls-key`).

Every mailbox listener binds loopback: the node reaches the receiver through its TCP relay, and
submission takes mail without a password, which only loopback makes safe. The mailbox ends no
TLS; the gateway, which listens publicly, ends STARTTLS. An SMTP client silent for five minutes
is told `421` and disconnected.

## Run

```sh
cargo run -p rings-mailbox-example -- --did <the node's DID>
```

Then map the receiver in the node's `config.yaml` and restart it:

```yaml
services:
  ports:
    - port: 25
      target: 127.0.0.1:2525
      proxy_protocol: v2
```

and start the mailbox with `--receiver-proxy-protocol` to match (or leave both out).

Point a mail client at IMAP `127.0.0.1:1143` (user `me`, the password in
`~/.rings/mailbox/password`) and SMTP `127.0.0.1:1587` (no authentication).

To receive at `<did>@<domain>` through mail gateways instead, run the mailbox as the node:

```sh
cargo run -p rings-mailbox-example -- --rings-config ~/.rings/config.yaml
```

A gateway is its own node, with its own configuration, on a host whose port 25 the domains' MX
records reach:

```sh
cargo run -p rings-mailbox-example --bin rings-mail-gateway -- \
  --config ~/.rings/config.yaml --host mx.example.net --tls-cert mx.pem --tls-key mx.key
```

## Module layout

- `store`: the mailboxes, their model and its disk shell.
- `smtp`: the SMTP session as a pure state machine, its socket shell for every listener (with
  STARTTLS), the receiver's PROXY header reader, and the certificate loader.
- `gateway`: the mail gateway's SMTP front over the DHT side `rings_node::mail::gateway`.
- `receive`: the mailbox as a recipient of its node's mail topic.
- `node`: the Rings node the gateway, and a mailbox that receives through gateways, run.
- `imap`: the IMAP session (its commands by what they act on), its view of the selected mailbox,
  MIME structure for FETCH, SEARCH, and the socket shell over `imap-next`.
- `outbound`: the delivery queue, its retry schedule, the mail exchangers it delivers to, and
  its SMTP client.
- `serve`: the accept loop every listener runs.
- `mailbox`: the program assembled from one configuration.

## Test

```sh
cargo test -p rings-mailbox-example
```

`test_mailbox` drives the mailbox over real sockets with plain SMTP and IMAP clients: delivery,
IMAP reading, flags, expunge and IDLE, a session ended by its mailbox's deletion, and mail from
one mailbox to another through submission, including a bounce. `test_relay` puts the DNS
relay's authority and ingress in front of the mailbox on the loopback: a mail server resolves
`me@o1.rings.rs`, delivers to the owner's address, and the message lands in the INBOX with a
`Received` header naming the server, read from the PROXY header. The hop over the Rings overlay
between relay and owner is tested in `rings-node` over its in-memory transport. `test_gateway`
drives the gateway over a stand-in DHT: `451` for a DID with no live descriptor and `550` for
a non-DID, a mail sealed once per DID and opened into its recipient's INBOX with its trace,
`552` above the mail bound, `451` for a mail no owner confirms, and a session under STARTTLS.
The DHT side, the descriptor's lookup, the write and the recipient's poll over the overlay, is
tested in `rings-node` (`processor::tests::test_mail`).
