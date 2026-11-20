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
    still_current: impl Fn() -> bool + 'static,
) {
    if !still_current() {
        return;
    }
    status.set(format!("{context}; syncing peers"));
    for delay_ms in PEER_SETTLE_DELAYS_MS {
        if *delay_ms > 0 {
            sleep(Duration::from_millis(*delay_ms)).await;
            if !still_current() {
                return;
            }
        }
        match node::list_peers(&node.provider).await {
            Ok(next) => {
                if !still_current() {
                    return;
                }
                let count = next.len();
                peers.set(next);
                status.set(peer_sync_status(context, count));
                if count > 0 {
                    break;
                }
            }
            Err(error) => {
                if still_current() {
                    status.set(format!("{context}; peer sync failed: {error}"));
                }
            }
        }
    }
}

pub(crate) fn peer_sync_status(context: &str, count: usize) -> String {
    match count {
        0 => format!("{context}; no peers visible yet"),
        1 => format!("{context}; 1 peer visible"),
        count => format!("{context}; {count} peers visible"),
    }
}

pub(crate) fn refresh_waiting_peer_status(current: &str, count: usize) -> Option<String> {
    if count == 0 {
        return None;
    }
    let context = current.strip_suffix("; no peers visible yet")?;
    Some(peer_sync_status(context, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_peer_sync_status_reports_admitted_peer_count() {
        assert_eq!(
            peer_sync_status("seed URL connected", 0),
            "seed URL connected; no peers visible yet"
        );
        assert_eq!(
            peer_sync_status("seed URL connected", 1),
            "seed URL connected; 1 peer visible"
        );
        assert_eq!(
            peer_sync_status("HTTP endpoint connected", 2),
            "HTTP endpoint connected; 2 peers visible"
        );
        assert_eq!(
            refresh_waiting_peer_status("seed URL connected; no peers visible yet", 0),
            None
        );
        assert_eq!(
            refresh_waiting_peer_status("seed URL connected; no peers visible yet", 1),
            Some("seed URL connected; 1 peer visible".to_string())
        );
        assert_eq!(
            refresh_waiting_peer_status("background node ready", 1),
            None
        );
    }
}
