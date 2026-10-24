# Rings mailbox example

The mailbox `me@<did>.<zone>` as a program a Rings node serves through a DNS relay (#1049): mail
from anywhere arrives over SMTP, and the user reads it with any mail client over IMAP.

```text
  mail server ─▶ DNS relay ─▶ TCP relay over Rings ─▶ node ─▶ receiver (127.0.0.1:2525) ─▶ INBOX
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

Every listener binds loopback: the node reaches the receiver through its TCP relay, and
submission takes mail without a password, which only loopback makes safe. The example ends no
TLS. An SMTP client silent for five minutes is told `421` and disconnected.

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

## Module layout

- `store`: the mailboxes, their model and its disk shell.
- `smtp`: the SMTP session as a pure state machine, its socket shell for both listeners, and
  the receiver's PROXY header reader.
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
between relay and owner is tested in `rings-node` over its in-memory transport.
