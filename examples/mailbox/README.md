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
  `Return-Path` and a `Received` trace header, and accepted only once it is on disk.
- **Store.** Mailboxes on disk: INBOX, Sent, Drafts, Trash, Junk and Archive (with their
  RFC 6154 special uses) and any the user creates; messages with UIDs, a UID validity and flags.
  Every change is a transaction persisted by atomic renames.
- **IMAP.** IMAP4rev1 with IDLE, UIDPLUS, MOVE, UNSELECT, SPECIAL-USE, NAMESPACE, ID and ENABLE;
  FETCH of ENVELOPE, BODYSTRUCTURE and every BODY section, SEARCH, STORE, COPY, APPEND and
  EXPUNGE. A session's view of its mailbox follows the store, so an idling client hears of new
  mail at once.
- **Submission.** SMTP from the user to anyone, delivered to each recipient's mail exchanger
  with STARTTLS when offered, retried on a growing schedule for three days, and bounced to the
  INBOX when refused or given up on. A recipient `me@<did>.<zone>` resolves through the relay's
  DNS, so one node's mailbox writes to another's like any mail server.

Every listener binds loopback: the node reaches the receiver through its TCP relay, and
submission takes mail without a password, which only loopback makes safe. The example ends no
TLS.

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
```

Point a mail client at IMAP `127.0.0.1:1143` (user `me`, the password in
`~/.rings/mailbox/password`) and SMTP `127.0.0.1:1587` (no authentication).

## Module layout

- `store`: the mailboxes, their model and its disk shell.
- `smtp`: the SMTP session as a pure state machine, and its socket shell for both listeners.
- `imap`: the IMAP session, its view of the selected mailbox, MIME structure for FETCH, SEARCH,
  and the socket shell over `imap-next`.
- `outbound`: the delivery queue, its retry schedule, and the mail exchangers it delivers to.
- `mailbox`: the program assembled from one configuration.

## Test

```sh
cargo test -p rings-mailbox-example
```

`test_mailbox` drives the mailbox over real sockets with plain SMTP and IMAP clients: delivery,
IMAP reading, flags, expunge and IDLE, and mail from one mailbox to another through submission,
including a bounce. `test_relay` runs the whole path over two in-process Rings nodes: a mail
server resolves `me@<did>.rings.rs` through the DNS relay and delivers through its ingress, and
the message lands in the owner's INBOX. Like the relay example's tests, it skips when the two
nodes cannot link over WebRTC.
