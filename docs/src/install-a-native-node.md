---
description: Start from using rings-node
---

# Install a native node

## Installation

You can install rings-node either from Cargo or from source.

### From Cargo:

```
cargo install rings-node
```

 > Rings Network is written in [Rust](https://www.rust-lang.org/). [Cargo](https://crates.io/) is a package management tool for the Rust language. You can learn about how to install and use Cargo [here](https://doc.rust-lang.org/cargo/getting-started/installation.html).

### From Source

```
git clone https://github.com/RingsNetwork/rings-node
cd rings-node
cargo build
```

### Usage

```
rings <command> [options]
```

#### Commands

* `help`: displays the usage information.
* `init`: creates a default configuration file, `~/.rings/config.yaml` unless `--location` says otherwise. This file can be edited to customize the behavior of the rings-node daemon. The generated file states every section explicitly, including a complete `gateway:` section with `enabled: false` (see [Native Gateway](native-gateway.md)).
* `run`: runs the rings-node daemon. By default, the daemon will use `~/.rings/config.yaml` for configuration. Use the "-c" or "--config" option to specify a custom configuration file.

#### Options

* `-c, --config <FILE>`: specifies a custom configuration file to use instead of the default `~/.rings/config.yaml`. The configuration file is used to specify the network configuration, account settings, and other parameters that control the behavior of the rings-node daemon.
* `-h, --help`: displays the usage information.
* `-V, --version`: displays the version information for rings-node.
