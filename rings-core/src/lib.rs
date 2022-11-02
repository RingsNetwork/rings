//! Rings: Chord based P2P implentation over WebRTC and ElGamal.
//! --------------
//! - [Chord](crate::dht::PeerRing) is a structured p2p network based on Chord protocol and expanded with secp256k1 based Did support.
//! - [ElGamal](crate::ecc::elgamal) provides End2End encryption based on Chord Did.
//! - [Swarm](crate::swarm) is a module for managing all transports.
//! - [Transport](crate::transports::Transport) is used for connection handshaking, which supports all platforms, including browser (wasm) and native runtime.

//! # Connection
//!
//! There are three phases when node A connects to node B. So-called join a DHT Ring.
//!
//! 1. Handshake
//! - Node A create a new transport via `swarm.new_transport()` and generate the handshake SDP
//!   with `transport.get_handshake_info(session_manager, offer)` and send it to node B.
//! - Node B accept the offer with `transport.register_remote_info(offer)` and response with Answer via
//!   `transport.get_handshake_info(session_manager, offer)`.
//! - Node A accept the answer and wait until the connection creation.
//! 2. Join Ring
//! - After the connection creation, node A will ask node B for a successor.
//!   (A successor is the closest node on the Ring for node A.)
//!   If node B know another node X could be the successor of node A, it will respond with the `Did` of node X.
//! 3. E2e encrypt
//! - After joining Ring, should encrypt all direct messages with the ElGamal algorithm.
//!
//! # MessagePayload
//!
//! MSRP over WebRTC Data Channel is Published in Jan/2021, which is based on The Message Session Relay Protocol and its extension.
//!
//! To implement MSRP over WebRTC, it's necessary to handle SCTP transport of peer connection (text data channel based on SCTP).
//! For webrtc-rs, there is already an implementation. But unfortunately, it is not widely supported in the browser environment.
//! (We can modify web-sys to support it, it may be easy, but it still won't work in Firefox.)
//! So the best solution for message relay is to create a similar protocol to MSRP.
//!
//! The message relay protocol is similar to MSRP(RFC8873). All relay messages should have path data fields: `path`, `destination`.
//! If a message is sent from A to Z over Ring when a relay node X got the message, the path data of the relay message may look like this:
//!
//! ```txt
//! path:[A, B, C, D] destination: Z
//! ```
//!
//! Node X must append itself to the `path` list: `path[A, B, C, X]`.
//!
//! When node Z receives the relay message, node Z can respond without any query on DHT -- Just reverse the path.
//!
//! # ECDSA Session
//!
//! To avoid too frequent signing, and keep the private key safe, we implemented a session protocol for signing/encrypting and verifying messages.
//!
//! ECDSA Session is based on secp256k1.
//! - ECDSA Session is based on secp256k1.
//!   ECDSA Session creates a temporary secret key with one-time signing auth.
//! - To create a ECDSA Session, we should generate the unsign_info with our pubkey (Address).
//!   `SessionManager::gen_unsign_info(addr, ..)`, it will return the msg needs for signing, and a temporary private key.
//! - Then we can sign the auth message via some web3 provider like metamask or just with a raw private key, and create the SessionManger with
//!   `SessionManager::new(sig, auth_info, temp_key)`.

//! # WASM Supported
//! ```shell
//! cargo build -p rings-core --target=wasm32-unknown-unknown --features wasm --no-default-features
//! ```

#![feature(associated_type_defaults)]
#![feature(async_closure)]
#![feature(iter_array_chunks)]
#![feature(box_syntax)]
#![feature(generators)]
#![feature(slice_group_by)]
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
#[cfg(test)]
mod tests;
pub mod transports;
pub mod types;
pub mod utils;
pub use async_trait::async_trait;
pub use futures;
pub mod chunk;
pub mod consts;
