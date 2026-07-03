//! Peer synchronization helpers after connection handshakes.

use std::time::Duration;

use gloo_timers::future::sleep;
use yew::UseStateHandle;

use crate::node;
use crate::node::DemoNode;
use crate::node::PeerView;

pub(crate) const PEER_SETTLE_DELAYS_MS: &[u64] = &[0, 1_000, 2_000, 4_000];

pub(crate) async fn sync_peers_after_handshake(
    node: DemoNode,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
    context: &'static str,
    required_peer: Option<PeerView>,
) {
    status.set(format!("{context}; syncing peers"));
    if let Some(required_peer) = required_peer.as_ref() {
        peers.set(merge_required_peer((*peers).clone(), required_peer));
    }
    for delay_ms in PEER_SETTLE_DELAYS_MS {
        if *delay_ms > 0 {
            sleep(Duration::from_millis(*delay_ms)).await;
        }
        match node::list_peers(&node.provider).await {
            Ok(next) => {
                let next = if let Some(required_peer) = required_peer.as_ref() {
                    merge_required_peer(next, required_peer)
                } else {
                    next
                };
                let count = next.len();
                peers.set(next);
                status.set(peer_sync_status(context, count));
            }
            Err(error) => status.set(format!("{context}; peer sync failed: {error}")),
        }
    }
}

pub(crate) fn merge_required_peer(mut peers: Vec<PeerView>, required: &PeerView) -> Vec<PeerView> {
    if required.did.trim().is_empty() {
        return peers;
    }
    if !peers.iter().any(|peer| peer.did == required.did) {
        peers.insert(0, required.clone());
    }
    peers
}

pub(crate) fn peer_sync_status(context: &str, count: usize) -> String {
    match count {
        0 => format!("{context}; no peers visible yet"),
        1 => format!("{context}; 1 peer visible"),
        count => format!("{context}; {count} peers visible"),
    }
}
