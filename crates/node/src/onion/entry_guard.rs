//! Local onion entry-guard selection state.
//!
//! Entry guards are client-local privacy state. They deliberately live outside
//! Chord and are persisted only in the node's local storage backend.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rings_core::dht::Did;
use rings_core::measure::PeerQuality;
use rings_core::storage::KvStorageInterface;
use rings_core::utils::get_epoch_ms;
use serde::Deserialize;
use serde::Serialize;

use super::route::pick_weighted_index;
use super::route::RouteEntropy;
use super::OnionRouteHop;
use crate::error::Error;
use crate::error::Result;

const ENTRY_GUARD_SCHEMA_VERSION: u16 = 1;
const ENTRY_GUARD_KEY_PREFIX: &str = "rings-node:onion-entry-guards:v1";
const DEFAULT_ENTRY_GUARD_COUNT: usize = 3;

/// Serialized local entry-guard snapshot.
///
/// This type is public so native and browser storage adapters can name the
/// concrete key-value value type, but its fields remain private because callers
/// should not construct or interpret guard state directly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OnionEntryGuardState {
    version: u16,
    network_id: u32,
    guards: Vec<OnionEntryGuardRecord>,
}

/// Local key-value storage for persisted onion entry guards.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub type OnionEntryGuardStorage = Box<dyn KvStorageInterface<OnionEntryGuardState>>;

/// Local key-value storage for persisted onion entry guards.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub type OnionEntryGuardStorage = Box<dyn KvStorageInterface<OnionEntryGuardState> + Send + Sync>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct OnionEntryGuardRecord {
    did: Did,
    selected_at_ms: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionEntryGuardSet {
    dids: BTreeSet<Did>,
}

impl OnionEntryGuardSet {
    pub(crate) fn contains(&self, did: Did) -> bool {
        self.dids.contains(&did)
    }
}

pub(crate) struct OnionEntryGuards {
    storage: OnionEntryGuardStorage,
    target_guard_count: usize,
}

impl OnionEntryGuards {
    pub(crate) fn new(storage: OnionEntryGuardStorage) -> Self {
        Self::new_with_target_count(storage, DEFAULT_ENTRY_GUARD_COUNT)
    }

    pub(crate) fn new_with_target_count(
        storage: OnionEntryGuardStorage,
        target_guard_count: usize,
    ) -> Self {
        Self {
            storage,
            target_guard_count: target_guard_count.max(1),
        }
    }

    pub(crate) async fn select_relay_guards(
        &self,
        network_id: u32,
        relays: &[OnionRouteHop],
        qualities: &[(Did, PeerQuality)],
        entropy: &mut impl RouteEntropy,
        first_hop_permitted: impl Fn(Did) -> bool,
    ) -> Result<OnionEntryGuardSet> {
        let key = entry_guard_state_key(network_id);
        let stored = self
            .storage
            .get(&key)
            .await
            .map_err(Error::Storage)?
            .filter(|state| state.matches_network(network_id));
        let quality_by_did = qualities.iter().copied().collect::<BTreeMap<_, _>>();
        let eligible_dids = eligible_guard_dids(relays, &quality_by_did, &first_hop_permitted);
        let next = reconcile_guard_state(
            network_id,
            stored,
            eligible_dids,
            &quality_by_did,
            self.target_guard_count,
            get_epoch_ms(),
            entropy,
        );
        self.persist_if_changed(&key, &next.previous, &next.state)
            .await?;
        Ok(next.set)
    }

    async fn persist_if_changed(
        &self,
        key: &str,
        previous: &Option<OnionEntryGuardState>,
        state: &OnionEntryGuardState,
    ) -> Result<()> {
        if previous.as_ref() != Some(state) {
            self.storage.put(key, state).await.map_err(Error::Storage)?;
        }
        Ok(())
    }
}

impl OnionEntryGuardState {
    fn matches_network(&self, network_id: u32) -> bool {
        self.version == ENTRY_GUARD_SCHEMA_VERSION && self.network_id == network_id
    }
}

struct ReconciledGuards {
    previous: Option<OnionEntryGuardState>,
    state: OnionEntryGuardState,
    set: OnionEntryGuardSet,
}

fn reconcile_guard_state(
    network_id: u32,
    previous: Option<OnionEntryGuardState>,
    eligible_dids: BTreeSet<Did>,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    target_guard_count: usize,
    now_ms: u128,
    entropy: &mut impl RouteEntropy,
) -> ReconciledGuards {
    let mut retained = previous
        .as_ref()
        .map(|state| state.guards.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|guard| eligible_dids.contains(&guard.did))
        .cloned()
        .collect::<Vec<_>>();
    let mut guard_dids = retained
        .iter()
        .map(|guard| guard.did)
        .collect::<BTreeSet<_>>();

    while retained.len() < target_guard_count {
        let remaining = eligible_dids
            .iter()
            .copied()
            .filter(|did| !guard_dids.contains(did))
            .collect::<Vec<_>>();
        let Some(index) = pick_weighted_index(&remaining, quality_by_did, entropy) else {
            break;
        };
        let Some(did) = remaining.get(index).copied() else {
            break;
        };
        retained.push(OnionEntryGuardRecord {
            did,
            selected_at_ms: now_ms,
        });
        guard_dids.insert(did);
    }

    let state = OnionEntryGuardState {
        version: ENTRY_GUARD_SCHEMA_VERSION,
        network_id,
        guards: retained,
    };
    let dids = state.guards.iter().map(|guard| guard.did).collect();
    ReconciledGuards {
        previous,
        state,
        set: OnionEntryGuardSet { dids },
    }
}

fn eligible_guard_dids(
    relays: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    first_hop_permitted: &impl Fn(Did) -> bool,
) -> BTreeSet<Did> {
    let live = relays
        .iter()
        .map(|hop| hop.did)
        .filter(|did| first_hop_permitted(*did))
        .collect::<BTreeSet<_>>();
    let preferred = live
        .iter()
        .copied()
        .filter(|did| quality_by_did.get(did) != Some(&PeerQuality::Degraded))
        .collect::<BTreeSet<_>>();

    if preferred.is_empty() {
        live
    } else {
        preferred
    }
}

fn entry_guard_state_key(network_id: u32) -> String {
    format!("{ENTRY_GUARD_KEY_PREFIX}:network:{network_id}")
}

#[cfg(all(test, not(all(feature = "browser", target_family = "wasm"))))]
mod tests {
    use std::collections::VecDeque;

    use rings_core::ecc::SecretKey;
    use rings_core::storage::file::FileStorage;
    use rings_core::storage::MemStorage;
    use uuid::Uuid;

    use super::*;
    use crate::prelude::SessionSk;

    struct FixedEntropy {
        values: VecDeque<u64>,
    }

    impl FixedEntropy {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self {
                values: values.into_iter().collect(),
            }
        }
    }

    impl RouteEntropy for FixedEntropy {
        fn next_u64(&mut self) -> u64 {
            self.values.pop_front().unwrap_or(0)
        }
    }

    fn relay() -> Result<OnionRouteHop> {
        let session_sk = SessionSk::new_with_seckey(&SecretKey::random())?;
        Ok(OnionRouteHop::new(
            session_sk.account_did(),
            session_sk.session_public_key(),
        ))
    }

    fn storage() -> OnionEntryGuardStorage {
        Box::new(MemStorage::new())
    }

    #[tokio::test]
    async fn test_entry_guards_remain_stable_across_repeated_selection() -> Result<()> {
        let relays = vec![relay()?, relay()?, relay()?];
        let manager = OnionEntryGuards::new_with_target_count(storage(), 2);
        let mut first_entropy = FixedEntropy::new([0, 0]);
        let first = manager
            .select_relay_guards(7, &relays, &[], &mut first_entropy, |_| true)
            .await?;
        let mut second_entropy = FixedEntropy::new([u64::MAX, u64::MAX]);
        let second = manager
            .select_relay_guards(7, &relays, &[], &mut second_entropy, |_| true)
            .await?;

        assert_eq!(first, second);
        assert_eq!(first.dids.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_entry_guards_fail_over_when_guard_degrades() -> Result<()> {
        let relays = vec![relay()?, relay()?, relay()?];
        let manager = OnionEntryGuards::new_with_target_count(storage(), 2);
        let mut first_entropy = FixedEntropy::new([0, 0]);
        let first = manager
            .select_relay_guards(7, &relays, &[], &mut first_entropy, |_| true)
            .await?;
        let degraded_did = first
            .dids
            .iter()
            .next()
            .copied()
            .ok_or(Error::InvalidData)?;
        assert!(first.contains(degraded_did));

        let mut second_entropy = FixedEntropy::new([0]);
        let second = manager
            .select_relay_guards(
                7,
                &relays,
                &[(degraded_did, PeerQuality::Degraded)],
                &mut second_entropy,
                |_| true,
            )
            .await?;

        assert!(!second.contains(degraded_did));
        assert_eq!(second.dids.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_entry_guards_persist_across_storage_reopen() -> Result<()> {
        let root = std::env::temp_dir().join(format!("rings-entry-guards-{}", Uuid::new_v4()));
        let relays = vec![relay()?, relay()?, relay()?];
        let first = {
            let storage = Box::new(FileStorage::new_with_cap_and_path(4096, &root).await?)
                as OnionEntryGuardStorage;
            let manager = OnionEntryGuards::new_with_target_count(storage, 2);
            let mut entropy = FixedEntropy::new([0, 0]);
            manager
                .select_relay_guards(7, &relays, &[], &mut entropy, |_| true)
                .await?
        };
        let second = {
            let storage = Box::new(FileStorage::new_with_cap_and_path(4096, &root).await?)
                as OnionEntryGuardStorage;
            let manager = OnionEntryGuards::new_with_target_count(storage, 2);
            let mut entropy = FixedEntropy::new([u64::MAX, u64::MAX]);
            manager
                .select_relay_guards(7, &relays, &[], &mut entropy, |_| true)
                .await?
        };
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(first, second);
        Ok(())
    }
}
