# Decentralized Service Examples

For a local-service example, use the
[TCP/UDP relay demos](https://github.com/RingsNetwork/rings/tree/master/examples/relay).
They register a named service and carry bytes between two Rings peers:

```sh
cargo run -p rings-relay-example --example rings-tcp-relay-example
cargo run -p rings-relay-example --example rings-udp-relay-example
```

For HTTP access through an Onion exit, see the [Native Gateway](../native-gateway.md)
and [Browser Node and Extension](../browser-node.md) guides. The relay demos and
Onion gateway have different routing and privacy properties; a direct relay to a
named service does not provide Onion routing.
