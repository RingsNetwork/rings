# Changelog

## 0.18.0

### Breaking changes

- The unused Subring DHT model and `SubringInterface` are removed. Legacy Postcard tags remain
  decode-compatible reserved slots so live entry and operation discriminants do not shift; old
  Subring values are ignored at native and IndexedDB DHT storage boundaries and are never repaired
  or replicated.
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
