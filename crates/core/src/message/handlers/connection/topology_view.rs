use crate::dht::successor::SuccessorReader;
use crate::dht::topology;
use crate::dht::Did;
use crate::dht::PeerRing;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::dht::TopoInfo;
use crate::error::Result;

#[cfg(all(test, not(target_family = "wasm")))]
pub(super) fn confirmed_topology(info: &TopoInfo, is_active: impl Fn(Did) -> bool) -> TopoInfo {
    info.confirmed_by(is_active)
}

#[cfg(all(test, not(target_family = "wasm")))]
pub(super) fn topology_has_confirmed_peer(info: &TopoInfo) -> bool {
    info.has_confirmed_peer()
}

pub(super) fn connect_successor_hint(dht: &PeerRing, requester: Did, reported: Did) -> Result<Did> {
    if reported != requester {
        return Ok(reported);
    }

    let mut candidates = dht.successors().list()?;
    candidates.push(dht.did);
    candidates.retain(|candidate| *candidate != requester);

    Ok(topology::successors(&candidates, requester, 1)
        .into_iter()
        .next()
        .unwrap_or(reported))
}
