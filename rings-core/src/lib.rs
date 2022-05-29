//! Rings: Browser-native p2p implentation with Chord and WebRTC.
//! --------------
//! - [Chord](crate::dht::PeerRing) is a structured p2p network based on Chord protocol, and expanded with secp256k1 based DID supporting.
//! - [ElGamal](crate::ecc::elgamal) provide a End2End encryption based on Chord DID
//! - [Swarm](crate::swarm) a the module for manage all transports
//! - [transports](crate::transports::Transport) is used for connection handshaking, which is support all platform, include browser (wasm) and native runtime

//! # Connection
//! There are three phase when a node A join a DHT Ring / or connected to Node B.
//! 1. handshake
//! - Node A should create a new transport via `swarm.new_transport()` and generate the handshake SDP with
//!   `transport.get_handshake_info(session_manager, Offer)` and send it to Node B
//! - Node B should accept the Offer with `transport.register_remote_info(offer)` and response with Answer via
//!   `transport.get_handshake_info(session_manager, Offer)`
//! - Node A accepted the answer, and wait until connection created.
//! 2. JoinRing
//! - After the connection was created, Node A will ask Node B for successor
//!   (A successor is the cloest Node on the Ring for Node A.)
//!   if Node B known other Node X can be treat as successor of Node A, it will response it's DID
//! 3. e2e encrypt
//! - After successfull joint Ring, all direct message should be encrypted with ElGamal algorithm.
//!
//! # MessageRelay
//! - MSRP over WebRTC Data Channel is Published at Jan/2021, which is based on The Message Session Relay Protocol, and it's extension.
//! - To implement MSRP over WebRTC, it's necessary to handle SCTP transport of peer connection (text data channel based on SCTP). For webrtc-rs there is already an implementation, but unfortunately, it is not widely supported in browser environment, (we can modify web-sys to support it, it may easy, but it still won't work in Firefox).
//! So the best solution of message relay is to create a similar protocol like MSRP.
//! - Out message relay protocol is similiar to MSRP(RFC8873), all relay message should have it's path data:
//! to_path, and from_path.
//! - If a message is send from A to Z over Ring, when a relay Node X got the message, the path data of the relay message may looks like:
//! `from_path[A, B, C, D] to_path[Z]`
//! - Node X must append itself to the `from_path` list: `from_path[A, B, C, X]`
//! - When Z received the relay message, the message is simply responsable without any query on DHT -- just reverse the path.
//!
//! # ECDSA Session
//! To avoid too frequent signing, and keep the private key safe, we implemented a session protocol for signing/encrypting and verifying message.
//! - ECDSA Session is based on secp256k1, which create a temporate secret key with one time signing auth
//! - To create a ECDSA Session, we should generate the unsign_info with our pubkey (Address)
//! - `SessionManager::gen_unsign_info(addr, ..)`, it will returns the msg needs for sign, and a temporate private key
//! - Then we can sign the auth message via some web3 provider like metamask or just with raw private key, and create the SessionManger with
//! - SessionManager::new(sig, auth_info, temp_key)

//! # WASM Supported
//! - To build for WASM
//! - `cargo build -p rings-core --target=wasm32-unknown-unknown --features wasm --no-default-features`

#![feature(associated_type_defaults)]
#![feature(async_closure)]
#![feature(box_syntax)]
#![feature(derive_default_enum)]
#![feature(generators)]
pub mod channels;
pub mod dht;
pub mod ecc;
pub mod err;
pub mod macros;
pub mod message;
pub mod prelude;
pub mod session;
pub mod storage;
pub mod swarm;
pub mod transports;
pub mod types;
pub mod utils;

pub use async_trait::async_trait;
pub use futures;
