//! Teardown of the local stream endpoints of destroyed circuits (#963, #966).

use std::collections::HashMap;

use super::pump::TcpStreamAbort;
use super::OnionTcpRuntime;
use super::TcpStreamKey;
use crate::error::Result;
use crate::extension::ext::LinkFact;
use crate::onion::circuit::OnionCircuitTeardown;
use crate::sync_lock::lock;

impl OnionTcpRuntime {
    /// End every local client and exit stream that `teardown` names.
    ///
    /// Post: no stream `teardown` ends remains; each has the cause [`stream_abort`] names
    /// recorded as its fate before its record, the lane's last sender, is released, so its pump
    /// ends aborted with that cause once the frames already queued are applied, a full lane
    /// included (`TcpStreamFate`). A client open still awaiting its answer fails at once
    /// (`TcpOpenResponseClosed`) instead of at its deadline.
    pub(in crate::onion) fn teardown(&self, teardown: OnionCircuitTeardown) -> Result<()> {
        let clients = take_matching(&mut *lock(&self.client_streams)?, |key, stream| {
            teardown.ends(stream.expected_return_peer, key.circuit_id)
        });
        for stream in clients {
            stream.inbound.fate.end(stream_abort(teardown));
        }
        let exits = take_matching(&mut *lock(&self.exit_streams)?, |key, stream| {
            teardown.ends(stream.expected_forward_peer, key.circuit_id)
        });
        for stream in exits {
            stream.inbound.fate.end(stream_abort(teardown));
        }
        Ok(())
    }
}

/// The cause a stream that `teardown` ends aborts with. After a drop here the link is up and the
/// adjacent hop has not observed the loss, so the stream aborts as `CellDropped`, a cause that
/// originates here: its end then destroys the circuit toward that hop (#966). Every other
/// teardown follows a destruction already known on that side.
fn stream_abort(teardown: OnionCircuitTeardown) -> TcpStreamAbort {
    match teardown {
        OnionCircuitTeardown::Link(LinkFact::Dropped(_)) => TcpStreamAbort::CellDropped,
        OnionCircuitTeardown::Link(LinkFact::Retired(_)) | OnionCircuitTeardown::Circuit { .. } => {
            TcpStreamAbort::CircuitDestroyed
        }
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
