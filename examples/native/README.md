# Rings native example

This native example registers the `example` namespace, connects to a seed node over HTTP, and sends
a message to a destination DID. It is intended to interoperate with
[`examples/frontend`](../frontend).

## Run with the browser frontend

Start a seed daemon first:

```sh
cargo run -p rings-node --bin rings -- init \
  --location /tmp/rings-seed/config.yaml \
  --session-sk /tmp/rings-seed/session_sk

cargo run -p rings-node --bin rings -- run \
  --config /tmp/rings-seed/config.yaml \
  --external-api-addr 127.0.0.1:50001 \
  --internal-api-port 50000 \
  --storage-path /tmp/rings-seed/storage
```

Open the browser frontend, start its provider, connect it to `http://127.0.0.1:50001`, then copy
the browser DID. The frontend registers the `example` namespace by default for native interop.

Run the native example:

```sh
cargo run -p rings-native-example -- http://127.0.0.1:50001 BROWSER_DID
```

The native example uses the same default network id as the daemon (`1`), sends one
`example`-namespace message to the browser, and waits 30 seconds so the browser can send an
`example` message back to the native DID printed at startup.
