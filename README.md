<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static.ringsnetwork.io/ringsnetwork_logo.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/asserts/main/logo/rings_network_red.png">
</picture>

Rings Node (The node service of Rings Network)
===============

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
[![npm version](https://badge.fury.io/js/@ringsnetwork%2Frings-node.svg)](https://badge.fury.io/js/@ringsnetwork%2Frings-node)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings-node)


Rings is a structured peer-to-peer network implementation using WebRTC, Chord algorithm, and full WebAssembly (WASM) support.

For more details you can check our [Rings Whitepaper](https://raw.githubusercontent.com/RingsNetwork/whitepaper/master/rings.pdf).

You can also visit [Rings Network's homepage](https://ringsnetwork.io) to get more project info.

And you can get more document [here](https://rings.gitbook.io/).

## Installation

You can install rings-node either from Cargo or from source.

### from cargo

To install rings-node from Cargo, run the following command:

```sh
cargo install rings-node
```

### from source

To install rings-node from source, follow these steps:

```sh
git clone git@github.com:RingsNetwork/rings-node.git
cd ./rings-node
cargo install --path .
```

### Build for WebAssembly


To build Rings Network for WebAssembly, run the following commands:

```sh
cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm
```

Or build with `wasm-pack`

```sh
wasm-pack build --scope ringsnetwork -t web --no-default-features --features browser --features console_error_panic_hook
```

### WASM package from NPM

```sh
npm i @ringsnetwork/rings-node
```


## Usage

```sh
rings <command> [options]
```

### Commands

- `help`: displays the usage information.
- `init`: creates a default configuration file named "config.toml" in the current directory. This file can be edited to customize the behavior of the rings-node daemon.
- `run`: runs the rings-node daemon. This command starts the daemon process, which will validate transactions, maintain the blockchain, and participate in consensus to earn rewards. By default, the daemon will use the "config.toml" file in the current directory for configuration. Use the "-c" or "--config" option to specify a custom configuration file.

### Options

- `-c, --config <FILE>`: specifies a custom configuration file to use instead of the default "config.toml". The configuration file is used to specify the network configuration, account settings, and other parameters that control the behavior of the rings-node daemon.
- `-h, --help`: displays the usage information.
- `-V, --version`: displays the version information for rings-node.



## Architecture

The Rings Network architecture is streamlined into five distinct layers.

```text
+-----------------------------------------------------------------------------------------+
|                                         RINGS                                           |
+-----------------------------------------------------------------------------------------+
|   Encrypted IM / Secret Sharing Storage / Distributed Content / Secret Data Exchange    |
+-----------------------------------------------------------------------------------------+
|                  SSSS                  |  Perdson Commitment/zkPod/ Secret Sharing      |
+-----------------------------------------------------------------------------------------+
|                     |       dDNS       |                 Sigma Protocol                 |
+      K-V Storage    +------------------+------------------------------------------------+
|                     |  Peer LOOKUP     |          MSRP        |  End-to-End Encryption  |
+----------------------------------------+------------------------------------------------+
|            Peer-to-Peer Network        |                                                |
|----------------------------------------+               DID / Resource ID                |
|                 Chord DHT              |                                                |
+----------------------------------------+------------------------------------------------+
|                Trickle SDP             |        ElGamal        | Persistence Storage    |
+----------------------------------------+-----------------------+------------------------+
|            STUN  | SDP  | ICE          |  Crosschain Binding   | Smart Contract Binding |
+----------------------------------------+------------------------------------------------+
|             SCTP | UDP | TCP           |             Finate Pubkey Group                |
+----------------------------------------+------------------------------------------------+
|   WASM Runtime    |                    |                                                |
+-------------------|  Operation System  |                   ECDSA                        |
|     Browser       |                    |                                                |
+-----------------------------------------------------------------------------------------+
```


### Runtime Layer

The design goal of Rings Network is to enable nodes to run in any environment, including browsers, mobile devices, Linux, Mac, Windows, etc. To achieve this, we have adopted a cross platform compile approach to build the code. For non-browser environments, we use pure Rust implementation that is independent of any system APIs, making our native implementation system agnostic. For browser environments, we compile the pure Rust implementation to WebAssembly (Wasm) and use web-sys, js-sys, and wasm-bindgen to glue it together, making our nodes fully functional in the browser.


### Transport Layer

 The implementation of WebRTC and WebAssembly in Rings Network provides several advantages for users. Firstly, the browser-based approach means that users do not need to install any additional software or plugins to participate in the network. Secondly, the use of WebRTC and WebAssembly enables the network to have a low latency and high throughput, making it suitable for real-time communication and data transfer.

 WebRTC, or Web Real-Time Communication, provides browsers and mobile apps with real-time communication capabilities through simple APIs. With WebRTC, users can easily send audio, video, and data streams directly between browsers, without the need for any plug-ins or extra software.
At the same time, the Rings Network has some special optimizations for the WebRTC handshakes process.

Assuming Node A and Node B want to create a WebRTC connection, they would need to exchange a minimum of three messages with each other:

#### ICE Scheme:

1. Peer A:
{
    create offer,
	set it as local description
} -> Send Offer to Peer B

2. Peer B: {
   set receiveed offer as remote description
   create answer
   set it as local description
   Send Answer to Peer A
}

3. Peer A: {
   Set receiveed answer as remote description
}

### Network Layer

The Rings Network is a structured peer-to-peer network that incorporates a distributed hash table (DHT) to facilitate efficient and scalable lookups. The Chord algorithm is utilized to implement the lookup function within the DHT, thereby enabling effective routing of messages and storage of key-value pairs in a peer-to-peer setting. The use of a DHT, incorporating the Chord algorithm, guarantees high availability in the Rings Network, which is critical for handling the substantial number of nodes and requests typically present in large-scale peer-to-peer networks.


### Protocol Layer

In the protocol layer, the central design concept revolves around the utilization of a Decentralized Identifier (DID), which constitutes a finite ring in abstract algebra. The DID is a $2^{160}$-bit identifier that enables the construction of a mathematical structure that encompasses the characteristics of both a group and a field. It is comprised of a set of elements with two binary operations, addition and multiplication, which satisfy a set of axioms such as associativity, commutativity, and distributivity. The ring is deemed finite due to its having a finite number of elements. Finite rings are widely employed in various domains of mathematics and computer science, including cryptography and coding theory.


### Application Layer

The nucleus of Rings Network is similar to the Actor Model, and it requires that each message type possess a Handler Trait. This allows for the separation of processing system messages, network messages, internal messages, and application-layer messages.


## Keywords

### Candidate

- A CANDIDATE is a transport address -- a combination of IP address and port for a particular transport protocol (with only UDP specified here).

- If an agent is multihomed, it obtains a candidate from each IP address.

- The agent uses STUN or TURN to obtain additional candidates. These come in two flavors: translated addresses on the public side of a NAT (SERVER REFLEXIVE CANDIDATES) and addresses on TURN servers (RELAYED CANDIDATES).


```text
                 To Internet

                     |
                     |
                     |  /------------  Relayed
                 Y:y | /               Address
                 +--------+
                 |        |
                 |  TURN  |
                 | Server |
                 |        |
                 +--------+
                     |
                     |
                     | /------------  Server
              X1':x1'|/               Reflexive
               +------------+         Address
               |    NAT     |
               +------------+
                     |
                     | /------------  Local
                 X:x |/               Address
                 +--------+
                 |        |
                 | Agent  |
                 |        |
                 +--------+

                     Figure 2: Candidate Relationships

```

### Channel

In the WebRTC framework, communication between the parties consists of media (for example, audio and video) and non-media data.

Non-media data is handled by using the Stream Control Transmission Protocol (SCTP) (RFC4960) encapsulated in DTLS.

```text
                               +----------+
                               |   SCTP   |
                               +----------+
                               |   DTLS   |
                               +----------+
                               | ICE/UDP  |
                               +----------+

```

The encapsulation of SCTP over DTLS (see RFC8261) over ICE/UDP (see RFC8445) provides a NAT traversal solution together with confidentiality, source authentication, and integrity-protected transfers.


 The layering of protocols for WebRTC is shown as:

```text
                                 +------+------+------+
                                 | DCEP | UTF-8|Binary|
                                 |      | Data | Data |
                                 +------+------+------+
                                 |        SCTP        |
                   +----------------------------------+
                   | STUN | SRTP |        DTLS        |
                   +----------------------------------+
                   |                ICE               |
                   +----------------------------------+
                   | UDP1 | UDP2 | UDP3 | ...         |
                   +----------------------------------+
```


## Contributing

We welcome contributions to rings-node!

If you have a bug report or feature request, please open an issue on GitHub.

If you'd like to contribute code, please follow these steps:

```text
    Fork the repository on GitHub.
    Create a new branch for your changes.
    Make your changes and commit them with descriptive commit messages.
    Push your changes to your fork.
    Create a pull request from your branch to the main repository.
```

We'll review your pull request as soon as we can, and we appreciate your contributions!


## Ref:

1. <https://datatracker.ietf.org/doc/html/rfc5245>

2. <https://datatracker.ietf.org/doc/html/draft-ietf-rtcweb-ip-handling-01>

3. <https://datatracker.ietf.org/doc/html/rfc8831>

4. <https://datatracker.ietf.org/doc/html/rfc8832>
