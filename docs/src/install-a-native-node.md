# Install a native node

## Installation

You can install the `rings` binary from a prebuilt release, from Cargo, or from source.

### Prebuilt binaries

Install the latest release on macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`) or Linux
(`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`) with the one-line installer:

```sh
curl -fsSL https://rings.rs/install.sh | sh
```

The script verifies the archive's SHA-256 checksum and installs `rings` to `/usr/local/bin`
(when writable) or `~/.local/bin`. Re-run the command to upgrade in place, or pass
`--version vX.Y.Z` to install a specific tag:

```sh
curl -fsSL https://rings.rs/install.sh | sh -s -- --version v1.0.0-pre.8
```

You can also download an archive directly from
[Releases](https://github.com/RingsNetwork/rings/releases), verify its `.sha256` file, and
place `rings` on your `PATH`:

```bash
shasum -a 256 -c rings-v1.0.0-pre.8-aarch64-apple-darwin.zip.sha256
unzip rings-v1.0.0-pre.8-aarch64-apple-darwin.zip
install -m 755 rings /usr/local/bin/rings
rings --version
```

For running `rings` persistently under `systemd` or `launchd`, see
[`contrib/README.md`](https://github.com/RingsNetwork/rings/blob/master/contrib/README.md).
Both templates configure file-descriptor limits (`LimitNOFILE=65535` on Linux and
`NumberOfFiles = 10240` on macOS), and `rings run` automatically raises its soft `RLIMIT_NOFILE`
limit on startup to prevent `EMFILE (Too many open files)` across multi-peer WebRTC meshes.

### From Cargo

The workspace denies compiler warnings, so build with the Rust release pinned in the
repository's `rust-toolchain.toml`; a newer compiler can add a warning that fails the build:

```sh
cargo +1.97.0 install --locked rings-node
```

Companion native binaries in the workspace can be installed the same way:

```sh
cargo +1.97.0 install --locked rings-tui           # interactive terminal dashboard
cargo +1.97.0 install --locked rings-turn          # standalone signature-authenticated TURN server
cargo +1.97.0 install --locked rings-turn-service  # DHT-discovered TURN + multi-hop Onion TURN daemon
```

 > Rings Network is written in [Rust](https://www.rust-lang.org/). [Cargo](https://crates.io/) is a package management tool for the Rust language. You can learn about how to install and use Cargo [here](https://doc.rust-lang.org/cargo/getting-started/installation.html).

### From Source

```sh
git clone https://github.com/RingsNetwork/rings
cd rings
cargo install --path crates/node
```

### Usage

```text
rings [OPTIONS] <COMMAND>
```

#### Top-level options

* `--log-level <LOG_LEVEL>`: set log verbosity (`debug`, `info`, `warn`, `error`, `trace`; env `LOG_LEVEL`, default `error`).
* `--runtime <RUNTIME>`: Tokio scheduler flavor (`multi-thread` or `current-thread`; env `RUNTIME`, default `multi-thread`).
* `-h, --help`: display usage information.
* `-V, --version`: display version information for `rings`.

#### Commands

* `init`: write a default `~/.rings/config.yaml` (or `--location <PATH>`) with a fresh `delegatee_key` (`0600`) and a disabled `gateway:` section; see [Host a Native Node](host-a-native-node.md).
* `new-delegation`: create or rotate a delegated signing key (`delegatee_key`).
* `run`: run the foreground Rings node daemon (accepts `-c, --config <FILE>`, default `~/.rings/config.yaml`, env `CONFIG`).
* `pubsub`: open an interactive publish/subscribe loop on a DHT data topic.
* `connect`: connect to a remote peer (`node`, `did`, `seed`, `offer`, `answer`, `accept`).
* `peer`: manage connected peers (`list`, `disconnect`).
* `send`: deliver a namespaced payload (`send message <TO_DID> <NAMESPACE> <DATA>`).
* `service`: register or look up a named service provider (`register`, `lookup`).
* `inspect`: inspect local swarm peers, successor list, predecessor, and finger table.

See [Operate a Node from the CLI](cli.md) for the full CLI reference and [Interactive Terminal UI (TUI)](tui.md) for `rings-tui`.
