//! Teardown of the local stream endpoints of destroyed circuits (#963).

use std::collections::HashMap;

use super::inbound::TcpInbound;
use super::OnionTcpRuntime;
use super::TcpStreamKey;
use crate::error::Result;
use crate::onion::circuit::OnionCircuitTeardown;
use crate::sync_lock::lock;

impl OnionTcpRuntime {
    /// End every local client and exit stream that `teardown` names.
    ///
    /// Post: no stream `teardown` ends remains; each was told `Destroyed`, so its pump ends
    /// aborted and reports it, and a client open still awaiting its answer fails at once
    /// (`TcpOpenResponseClosed`) instead of at its deadline. A pump whose inbound lane was full
    /// still ends aborted: releasing the stream closes its last sender.
    pub(in crate::onion) fn teardown(&self, teardown: OnionCircuitTeardown) -> Result<()> {
        let clients = take_matching(&mut *lock(&self.client_streams)?, |key, stream| {
            teardown.ends(stream.expected_return_peer, key.circuit_id)
        });
        for stream in clients {
            let _ = stream.tx.try_send(TcpInbound::Destroyed);
        }
        let exits = take_matching(&mut *lock(&self.exit_streams)?, |key, stream| {
            teardown.ends(stream.expected_forward_peer, key.circuit_id)
        });
        for stream in exits {
            let _ = stream.tx.try_send(TcpInbound::Destroyed);
        }
        Ok(())
    }
}

/// Remove and return every entry of `streams` that `ends` holds for.
fn take_matching<V>(
    streams: &mut HashMap<TcpStreamKey, V>,
    ends: impl Fn(&TcpStreamKey, &V) -> bool,
) -> Vec<V> {
    let keys = streams
        .iter()
        .filter(|(key, stream)| ends(key, stream))
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| streams.remove(&key))
        .collect()
}
