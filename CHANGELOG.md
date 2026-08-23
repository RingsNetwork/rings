# Changelog

## 0.18.0

### Breaking changes

- `rings_transport::TransportMessage::Custom` now stores `bytes::Bytes` instead of `Vec<u8>`.
  Migrate owned vectors with `TransportMessage::Custom(data.into())`. Its wire encoding remains
  compatible with 0.17.
- Native `rings_core::SwarmCallback` methods now return `CallbackError`, whose error source must be
  `Send + Sync`. Implementations should return `rings_core::CallbackError` or
  `Box<dyn std::error::Error + Send + Sync>`; wasm callbacks keep the thread-local error bound.

### Fixed

- Bound inbound and outbound scheduling, chunk reassembly, and data-channel delivery lifetimes.
- Preserve typed callback and native send task error sources.
- Keep connection admission, cancellation, and same-class transfer ordering coherent under races.
- Emit the observable `Connecting` state before connection establishment completes.
- Run native and wasm transport tests with production timeout profiles; dummy-native transport
  tests keep a reduced profile for bounded failure witnesses.
