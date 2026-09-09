# Build for Wasm

We offer several ways to use Wasm Rings, including:

1. Using Rings SDK
2. Using Rings Browser Extension
3. Build your custom-rings

### Rings SDK

To fetch the SDK from `npm`, just:

```
npm i @ringsnetwork/rings-node
```

If you want to use the latest version of Rings Network, you can also directly incorporate the latest version or a specific branch of Rings Network into your Node.js project by modifying the package.json file. For example:

```
# package.json
"@ringsnetwork/rings-node": "https://github.com/RingsNetwork/rings.git#master",

```

As a developer, if you want to build local offline dependencies, you can do the following.

````
```text
"@ringsnetwork/rings-node": "git+file:../rings#master",
````

Note: The reason for using a git path instead of a local path is that npm, after a certain version, no longer executes the prepare script in local path packages, and the package relies on the prepare script to help with wasm compilation.

### Rings Browser Extension

The browser extension is the repository's frontend packaged as a Chrome Manifest V3
extension; [Browser Node and Extension](browser-node.md) covers the hosted console and the
packaging steps.

### Custom Build

To build Rings Network for WebAssembly, run the following commands:

```
cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm

```

Or build with `wasm-pack`

```
wasm-pack build --scope ringsnetwork -t web --no-default-features --features browser --features console_error_panic_hook
```
