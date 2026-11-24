# rings-codec

`rings-codec` provides the canonical binary wire encoding helpers used across Rings crates.

## Wire encoding

- Serialization and deserialization wrap [`postcard`](https://docs.rs/postcard) via `serialize`, `deserialize`, and `serialized_size`.
- `deserialize` enforces whole-buffer consumption and fails with `Error::TrailingBytes { decoded, total }` if any trailing bytes remain after decoding a message.
- `deserialize_prefix` decodes a prefix value and returns the unconsumed slice for zero-allocation envelope inspection.
- `deserialize_enum_variant` reads the leading wire enum discriminant (`u32`) without allocating or deserializing the variant body, supporting bounded admission checks before full decoding.
