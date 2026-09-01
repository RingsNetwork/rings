# Desktop FFI examples

The Python, Swift, and Kotlin/JNA examples all consume the same stable C ABI in
`crates/node/include/rings.h`. They own the returned provider handle, retain the signer callback,
free every non-null request string, and destroy the provider exactly once.

The Swift and Kotlin wrappers expose the same high-level desktop flow as the Python wrapper:
node identity, offer/answer exchange, peer connection polling, E2E handshake and message sending,
and E2E event/stream polling. They deliberately stop at the portable C ABI; mobile packaging and
iOS NetworkExtension or Android VpnService integration are outside this release.

Build the actual native library first:

```bash
cargo build -p rings-node --features ffi
```

The Swift wrapper dynamically loads the library, so it needs no Xcode project or link-time search
path. Its smoke test builds a deterministic fake C ABI and exercises two simultaneous signer
callbacks, request ownership, and provider destruction. Pass the real library as an argument to
also verify dynamic loading, every required symbol, and a call into the Rust logging entry point:

```bash
bash examples/ffi/swift/test.sh target/debug/librings_node.dylib
```

Use `RingsRuntime(libraryPath:)` with `target/debug/librings_node.dylib` (macOS) and supply an
EIP-191 signer returning exactly 65 bytes. The fixed trampoline pool supports eight simultaneous
Swift providers because the current C callback has no context pointer.

The Kotlin wrapper uses JNA and retains one callback object per provider, so it does not need a
fixed callback pool. Its Linux desktop smoke test uses the same fake C ABI for deterministic
behavior and loads the real Rust ABI when `RINGS_FFI_LIBRARY` is set:

```bash
RINGS_FFI_LIBRARY="$PWD/target/debug/librings_node.so" \
  RINGS_FFI_REQUIRE_LIBRARY=1 \
  gradle -p examples/ffi/kotlin test --no-daemon
```

Use `RingsRuntime.load(path)` with `librings_node.so` on Linux, `librings_node.dylib` on macOS, or
`rings_node.dll` on Windows. As in the Python example, the application supplies an EIP-191 signer;
neither wrapper chooses a wallet or stores private keys.

The real-library load checks are intentionally narrower than the Python integration tests: Swift
and Kotlin validate their own loader against the Rust ABI, while the deterministic fake validates
wrapper behavior. Python supplies a real EIP-191 signer and therefore remains the shared native
provider/offer/E2E runtime test instead of duplicating wallet cryptography in every example.
