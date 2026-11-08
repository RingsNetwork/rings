//! Local onion entry-guard selection state.
//!
//! Entry guards are client-local privacy state. They deliberately live outside
//! Chord and are persisted only in the node's local storage backend.
//!
//! Guard policy (#911): the store is opened as a `RecordAuthority::DurableCache`, so a guard
//! write that completed survives a crash, while a guard record that is absent or does not decode
//! means "no guards yet": the storage retires it (logged at `warn`) and the selection below
//! re-selects from scratch. Losing the set costs one re-selection of the first hop; it never
//! fails a selection, which would take onion routing down for privacy state.

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

const ENTRY_GUARD_KEY_PREFIX: &str = "rings-node:onion-entry-guards";
const DEFAULT_ENTRY_GUARD_COUNT: usize = 3;
/// Maximum lifetime a persisted entry guard is retained before rotation (30 days).
const ENTRY_GUARD_ROTATION_LIFETIME_MS: u128 = 30 * 24 * 60 * 60 * 1_000;
/// Grace window a persisted entry guard may remain absent from live snapshots before eviction
/// (24 hours).
const ENTRY_GUARD_UNREACHABLE_GRACE_MS: u128 = 24 * 60 * 60 * 1_000;

/// Serialized local entry-guard snapshot.
///
/// This type is public so native and browser storage adapters can name the
/// concrete key-value value type, but its fields remain private because callers
/// should not construct or interpret guard state directly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OnionEntryGuardState {
    network_id: u32,
    guards: Vec<OnionEntryGuardRecord>,
}

/// Local key-value storage for persisted onion entry guards.
pub type OnionEntryGuardStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<OnionEntryGuardState>)>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct OnionEntryGuardRecord {
    did: Did,
    selected_at_ms: u128,
    #[serde(default)]
    unreachable_since_ms: Option<u128>,
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
        self.select_relay_guards_at(
            network_id,
            relays,
            qualities,
            get_epoch_ms(),
            entropy,
            first_hop_permitted,
        )
        .await
    }

    pub(crate) async fn select_relay_guards_at(
        &self,
        network_id: u32,
        relays: &[OnionRouteHop],
        qualities: &[(Did, PeerQuality)],
        now_ms: u128,
        entropy: &mut impl RouteEntropy,
        first_hop_permitted: impl Fn(Did) -> bool,
    ) -> Result<OnionEntryGuardSet> {
        let key = entry_guard_state_key(network_id);
        let stored = self
            .storage
            .get(&key)
            .await
            .map_err(Error::Storage)?
            .filter(|state| state.network_id == network_id);
        let quality_by_did = qualities.iter().copied().collect::<BTreeMap<_, _>>();
        let candidates = GuardCandidates {
            healthy_snapshot_dids: healthy_snapshot_guard_dids(relays, &quality_by_did),
            eligible_dids: eligible_guard_dids(relays, &quality_by_did, &first_hop_permitted),
        };
        let next = reconcile_guard_state(
            network_id,
            stored,
            &candidates,
            &quality_by_did,
            self.target_guard_count,
            now_ms,
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

struct GuardCandidates {
    healthy_snapshot_dids: BTreeSet<Did>,
    eligible_dids: BTreeSet<Did>,
}

struct ReconciledGuards {
    previous: Option<OnionEntryGuardState>,
    state: OnionEntryGuardState,
    set: OnionEntryGuardSet,
}

fn reconcile_guard_state(
    network_id: u32,
    previous: Option<OnionEntryGuardState>,
    candidates: &GuardCandidates,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    target_guard_count: usize,
    now_ms: u128,
    entropy: &mut impl RouteEntropy,
) -> ReconciledGuards {
    let GuardCandidates {
        healthy_snapshot_dids,
        eligible_dids,
    } = candidates;
    let mut retained = Vec::new();
    let mut guard_dids = BTreeSet::new();

    for guard in previous
        .as_ref()
        .map(|state| state.guards.as_slice())
        .unwrap_or_default()
    {
        if now_ms.saturating_sub(guard.selected_at_ms) >= ENTRY_GUARD_ROTATION_LIFETIME_MS {
            continue;
        }
        if !guard_dids.insert(guard.did) {
            continue;
        }
        if healthy_snapshot_dids.contains(&guard.did) {
            retained.push(OnionEntryGuardRecord {
                did: guard.did,
                selected_at_ms: guard.selected_at_ms,
                unreachable_since_ms: None,
            });
        } else {
            let unreachable_since_ms = guard.unreachable_since_ms.unwrap_or(now_ms);
            if now_ms.saturating_sub(unreachable_since_ms) < ENTRY_GUARD_UNREACHABLE_GRACE_MS {
                retained.push(OnionEntryGuardRecord {
                    did: guard.did,
                    selected_at_ms: guard.selected_at_ms,
                    unreachable_since_ms: Some(unreachable_since_ms),
                });
            } else {
                guard_dids.remove(&guard.did);
            }
        }
    }

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
            unreachable_since_ms: None,
        });
        guard_dids.insert(did);
    }

    let mut active_dids = retained
        .iter()
        .filter(|guard| {
            healthy_snapshot_dids.contains(&guard.did) && eligible_dids.contains(&guard.did)
        })
        .map(|guard| guard.did)
        .collect::<BTreeSet<_>>();
    while active_dids.len() < target_guard_count {
        let remaining = eligible_dids
            .iter()
            .copied()
            .filter(|did| !active_dids.contains(did) && !guard_dids.contains(did))
            .collect::<Vec<_>>();
        let Some(index) = pick_weighted_index(&remaining, quality_by_did, entropy) else {
            break;
        };
        let Some(did) = remaining.get(index).copied() else {
            break;
        };
        active_dids.insert(did);
    }

    let state = OnionEntryGuardState {
        network_id,
        guards: retained,
    };
    ReconciledGuards {
        previous,
        state,
        set: OnionEntryGuardSet { dids: active_dids },
    }
}

fn healthy_snapshot_guard_dids(
    relays: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
) -> BTreeSet<Did> {
    let live = relays.iter().map(|hop| hop.did).collect::<BTreeSet<_>>();
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
    use rings_core::storage::KvStorageScan;
    use rings_core::storage::MemStorage;
    use rings_core::storage::RecordAuthority;
    use uuid::Uuid;

    use super::*;
    use crate::onion::OnionProcessEpoch;
    use crate::prelude::DelegateeKey;

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
        let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        Ok(OnionRouteHop::new(
            delegatee_key.delegator_did(),
            delegatee_key.delegatee_public_key(),
            OnionProcessEpoch::new([7; 16]),
        ))
    }

    fn storage() -> OnionEntryGuardStorage {
        Box::new(MemStorage::new())
    }

    /// The entry-guard store at `root`, opened as the daemon opens it: a durable cache (#911).
    async fn durable_cache(root: &std::path::Path) -> Result<FileStorage> {
        Ok(
            FileStorage::new_with_cap_path_and_authority(4096, root, RecordAuthority::DurableCache)
                .await?,
        )
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
    async fn test_first_hop_permitted_exclusion_does_not_mark_guard_unreachable() -> Result<()> {
        let r0 = relay()?;
        let r1 = relay()?;
        let r2 = relay()?;
        let all_relays = vec![r0, r1, r2];
        let manager = OnionEntryGuards::new_with_target_count(storage(), 2);
        let t0 = 10_000;

        let mut first_entropy = FixedEntropy::new([0, 0]);
        let first = manager
            .select_relay_guards_at(7, &all_relays, &[], t0, &mut first_entropy, |_| true)
            .await?;
        assert_eq!(first.dids.len(), 2);

        let excluded_did = first
            .dids
            .iter()
            .next()
            .copied()
            .ok_or(Error::InvalidData)?;
        let mut second_entropy = FixedEntropy::new([0]);
        let second = manager
            .select_relay_guards_at(
                7,
                &all_relays,
                &[],
                t0 + 1_000,
                &mut second_entropy,
                |did| did != excluded_did,
            )
            .await?;

        assert!(!second.contains(excluded_did));
        assert_eq!(second.dids.len(), 2);

        let persisted = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        assert_eq!(
            persisted
                .guards
                .iter()
                .map(|guard| guard.did)
                .collect::<BTreeSet<_>>(),
            first.dids
        );
        assert!(persisted
            .guards
            .iter()
            .all(|guard| guard.selected_at_ms == t0 && guard.unreachable_since_ms.is_none()));
        Ok(())
    }

    #[tokio::test]
    async fn test_entry_guard_survives_single_snapshot_miss_and_reuses_on_reappearance(
    ) -> Result<()> {
        let r0 = relay()?;
        let r1 = relay()?;
        let r2 = relay()?;
        let all_relays = vec![r0, r1, r2];
        let manager = OnionEntryGuards::new_with_target_count(storage(), 2);
        let t0 = 10_000;

        let mut first_entropy = FixedEntropy::new([0, 0]);
        let first = manager
            .select_relay_guards_at(7, &all_relays, &[], t0, &mut first_entropy, |_| true)
            .await?;
        assert_eq!(first.dids.len(), 2);

        let missing_did = first
            .dids
            .iter()
            .next()
            .copied()
            .ok_or(Error::InvalidData)?;
        let surviving_did = first
            .dids
            .iter()
            .copied()
            .find(|did| *did != missing_did)
            .ok_or(Error::InvalidData)?;
        let snapshot_without_missing = all_relays
            .iter()
            .copied()
            .filter(|hop| hop.did != missing_did)
            .collect::<Vec<_>>();

        let t_miss = t0 + 90_000;
        let mut miss_entropy = FixedEntropy::new([0, 0]);
        let during_miss = manager
            .select_relay_guards_at(
                7,
                &snapshot_without_missing,
                &[],
                t_miss,
                &mut miss_entropy,
                |_| true,
            )
            .await?;
        assert!(!during_miss.contains(missing_did));
        assert!(during_miss.contains(surviving_did));
        assert_eq!(during_miss.dids.len(), 2);

        let persisted_during_miss = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        assert_eq!(
            persisted_during_miss
                .guards
                .iter()
                .map(|guard| guard.did)
                .collect::<BTreeSet<_>>(),
            first.dids
        );
        let missing_record = persisted_during_miss
            .guards
            .iter()
            .find(|guard| guard.did == missing_did)
            .ok_or(Error::InvalidData)?;
        assert_eq!(missing_record.selected_at_ms, t0);
        assert_eq!(missing_record.unreachable_since_ms, Some(t_miss));

        let t_reappear = t_miss + 90_000;
        let mut reappear_entropy = FixedEntropy::new([u64::MAX, u64::MAX]);
        let after_reappear = manager
            .select_relay_guards_at(
                7,
                &all_relays,
                &[],
                t_reappear,
                &mut reappear_entropy,
                |_| true,
            )
            .await?;
        assert_eq!(after_reappear, first);

        let persisted_after_reappear = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        assert!(persisted_after_reappear
            .guards
            .iter()
            .all(|guard| guard.selected_at_ms == t0 && guard.unreachable_since_ms.is_none()));
        Ok(())
    }

    #[tokio::test]
    async fn test_entry_guard_all_missing_uses_fallback_without_evicting_persisted_roster(
    ) -> Result<()> {
        let r0 = relay()?;
        let r1 = relay()?;
        let r2 = relay()?;
        let r3 = relay()?;
        let initial_relays = vec![r0, r1];
        let fallback_relays = vec![r2, r3];
        let all_relays = vec![r0, r1, r2, r3];
        let manager = OnionEntryGuards::new_with_target_count(storage(), 2);
        let t0 = 10_000;

        let mut first_entropy = FixedEntropy::new([0, 0]);
        let first = manager
            .select_relay_guards_at(7, &initial_relays, &[], t0, &mut first_entropy, |_| true)
            .await?;
        assert_eq!(first.dids, BTreeSet::from([r0.did, r1.did]));

        let t_miss = t0 + 30_000;
        let mut fallback_entropy = FixedEntropy::new([0, 0]);
        let fallback = manager
            .select_relay_guards_at(
                7,
                &fallback_relays,
                &[],
                t_miss,
                &mut fallback_entropy,
                |_| true,
            )
            .await?;
        assert_eq!(fallback.dids, BTreeSet::from([r2.did, r3.did]));

        let persisted_during_miss = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        assert_eq!(
            persisted_during_miss
                .guards
                .iter()
                .map(|guard| guard.did)
                .collect::<BTreeSet<_>>(),
            first.dids
        );

        let t_recover = t_miss + 30_000;
        let mut recover_entropy = FixedEntropy::new([u64::MAX, u64::MAX]);
        let recovered = manager
            .select_relay_guards_at(7, &all_relays, &[], t_recover, &mut recover_entropy, |_| {
                true
            })
            .await?;
        assert_eq!(recovered, first);
        Ok(())
    }

    #[tokio::test]
    async fn test_entry_guard_eviction_and_rotation_follow_deterministic_time_bounds() -> Result<()>
    {
        let r0 = relay()?;
        let r1 = relay()?;
        let r2 = relay()?;
        let initial_relays = vec![r0, r1];
        let replacement_relays = vec![r1, r2];
        let manager = OnionEntryGuards::new_with_target_count(storage(), 2);
        let t0 = 100_000;

        let mut entropy = FixedEntropy::new([0, 0]);
        let first = manager
            .select_relay_guards_at(7, &initial_relays, &[], t0, &mut entropy, |_| true)
            .await?;
        assert_eq!(first.dids, BTreeSet::from([r0.did, r1.did]));

        let t_miss = t0 + 1_000;
        let _ = manager
            .select_relay_guards_at(7, &replacement_relays, &[], t_miss, &mut entropy, |_| true)
            .await?;

        let before_grace_end = t_miss + ENTRY_GUARD_UNREACHABLE_GRACE_MS - 1;
        let before_eviction = manager
            .select_relay_guards_at(
                7,
                &replacement_relays,
                &[],
                before_grace_end,
                &mut entropy,
                |_| true,
            )
            .await?;
        assert_eq!(before_eviction.dids, BTreeSet::from([r1.did, r2.did]));
        let persisted_before_eviction = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        assert_eq!(
            persisted_before_eviction
                .guards
                .iter()
                .map(|guard| guard.did)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([r0.did, r1.did])
        );

        let at_grace_end = t_miss + ENTRY_GUARD_UNREACHABLE_GRACE_MS;
        let after_eviction = manager
            .select_relay_guards_at(
                7,
                &replacement_relays,
                &[],
                at_grace_end,
                &mut entropy,
                |_| true,
            )
            .await?;
        assert_eq!(after_eviction.dids, BTreeSet::from([r1.did, r2.did]));
        let persisted_after_eviction = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        assert_eq!(
            persisted_after_eviction
                .guards
                .iter()
                .map(|guard| guard.did)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([r1.did, r2.did])
        );

        let before_r1_rotation = t0 + ENTRY_GUARD_ROTATION_LIFETIME_MS - 1;
        let _ = manager
            .select_relay_guards_at(
                7,
                &replacement_relays,
                &[],
                before_r1_rotation,
                &mut entropy,
                |_| true,
            )
            .await?;
        let persisted_before_rotation = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        let r1_before = persisted_before_rotation
            .guards
            .iter()
            .find(|guard| guard.did == r1.did)
            .ok_or(Error::InvalidData)?;
        assert_eq!(r1_before.selected_at_ms, t0);

        let at_r1_rotation = t0 + ENTRY_GUARD_ROTATION_LIFETIME_MS;
        let _ = manager
            .select_relay_guards_at(
                7,
                &replacement_relays,
                &[],
                at_r1_rotation,
                &mut entropy,
                |_| true,
            )
            .await?;
        let persisted_after_rotation = manager
            .storage
            .get(&entry_guard_state_key(7))
            .await
            .map_err(Error::Storage)?
            .ok_or(Error::InvalidData)?;
        let r1_after = persisted_after_rotation
            .guards
            .iter()
            .find(|guard| guard.did == r1.did)
            .ok_or(Error::InvalidData)?;
        assert_eq!(r1_after.selected_at_ms, at_r1_rotation);
        Ok(())
    }

    #[tokio::test]
    async fn test_entry_guards_persist_across_storage_reopen() -> Result<()> {
        let root = std::env::temp_dir().join(format!("rings-entry-guards-{}", Uuid::new_v4()));
        let relays = vec![relay()?, relay()?, relay()?];
        let first = {
            let storage = Box::new(durable_cache(&root).await?) as OnionEntryGuardStorage;
            let manager = OnionEntryGuards::new_with_target_count(storage, 2);
            let mut entropy = FixedEntropy::new([0, 0]);
            manager
                .select_relay_guards(7, &relays, &[], &mut entropy, |_| true)
                .await?
        };
        let second = {
            let storage = Box::new(durable_cache(&root).await?) as OnionEntryGuardStorage;
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

    /// Guard policy (#911): a torn guard record, which a crash cannot leave in a durable cache
    /// but a loss outside the store can, is retired and the set re-selected from scratch, never
    /// an error that takes onion routing down; the re-selected set is persisted whole.
    #[tokio::test]
    async fn test_a_torn_guard_record_is_reselected_end_to_end() -> Result<()> {
        let root = std::env::temp_dir().join(format!("rings-entry-guards-{}", Uuid::new_v4()));
        let relays = vec![relay()?, relay()?, relay()?];
        let record = {
            let storage = durable_cache(&root).await?;
            let record = <FileStorage as KvStorageScan<OnionEntryGuardState>>::record_name(
                &storage,
                &entry_guard_state_key(7),
            );
            let manager = OnionEntryGuards::new_with_target_count(Box::new(storage), 2);
            let mut entropy = FixedEntropy::new([0, 0]);
            manager
                .select_relay_guards(7, &relays, &[], &mut entropy, |_| true)
                .await?;
            root.join(record)
        };
        let io = |error| Error::Storage(rings_core::error::Error::ServiceIOError(error));
        let whole = std::fs::read(&record).map_err(io)?;
        std::fs::write(&record, whole.get(..whole.len() / 2).unwrap_or_default()).map_err(io)?;

        let reselected = {
            let manager =
                OnionEntryGuards::new_with_target_count(Box::new(durable_cache(&root).await?), 2);
            let mut entropy = FixedEntropy::new([u64::MAX, u64::MAX]);
            manager
                .select_relay_guards(7, &relays, &[], &mut entropy, |_| true)
                .await?
        };
        let reopened = {
            let manager =
                OnionEntryGuards::new_with_target_count(Box::new(durable_cache(&root).await?), 2);
            let mut entropy = FixedEntropy::new([0, 0]);
            manager
                .select_relay_guards(7, &relays, &[], &mut entropy, |_| true)
                .await?
        };
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(reselected.dids.len(), 2);
        assert_eq!(reopened, reselected);
        Ok(())
    }
}
