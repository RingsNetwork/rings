//! Multi-hop TURN onion payload permutation (`rings-aez` at `τ = 0`) and formal state-machine
//! model (`TurnChainModel`) for chained TURN relays (#1292).
//!
//! Because browser `RTCPeerConnection` and native `webrtc-rs` require standard RFC 5766 TURN wire
//! framing without byte expansion, each hop in a multi-hop TURN chain applies a length-preserving
//! wide-block tweakable permutation (`rings-aez` with `τ = 0`).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::SocketAddr;

use rings_aez::Aez;
use rings_aez::Tweak;
use rings_aez::KEY_BYTES;
use rings_core::error::Error as CoreError;
use serde::Deserialize;
use serde::Serialize;

/// Direction of a single `rings-aez` (`τ = 0`) wide-block permutation layer on the outbound path
/// (client toward exit peer). The inbound path (exit peer toward client) automatically applies the
/// inverse direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnCipherDirection {
    /// Apply `Aez::encipher` on outbound and `Aez::decipher` on inbound.
    Encipher,
    /// Apply `Aez::decipher` on outbound and `Aez::encipher` on inbound.
    Decipher,
}

impl TurnCipherDirection {
    /// Return the inverse permutation direction.
    pub const fn inverse(self) -> Self {
        match self {
            Self::Encipher => Self::Decipher,
            Self::Decipher => Self::Encipher,
        }
    }
}

struct CompiledCipherLayer {
    aez: Aez,
    raw_key: [u8; KEY_BYTES],
    outbound_direction: TurnCipherDirection,
}

/// Composable stack of length-preserving `rings-aez` (`τ = 0`) permutation layers installed on a
/// single TURN allocation or chained hop (#1292).
///
/// Laws:
/// - **Length Preservation**: `transform_outbound` and `transform_inbound` mutate `buffer` in place
///   (`|out| == |in|` for all `|in| >= 0`).
/// - **Exact Involution / Bijection**: For any layer stack `S`,
///   `transform_inbound(transform_outbound(M)) == M` and
///   `transform_outbound(transform_inbound(M)) == M`.
pub struct TurnPayloadCipher {
    layers: Vec<CompiledCipherLayer>,
}

impl Clone for TurnPayloadCipher {
    fn clone(&self) -> Self {
        let mut layers = Vec::with_capacity(self.layers.len());
        for layer in &self.layers {
            if let Ok(aez) = Aez::new(&layer.raw_key) {
                layers.push(CompiledCipherLayer {
                    aez,
                    raw_key: layer.raw_key,
                    outbound_direction: layer.outbound_direction,
                });
            }
        }
        Self { layers }
    }
}

impl std::fmt::Debug for TurnPayloadCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnPayloadCipher")
            .field("layer_count", &self.layers.len())
            .finish()
    }
}

impl TurnPayloadCipher {
    /// Construct an identity (0-layer) payload cipher.
    pub const fn identity() -> Self {
        Self { layers: Vec::new() }
    }

    /// Construct a single-layer AEZ (`τ = 0`) payload cipher.
    pub fn single(
        key: &[u8; KEY_BYTES],
        outbound_direction: TurnCipherDirection,
    ) -> std::result::Result<Self, CoreError> {
        let aez = Aez::new(key)
            .map_err(|e| CoreError::MessageEncryptionFailed(format!("invalid AEZ key: {e}")))?;
        Ok(Self {
            layers: vec![CompiledCipherLayer {
                aez,
                raw_key: *key,
                outbound_direction,
            }],
        })
    }

    /// Construct a multi-layer AEZ (`τ = 0`) payload cipher from an ordered sequence of
    /// `(key, outbound_direction)` pairs.
    pub fn from_layers(
        layers: impl IntoIterator<Item = ([u8; KEY_BYTES], TurnCipherDirection)>,
    ) -> std::result::Result<Self, CoreError> {
        let mut compiled = Vec::new();
        for (raw_key, outbound_direction) in layers {
            let aez = Aez::new(&raw_key)
                .map_err(|e| CoreError::MessageEncryptionFailed(format!("invalid AEZ key: {e}")))?;
            compiled.push(CompiledCipherLayer {
                aez,
                raw_key,
                outbound_direction,
            });
        }
        Ok(Self { layers: compiled })
    }

    /// Return the number of AEZ permutation layers in this cipher.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Export the serialized `(key, outbound_direction)` layers for control-plane transport.
    pub fn to_layer_specs(&self) -> Vec<([u8; KEY_BYTES], TurnCipherDirection)> {
        self.layers
            .iter()
            .map(|l| (l.raw_key, l.outbound_direction))
            .collect()
    }

    /// Apply the outbound permutation stack in forward order (`L_1 -> ... -> L_m`) in place.
    pub fn transform_outbound(&self, buffer: &mut [u8]) {
        if buffer.is_empty() {
            return;
        }
        for layer in &self.layers {
            match layer.outbound_direction {
                TurnCipherDirection::Encipher => layer.aez.encipher(Tweak::EMPTY, buffer),
                TurnCipherDirection::Decipher => layer.aez.decipher(Tweak::EMPTY, buffer),
            }
        }
    }

    /// Apply the inverse inbound permutation stack in reverse order (`L_m^-1 -> ... -> L_1^-1`)
    /// in place.
    pub fn transform_inbound(&self, buffer: &mut [u8]) {
        if buffer.is_empty() {
            return;
        }
        for layer in self.layers.iter().rev() {
            match layer.outbound_direction.inverse() {
                TurnCipherDirection::Encipher => layer.aez.encipher(Tweak::EMPTY, buffer),
                TurnCipherDirection::Decipher => layer.aez.decipher(Tweak::EMPTY, buffer),
            }
        }
    }
}

/// Derive per-hop [`TurnPayloadCipher`] schedules for an `N`-hop TURN onion chain
/// (`T_1 -> T_2 -> ... -> T_N`) serving an unmodified WebRTC client (such as browser
/// `RTCPeerConnection` or `webrtc-rs`).
///
/// Given `N - 1` link keys `[K_1, ..., K_{N-1}]` (one per inter-relay hop `T_2 .. T_N`):
/// - Entry guard `T_1` wraps outbound WebRTC datagrams in all `N - 1` onion layers
///   `[(K_{N-1}, Encipher), ..., (K_1, Encipher)]` and unwraps inbound datagrams in reverse order.
/// - Each subsequent hop `T_{i+1}` (`i in 1..=N-1`) holds only its single key `K_i` with
///   `[(K_i, Decipher)]` on outbound and `[(K_i, Encipher)]` on inbound.
/// - Therefore:
///   1. Every inter-relay UDP link `T_i <-> T_{i+1}` carries distinct AEZ-permuted ciphertext.
///   2. No single hop `T_2 .. T_N` knows the keys of earlier hops.
///   3. Exit `T_N` peels the final layer `K_{N-1}` so the target WebRTC peer receives the exact
///      original STUN/DTLS/SCTP datagram without byte expansion (`τ = 0`).
pub fn derive_entry_wrapped_hop_ciphers(
    link_keys: &[[u8; KEY_BYTES]],
) -> std::result::Result<Vec<TurnPayloadCipher>, CoreError> {
    let mut hop_ciphers = Vec::with_capacity(link_keys.len() + 1);
    let entry_layers = link_keys
        .iter()
        .rev()
        .map(|k| (*k, TurnCipherDirection::Encipher));
    hop_ciphers.push(TurnPayloadCipher::from_layers(entry_layers)?);
    for key in link_keys {
        hop_ciphers.push(TurnPayloadCipher::single(
            key,
            TurnCipherDirection::Decipher,
        )?);
    }
    Ok(hop_ciphers)
}

/// Abstract state of one hop in the formal `N`-hop TURN chain model (#1292).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TurnHopModelState {
    /// Whether this hop's allocation is currently active.
    pub allocated: bool,
    /// The `XOR-RELAYED-ADDRESS` projected by this hop to its upstream caller.
    pub projected_relay_addr: Option<SocketAddr>,
    /// Installed peer IP permissions at this hop.
    pub permissions: BTreeSet<IpAddr>,
    /// Active `(ChannelNumber -> SocketAddr)` bindings at this hop.
    pub channels: BTreeMap<u16, SocketAddr>,
}

/// Abstract transition action in the formal `N`-hop TURN chain model (#1292).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnChainAction {
    /// Reserve from exit `T_N` down to entry `T_1` and allocate `T_1`.
    AllocateChain,
    /// Cascade `CreatePermission(peer_ip)` from `T_1` through `T_N`, failing at `fail_at_hop` if
    /// `Some(idx)` (`0`-based).
    CreatePermission {
        /// Peer IP to authorize.
        peer_ip: IpAddr,
        /// Optional fault-injected hop index where permission installation fails.
        fail_at_hop: Option<usize>,
    },
    /// Cascade `ChannelBind(channel, peer)` from `T_1` through `T_N`.
    ChannelBind {
        /// RFC 5766 channel number (`0x4000..=0x7FFF`).
        channel: u16,
        /// Peer socket address to bind.
        peer: SocketAddr,
    },
    /// Tear down the chain via `Refresh(0)`.
    CloseChain,
}

/// Formal state-machine model of an `N`-hop chained TURN relay circuit (#1292).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TurnChainModel {
    /// Number of TURN relay hops `N >= 1`.
    pub n_hops: usize,
    /// Concrete exit relay address `E_exit` allocated on `T_N`.
    pub exit_relay_addr: SocketAddr,
    /// Per-hop states `[T_1, ..., T_N]`.
    pub hops: Vec<TurnHopModelState>,
}

impl TurnChainModel {
    /// Construct an initial unallocated `N`-hop TURN chain model with exit relay address
    /// `exit_relay_addr`.
    pub fn new(n_hops: usize, exit_relay_addr: SocketAddr) -> Self {
        let hops = (0..n_hops)
            .map(|_| TurnHopModelState {
                allocated: false,
                projected_relay_addr: None,
                permissions: BTreeSet::new(),
                channels: BTreeMap::new(),
            })
            .collect();
        Self {
            n_hops,
            exit_relay_addr,
            hops,
        }
    }

    /// Execute one atomic or fault-injected [`TurnChainAction`], returning `(next_state, succeeded)`.
    pub fn step(&self, action: &TurnChainAction) -> (Self, bool) {
        let mut next = self.clone();
        match action {
            TurnChainAction::AllocateChain => {
                if next.hops.iter().any(|h| h.allocated) {
                    return (next, false);
                }
                for hop in &mut next.hops {
                    hop.allocated = true;
                    hop.projected_relay_addr = Some(next.exit_relay_addr);
                }
                (next, true)
            }
            TurnChainAction::CreatePermission {
                peer_ip,
                fail_at_hop,
            } => {
                if !next.hops.iter().all(|h| h.allocated) {
                    return (next, false);
                }
                if let Some(fail_idx) = *fail_at_hop {
                    if fail_idx < next.n_hops {
                        // Transactional cascading: if an upstream hop rejects CreatePermission,
                        // downstream hops do not commit the permission and the client receives an
                        // error response.
                        return (self.clone(), false);
                    }
                }
                for hop in &mut next.hops {
                    hop.permissions.insert(*peer_ip);
                }
                (next, true)
            }
            TurnChainAction::ChannelBind { channel, peer } => {
                if !next.hops.iter().all(|h| h.allocated) {
                    return (next, false);
                }
                for hop in &next.hops {
                    for (&bound_ch, &bound_peer) in &hop.channels {
                        if (bound_ch == *channel && bound_peer != *peer)
                            || (bound_peer == *peer && bound_ch != *channel)
                        {
                            return (self.clone(), false);
                        }
                    }
                }
                for hop in &mut next.hops {
                    hop.permissions.insert(peer.ip());
                    hop.channels.insert(*channel, *peer);
                }
                (next, true)
            }
            TurnChainAction::CloseChain => {
                if !next.hops.iter().any(|h| h.allocated) {
                    return (next, false);
                }
                for hop in &mut next.hops {
                    hop.allocated = false;
                    hop.projected_relay_addr = None;
                    hop.permissions.clear();
                    hop.channels.clear();
                }
                (next, true)
            }
        }
    }

    /// Return whether a datagram to/from `peer` can traverse all `N` hops.
    pub fn can_forward_datagram(&self, peer: SocketAddr) -> bool {
        self.hops
            .iter()
            .all(|h| h.allocated && h.permissions.contains(&peer.ip()))
    }

    /// Verify all safety invariants on `self`:
    /// 1. **Identity Projection**: `\forall i: allocated_i => projected_relay_addr_i == Some(E_exit)`
    /// 2. **Lockstep Allocation & Permission Coherence**: all hops share the same allocation state,
    ///    permission set, and channel map.
    /// 3. **Forwarding Soundness**: `can_forward_datagram(peer) <=> allocated_1 && peer.ip in permissions_1`.
    pub fn check_invariants(&self, candidate_peers: &[SocketAddr]) -> bool {
        let Some(first) = self.hops.first() else {
            return true;
        };
        for hop in &self.hops {
            if hop.allocated != first.allocated {
                return false;
            }
            if hop.allocated && hop.projected_relay_addr != Some(self.exit_relay_addr) {
                return false;
            }
            if !hop.allocated && hop.projected_relay_addr.is_some() {
                return false;
            }
            if hop.permissions != first.permissions || hop.channels != first.channels {
                return false;
            }
            for bound_peer in hop.channels.values() {
                if !hop.permissions.contains(&bound_peer.ip()) {
                    return false;
                }
            }
        }
        for &peer in candidate_peers {
            let expected = first.allocated && first.permissions.contains(&peer.ip());
            if self.can_forward_datagram(peer) != expected {
                return false;
            }
        }
        true
    }

    /// Exhaustively explore the reachable state space of an `n_hops` TURN chain via BFS and assert
    /// that all safety invariants hold in every reachable state. Returns the number of reachable
    /// states verified.
    pub fn verify_exhaustive(n_hops: usize) -> usize {
        let exit_addr: SocketAddr = "203.0.113.99:54321".parse().unwrap_or_else(|_| {
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 99)), 54321)
        });
        let peer_a: SocketAddr = "198.51.100.10:6000".parse().unwrap_or_else(|_| {
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 10)), 6000)
        });
        let peer_b: SocketAddr = "198.51.100.20:6001".parse().unwrap_or_else(|_| {
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 20)), 6001)
        });
        let candidate_peers = [peer_a, peer_b];

        let mut actions = vec![TurnChainAction::AllocateChain, TurnChainAction::CloseChain];
        for peer in candidate_peers {
            actions.push(TurnChainAction::CreatePermission {
                peer_ip: peer.ip(),
                fail_at_hop: None,
            });
            for hop_idx in 0..n_hops {
                actions.push(TurnChainAction::CreatePermission {
                    peer_ip: peer.ip(),
                    fail_at_hop: Some(hop_idx),
                });
            }
            actions.push(TurnChainAction::ChannelBind {
                channel: 0x4000,
                peer,
            });
            actions.push(TurnChainAction::ChannelBind {
                channel: 0x4001,
                peer,
            });
        }

        let init = Self::new(n_hops, exit_addr);
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        visited.insert(init.clone());
        queue.push_back(init);

        while let Some(state) = queue.pop_front() {
            assert!(
                state.check_invariants(&candidate_peers),
                "invariant violated in state: {state:?}"
            );
            for action in &actions {
                let (next, _) = state.step(action);
                assert!(
                    next.check_invariants(&candidate_peers),
                    "invariant violated after action {action:?}: {next:?}"
                );
                if visited.insert(next.clone()) {
                    queue.push_back(next);
                }
            }
        }
        visited.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_aez_key(seed: u8) -> [u8; KEY_BYTES] {
        let mut key = [0u8; KEY_BYTES];
        for (idx, byte) in key.iter_mut().enumerate() {
            *byte = seed.wrapping_add(u8::try_from(idx).unwrap_or(0)).max(1);
        }
        key
    }

    #[test]
    fn turn_chain_model_exhaustive_verification_holds_for_1_2_and_3_hops() {
        for n_hops in 1..=3 {
            let states = TurnChainModel::verify_exhaustive(n_hops);
            assert!(
                states >= 10,
                "expected non-trivial reachable state space for {n_hops}-hop chain, got {states}"
            );
        }
    }

    #[test]
    fn entry_wrapped_aez_ciphers_preserve_length_and_bijection_across_n_hops() {
        let link_keys = [
            sample_aez_key(0x11),
            sample_aez_key(0x37),
            sample_aez_key(0x73),
        ];
        let hop_ciphers = derive_entry_wrapped_hop_ciphers(&link_keys).unwrap();
        assert_eq!(hop_ciphers.len(), 4);

        for len in [0usize, 1, 7, 16, 20, 64, 512, 1200] {
            let original: Vec<u8> = (0..len)
                .map(|i| u8::try_from((i * 131 + 17) & 0xFF).unwrap_or(0))
                .collect();
            let mut wire = original.clone();

            // Outbound: T_1 wraps, T_2..T_4 each peel one layer.
            let mut intermediate_wires = Vec::new();
            for (hop_idx, cipher) in hop_ciphers.iter().enumerate() {
                cipher.transform_outbound(&mut wire);
                assert_eq!(wire.len(), original.len(), "AEZ tau=0 must preserve length");
                if hop_idx + 1 < hop_ciphers.len() && original.len() >= 16 {
                    assert_ne!(
                        wire, original,
                        "inter-relay wire payload at hop {hop_idx} must not equal plaintext"
                    );
                    for prev in &intermediate_wires {
                        assert_ne!(
                            &wire, prev,
                            "inter-relay wire payloads must be pairwise distinct across hops"
                        );
                    }
                    intermediate_wires.push(wire.clone());
                }
            }
            assert_eq!(
                wire, original,
                "exit hop T_N must recover exact plaintext WebRTC datagram"
            );

            // Inbound: T_4..T_2 each wrap one layer, T_1 unwraps all layers.
            for cipher in hop_ciphers.iter().rev() {
                cipher.transform_inbound(&mut wire);
                assert_eq!(wire.len(), original.len());
            }
            assert_eq!(
                wire, original,
                "entry hop T_1 must recover exact inbound datagram for client"
            );
        }
    }
}
