#![warn(missing_docs)]

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::dht::entry::Entry;
use crate::dht::Chord;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageCache;
use crate::dht::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::PayloadRelayFunctor;
use crate::message::types::FoundEntry;
use crate::message::types::Message;
use crate::message::types::SearchEntry;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::types::SyncEntriesWithSuccessorReport;
use crate::message::Encoded;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::prelude::entry::EntryOperation;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;

/// ChordStorageInterface should imply necessary method for DHT storage
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageInterface<const REDUNDANT: u16> {
    /// Fetch an entry from DHT storage.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()>;
    /// Store an entry on DHT storage.
    async fn storage_store(&self, entry: Entry) -> Result<()>;
    /// Append data to a Data kind entry.
    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()>;
    /// Append data to a Data kind entry uniquely.
    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()>;
}

/// ChordStorageInterfaceCacheChecker defines the interface for checking the local cache of the DHT.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageInterfaceCacheChecker {
    /// Check the local cache of the DHT for a specific entry key.
    ///
    /// Returns an optional `Entry` representing the cached data, or `None` if it is not found.
    async fn storage_check_cache(&self, entry_key: Did) -> Option<Entry>;
}

fn finish_storage_action(act: PeerRingAction) -> Result<()> {
    match act {
        PeerRingAction::None => Ok(()),
        act => Err(Error::PeerRingUnexpectedAction(act)),
    }
}

fn finish_storage_action_ref(act: &PeerRingAction) -> Result<()> {
    match act {
        PeerRingAction::None => Ok(()),
        act => Err(Error::PeerRingUnexpectedAction(act.clone())),
    }
}

async fn reset_storage_relay_destination(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    next: Did,
) -> Result<()> {
    handler
        .run_effects([PayloadRelayFunctor::reset_destination(ctx, next).into()])
        .await
}

/// Execute storage fetch actions for the Swarm-facing storage API.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_fetch_act<const REDUNDANT: u16>(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(v) => {
            transport.dht.local_cache_put(v.clone()).await?;
            // Read-repair is full affine republish today. It is additive and
            // idempotent, but can send redundant copies because entry_lookup
            // does not yet return the exact placement keys observed missing.
            let repair = transport.dht.read_repair_entry(v, REDUNDANT).await?;
            handle_storage_repair_act(transport.clone(), repair).await?;
        }
        PeerRingAction::RemoteAction(next, dht_act) => {
            if let PeerRingRemoteAction::FindEntry(entry_key) = dht_act {
                tracing::debug!(
                    "storage_fetch send_message: SearchEntry({:?}) to {:?}",
                    entry_key,
                    next
                );
                transport
                    .send_message(
                        Message::SearchEntry(SearchEntry {
                            key: entry_key,
                            redundancy: REDUNDANT,
                        }),
                        next,
                    )
                    .await?;
            }
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_fetch_act::<REDUNDANT>(transport.clone(), act).await?;
            }
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Execute storage store actions for the Swarm-facing storage API.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub(super) async fn handle_storage_store_act(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindEntryForOperate(op)) => {
            transport
                .send_message(Message::OperateEntry(op), target)
                .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_store_act(transport.clone(), act).await?;
            }
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Execute copy-only storage repair actions.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub(super) async fn handle_storage_repair_act(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::RemoteAction(
            destination,
            PeerRingRemoteAction::SyncEntriesWithSuccessor(data),
        ) => {
            transport
                .send_message(
                    Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor { data }),
                    destination,
                )
                .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_repair_act(transport.clone(), act).await?;
            }
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Execute storage search actions emitted by inbound message handlers.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_search_act(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    act: PeerRingAction,
    redundancy: u16,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(v) => {
            handler
                .run_effects([PayloadRelayFunctor::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry {
                        data: vec![v],
                        redundancy,
                    }),
                )
                .into()])
                .await
        }
        PeerRingAction::RemoteAction(next, _) => {
            reset_storage_relay_destination(handler, ctx, next).await
        }
        PeerRingAction::MultiActions(acts) => {
            let jobs = acts.iter().map(|act| async move {
                handle_storage_search_act(handler, ctx, act.clone(), redundancy).await
            });

            for res in futures::future::join_all(jobs).await {
                if res.is_err() {
                    tracing::error!("Failed on handle multi actions: {:#?}", res)
                }
            }

            Ok(())
        }
        act => finish_storage_action(act),
    }
}

/// Execute storage operation actions emitted by inbound message handlers.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_operate_act(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    act: &PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::RemoteAction(next, _) => {
            reset_storage_relay_destination(handler, ctx, *next).await
        }
        PeerRingAction::MultiActions(acts) => {
            let jobs = acts
                .iter()
                .map(|act| async move { handle_storage_operate_act(handler, ctx, act).await });

            for res in futures::future::join_all(jobs).await {
                if res.is_err() {
                    tracing::error!("Failed on handle multi actions: {:#?}", res)
                }
            }

            Ok(())
        }
        act => finish_storage_action_ref(act),
    }
}

async fn persist_synced_entries(
    handler: &MessageHandler,
    msg: &SyncEntriesWithSuccessor,
) -> Result<Vec<Did>> {
    let mut keys = Vec::with_capacity(msg.data.len());
    for placed in msg.data.iter() {
        let entry = placed.entry.clone().into_storage_entry();
        handler
            .dht
            .storage
            .put(&placed.key.to_string(), &entry)
            .await?;
        keys.push(placed.key);
    }
    Ok(keys)
}

fn next_hop_for_sync_entries(
    handler: &MessageHandler,
    ctx: &MessagePayload,
) -> Result<Option<Did>> {
    if ctx.is_relay_destination_for(handler.dht.did) {
        return Ok(None);
    }

    match handler.dht.find_successor(ctx.relay.destination)? {
        PeerRingAction::Some(owner) if owner == handler.dht.did => Ok(None),
        PeerRingAction::Some(next) => Ok(Some(next)),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(_)) => {
            Ok(Some(next))
        }
        action => Err(Error::PeerRingUnexpectedAction(action)),
    }
}

async fn report_synced_entries(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    keys: Vec<Did>,
) -> Result<()> {
    handler
        .run_effects([PayloadRelayFunctor::send_report_message(
            ctx,
            Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport { keys }),
        )
        .into()])
        .await
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageInterfaceCacheChecker for Swarm {
    /// Check local cache
    async fn storage_check_cache(&self, entry_key: Did) -> Option<Entry> {
        self.dht.local_cache_get(entry_key).await.ok().flatten()
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl<const REDUNDANT: u16> ChordStorageInterface<REDUNDANT> for Swarm {
    /// Fetch an entry. If it exists in local storage, copy it to the cache;
    /// otherwise query the responsible remote node.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        // If peer found that data is on it's localstore, copy it to the cache
        let act =
            <PeerRing as ChordStorage<_, REDUNDANT>>::entry_lookup(&self.dht, entry_key).await?;
        handle_storage_fetch_act::<REDUNDANT>(self.transport.clone(), act).await?;
        Ok(())
    }

    /// Store Entry, `TryInto<Entry>` is implemented for alot of types
    async fn storage_store(&self, entry: Entry) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let op = EntryOperation::Overwrite(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let entry: Entry = (topic.to_string(), data).try_into()?;
        let op = EntryOperation::Extend(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let entry: Entry = (topic.to_string(), data).try_into()?;
        let op = EntryOperation::Touch(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SearchEntry> for MessageHandler {
    /// Search Entry via successor
    /// If a Entry is storead local, it will response immediately.(See Chordstorageinterface::storage_fetch)
    async fn handle(&self, ctx: &MessagePayload, msg: &SearchEntry) -> Result<()> {
        // For relay message, set redundant to 1
        match <PeerRing as ChordStorage<_, 1>>::entry_lookup(&self.dht, msg.key).await {
            Ok(action) => handle_storage_search_act(self, ctx, action, msg.redundancy).await,
            Err(e) => Err(e),
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FoundEntry> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FoundEntry) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
                .await;
        }
        for data in msg.data.iter().cloned() {
            self.dht.local_cache_put(data.clone()).await?;
            // The requester may be far from the entry's placement key. Until
            // lookup reports per-replica misses, read-repair republishes the
            // full affine set and relies on additive/idempotent sync.
            let repair = self.dht.read_repair_entry(data, msg.redundancy).await?;
            handle_storage_repair_act(self.transport.clone(), repair).await?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<EntryOperation> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &EntryOperation) -> Result<()> {
        // For relay message, set redundant to 1
        let action =
            <PeerRing as ChordStorage<_, 1>>::entry_operate(&self.dht, msg.clone()).await?;
        handle_storage_operate_act(self, ctx, &action).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SyncEntriesWithSuccessor> for MessageHandler {
    // received remote sync entry request
    async fn handle(&self, ctx: &MessagePayload, msg: &SyncEntriesWithSuccessor) -> Result<()> {
        if let Some(next) = next_hop_for_sync_entries(self, ctx)? {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, Some(next)).into()])
                .await;
        }

        let keys = persist_synced_entries(self, msg).await?;
        if let Err(e) = report_synced_entries(self, ctx, keys).await {
            tracing::warn!("Failed to report synced entries: {e:?}");
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SyncEntriesWithSuccessorReport> for MessageHandler {
    async fn handle(
        &self,
        _ctx: &MessagePayload,
        msg: &SyncEntriesWithSuccessorReport,
    ) -> Result<()> {
        let action = self.dht.acknowledge_synced_entries(&msg.keys).await?;
        finish_storage_action(action)
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::consts::ENTRY_DATA_MAX_LEN;
    use crate::dht::entry::PlacedEntry;
    use crate::dht::successor::SuccessorReader;
    use crate::dht::successor::SuccessorWriter;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::message::Encoder;
    use crate::prelude::entry::EntryKind;
    use crate::session::SessionSk;
    use crate::storage::MemStorage;
    use crate::swarm::callback::SwarmCallback;
    use crate::swarm::SwarmBuilder;
    use crate::tests::default::assert_no_more_msg;
    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::default::Node;
    use crate::tests::manually_establish_connection;

    struct NoopCallback;

    impl SwarmCallback for NoopCallback {}

    async fn next_payload(node: &Node) -> Result<MessagePayload> {
        node.listen_once()
            .await
            .ok_or_else(|| Error::InvalidMessage("expected message payload".to_string()))
    }

    fn next_generated_key(keys: &mut impl Iterator<Item = SecretKey>) -> Result<SecretKey> {
        keys.next()
            .ok_or_else(|| Error::InvalidMessage("expected generated key".to_string()))
    }

    #[test]
    fn finish_storage_action_accepts_empty_action() -> Result<()> {
        finish_storage_action(PeerRingAction::None)?;
        finish_storage_action_ref(&PeerRingAction::None)?;
        Ok(())
    }

    #[test]
    fn finish_storage_action_rejects_unhandled_action() -> Result<()> {
        let did = SecretKey::random().address().into();
        match finish_storage_action(PeerRingAction::Some(did)) {
            Err(Error::PeerRingUnexpectedAction(PeerRingAction::Some(actual))) => {
                assert_eq!(actual, did);
                Ok(())
            }
            res => Err(Error::InvalidMessage(format!(
                "expected unexpected storage action, got {res:?}"
            ))),
        }
    }

    #[tokio::test]
    async fn sync_entries_handler_stores_entry_at_placement_key() -> Result<()> {
        let node = prepare_node(SecretKey::random()).await;
        let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
        let resource_id = Did::from(10u32);
        let placement_key = Did::from(100u32);
        let entry = Entry {
            did: resource_id,
            data: vec!["placed".to_string().encode()?],
            kind: EntryKind::Data,
        };
        let context_key = SecretKey::random();
        let context_session = SessionSk::new_with_seckey(&context_key)?;
        let context = MessagePayload::new_send(
            Message::custom(b"sync context")?,
            &context_session,
            node.did(),
            node.did(),
        )?;

        handler
            .handle(&context, &SyncEntriesWithSuccessor {
                data: vec![PlacedEntry::new(placement_key, entry.clone())],
            })
            .await?;

        assert_eq!(
            node.dht().storage.get(&placement_key.to_string()).await?,
            Some(entry)
        );
        assert_eq!(
            node.dht().storage.get(&resource_id.to_string()).await?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn sync_entries_handler_caps_inbound_entry_payloads() -> Result<()> {
        let node = prepare_node(SecretKey::random()).await;
        let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
        let placement_key = Did::from(100u32);
        let entry = Entry {
            did: Did::from(10u32),
            data: (0..ENTRY_DATA_MAX_LEN + 3)
                .map(|i| format!("payload{i}").encode())
                .collect::<Result<Vec<_>>>()?,
            kind: EntryKind::Data,
        };
        let context_key = SecretKey::random();
        let context_session = SessionSk::new_with_seckey(&context_key)?;
        let context = MessagePayload::new_send(
            Message::custom(b"sync context")?,
            &context_session,
            node.did(),
            node.did(),
        )?;

        handler
            .handle(&context, &SyncEntriesWithSuccessor {
                data: vec![PlacedEntry::new(placement_key, entry)],
            })
            .await?;

        let stored = node
            .dht()
            .storage
            .get(&placement_key.to_string())
            .await?
            .ok_or_else(|| Error::InvalidMessage("expected stored sync entry".to_string()))?;

        assert_eq!(stored.data.len(), ENTRY_DATA_MAX_LEN);
        let first_payload: String = stored
            .data
            .first()
            .ok_or_else(|| Error::InvalidMessage("expected capped payload".to_string()))?
            .decode()?;
        assert_eq!(first_payload, String::from("payload3"));
        Ok(())
    }

    #[tokio::test]
    async fn sync_entries_handler_routes_repair_by_placement_destination() -> Result<()> {
        let mut keys = gen_ordered_keys(2).into_iter();
        let node1 = prepare_node(next_generated_key(&mut keys)?).await;
        let node2 = prepare_node(next_generated_key(&mut keys)?).await;
        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
        let placement_key = node2.did();
        let entry = Entry {
            did: Did::from(10u32),
            data: vec!["routed repair".to_string().encode()?],
            kind: EntryKind::Data,
        };
        let msg = SyncEntriesWithSuccessor {
            data: vec![PlacedEntry::new(placement_key, entry.clone())],
        };
        let context_key = SecretKey::random();
        let context_session = SessionSk::new_with_seckey(&context_key)?;
        let context = MessagePayload::new_send(
            Message::SyncEntriesWithSuccessor(msg.clone()),
            &context_session,
            node1.did(),
            placement_key,
        )?;

        handler.handle(&context, &msg).await?;

        let forwarded = next_payload(&node2).await?;
        assert!(matches!(
            forwarded.transaction.data()?,
            Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor { data })
                if data == vec![PlacedEntry::new(placement_key, entry.clone())]
        ));
        let ack = next_payload(&node1).await?;
        assert!(matches!(
            ack.transaction.data()?,
            Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport { keys })
                if keys == vec![placement_key]
        ));
        assert_eq!(
            node1.dht().storage.get(&placement_key.to_string()).await?,
            None
        );
        assert_eq!(
            node2.dht().storage.get(&placement_key.to_string()).await?,
            Some(entry)
        );
        Ok(())
    }

    #[tokio::test]
    async fn leave_dht_republishes_after_responsibility_peer_departure() -> Result<()> {
        let key = SecretKey::random();
        let session = SessionSk::new_with_seckey(&key)?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                "stun://stun.l.google.com:19302",
                Box::new(MemStorage::new()),
                session,
            )
            .dht_storage_redundancy(2)
            .build(),
        );
        let node = Node::new(swarm);
        let departed = Did::from(100u32);
        node.dht().successors().update(departed)?;
        let entry = Entry {
            did: key.address().into(),
            data: vec![],
            kind: EntryKind::Data,
        };
        let placement_keys = entry.did.rotate_affine(2)?;
        node.dht()
            .storage
            .put(&placement_keys[0].to_string(), &entry)
            .await?;
        let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));

        handler.leave_dht(departed).await?;

        assert!(!node.dht().successors().contains(&departed)?);
        assert_eq!(
            node.dht()
                .storage
                .get(&placement_keys[1].to_string())
                .await?,
            Some(entry)
        );
        Ok(())
    }

    #[tokio::test]
    async fn storage_api_rejects_redundancy_mismatch() -> Result<()> {
        let node = prepare_node(SecretKey::random()).await;

        let result =
            <Swarm as ChordStorageInterface<2>>::storage_fetch(&node.swarm, node.did()).await;

        assert!(matches!(
            result,
            Err(Error::StorageRedundancyMismatch {
                configured: 1,
                requested: 2
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn sync_entries_handler_reports_persisted_placement_keys() -> Result<()> {
        let sender = prepare_node(SecretKey::random()).await;
        let receiver = prepare_node(SecretKey::random()).await;
        manually_establish_connection(&sender.swarm, &receiver.swarm).await;
        wait_for_msgs([&sender, &receiver]).await;

        let handler = MessageHandler::new(receiver.swarm.transport.clone(), Arc::new(NoopCallback));
        let placement_key = Did::from(100u32);
        let entry = Entry {
            did: Did::from(10u32),
            data: vec!["acked".to_string().encode()?],
            kind: EntryKind::Data,
        };
        let sync_msg = SyncEntriesWithSuccessor {
            data: vec![PlacedEntry::new(placement_key, entry.clone())],
        };
        let context = MessagePayload::new_send(
            Message::SyncEntriesWithSuccessor(sync_msg.clone()),
            sender.swarm.transport.session_sk(),
            receiver.did(),
            receiver.did(),
        )?;

        handler.handle(&context, &sync_msg).await?;

        let payload = next_payload(&sender).await?;
        match payload.transaction.data::<Message>()? {
            Message::SyncEntriesWithSuccessorReport(report) => {
                assert_eq!(report.keys, vec![placement_key]);
            }
            message => {
                return Err(Error::InvalidMessage(format!(
                    "expected SyncEntriesWithSuccessorReport, got {message:?}"
                )))
            }
        }
        assert_eq!(
            receiver
                .dht()
                .storage
                .get(&placement_key.to_string())
                .await?,
            Some(entry)
        );
        Ok(())
    }

    #[tokio::test]
    async fn sync_entries_report_handler_deletes_only_acked_keys() -> Result<()> {
        let node = prepare_node(SecretKey::random()).await;
        let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
        let acked_key = Did::from(100u32);
        let pending_key = Did::from(120u32);
        let acked_entry = Entry {
            did: Did::from(10u32),
            data: vec![],
            kind: EntryKind::Data,
        };
        let pending_entry = Entry {
            did: Did::from(20u32),
            data: vec![],
            kind: EntryKind::Data,
        };
        let context = MessagePayload::new_send(
            Message::custom(b"sync ack context")?,
            node.swarm.transport.session_sk(),
            node.did(),
            node.did(),
        )?;
        node.dht()
            .storage
            .put(&acked_key.to_string(), &acked_entry)
            .await?;
        node.dht()
            .storage
            .put(&pending_key.to_string(), &pending_entry)
            .await?;

        handler
            .handle(&context, &SyncEntriesWithSuccessorReport {
                keys: vec![acked_key],
            })
            .await?;

        assert_eq!(node.dht().storage.get(&acked_key.to_string()).await?, None);
        assert_eq!(
            node.dht().storage.get(&pending_key.to_string()).await?,
            Some(pending_entry)
        );
        Ok(())
    }

    #[tokio::test]
    async fn storage_store_fetches_remote_entry_into_cache() -> Result<()> {
        let mut keys = gen_ordered_keys(2).into_iter();
        let key1 = next_generated_key(&mut keys)?;
        let key2 = next_generated_key(&mut keys)?;
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        // Now, node1 is the successor of node2, and node2 is the successor of node1.
        // Following tests storing data on node2 and query it from node1.
        let data = "Across the Great Wall we can reach every corner in the world.".to_string();
        let entry: Entry = data.clone().try_into()?;
        let entry_key = entry.did;

        // Make sure the data is stored on node2.
        let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
            (node1, node2)
        } else {
            (node2, node1)
        };

        assert_eq!(node1.dht().cache.count().await?, 0);
        assert_eq!(node2.dht().cache.count().await?, 0);
        assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());

        <Swarm as ChordStorageInterface<1>>::storage_store(&node1.swarm, entry.clone()).await?;
        let ev = next_payload(&node2).await?;
        assert!(matches!(
            ev.transaction.data()?,
            Message::OperateEntry(EntryOperation::Overwrite(x)) if x.did == entry_key
        ));

        assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node1.dht().storage.count().await? == 0);
        assert!(node2.dht().storage.count().await? != 0);

        // test remote query
        println!("entry_key is on node2 {:?}", node2.did());
        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;

        // it will send request to node2
        let ev = next_payload(&node2).await?;
        // node2 received search entry request
        assert!(matches!(
            ev.transaction.data()?,
            Message::SearchEntry(x) if x.key == entry_key
        ));

        let ev = next_payload(&node1).await?;
        assert!(matches!(
            ev.transaction.data()?,
            Message::FoundEntry(x) if x.data.first().is_some_and(|entry| entry.did == entry_key)
        ));

        assert_eq!(
            node1.swarm.storage_check_cache(entry_key).await,
            Some(Entry {
                did: entry_key,
                data: vec![data.encode()?],
                kind: EntryKind::Data
            })
        );

        Ok(())
    }

    #[tokio::test]
    async fn storage_append_data_preserves_entry_payload_order() -> Result<()> {
        let mut keys = gen_ordered_keys(2).into_iter();
        let key1 = next_generated_key(&mut keys)?;
        let key2 = next_generated_key(&mut keys)?;
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        // Now, node1 is the successor of node2, and node2 is the successor of node1.
        // Following tests storing data on node2 and query it from node1.
        let topic = "Across the Great Wall we can reach every corner in the world.".to_string();
        let entry: Entry = topic.clone().try_into()?;
        let entry_key = entry.did;

        // Make sure the data is stored on node2.
        let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
            (node1, node2)
        } else {
            (node2, node1)
        };

        assert_eq!(node1.dht().cache.count().await?, 0);
        assert_eq!(node2.dht().cache.count().await?, 0);
        assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());

        <Swarm as ChordStorageInterface<1>>::storage_append_data(
            &node1.swarm,
            &topic,
            "111".to_string().encode()?,
        )
        .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        <Swarm as ChordStorageInterface<1>>::storage_append_data(
            &node1.swarm,
            &topic,
            "222".to_string().encode()?,
        )
        .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node1.dht().storage.count().await? == 0);
        assert!(node2.dht().storage.count().await? != 0);

        // test remote query
        println!("entry_key is on node2 {:?}", node2.did());
        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        assert_eq!(
            node1.swarm.storage_check_cache(entry_key).await,
            Some(Entry {
                did: entry_key,
                data: vec!["111".to_string().encode()?, "222".to_string().encode()?],
                kind: EntryKind::Data
            })
        );

        // Append more data
        <Swarm as ChordStorageInterface<1>>::storage_append_data(
            &node1.swarm,
            &topic,
            "333".to_string().encode()?,
        )
        .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        // test remote query agagin
        println!("entry_key is on node2 {:?}", node2.did());
        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        assert_eq!(
            node1.swarm.storage_check_cache(entry_key).await,
            Some(Entry {
                did: entry_key,
                data: vec![
                    "111".to_string().encode()?,
                    "222".to_string().encode()?,
                    "333".to_string().encode()?
                ],
                kind: EntryKind::Data
            })
        );

        Ok(())
    }

    #[tokio::test]
    async fn storage_touch_data_moves_existing_entry_payload_to_end_once() -> Result<()> {
        let mut keys = gen_ordered_keys(2).into_iter();
        let key1 = next_generated_key(&mut keys)?;
        let key2 = next_generated_key(&mut keys)?;
        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;

        manually_establish_connection(&node1.swarm, &node2.swarm).await;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        let topic = "touch keeps unique entry payloads ordered by recency".to_string();
        let entry: Entry = topic.clone().try_into()?;
        let entry_key = entry.did;

        let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
            (node1, node2)
        } else {
            (node2, node1)
        };

        for value in ["111", "222", "333", "222"] {
            <Swarm as ChordStorageInterface<1>>::storage_touch_data(
                &node1.swarm,
                &topic,
                value.to_string().encode()?,
            )
            .await?;
            wait_for_msgs([&node1, &node2]).await;
            assert_no_more_msg([&node1, &node2]).await;
        }

        assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
        assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
        assert_eq!(node1.dht().storage.count().await?, 0);
        assert_ne!(node2.dht().storage.count().await?, 0);

        <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;

        assert_eq!(
            node1.swarm.storage_check_cache(entry_key).await,
            Some(Entry {
                did: entry_key,
                data: vec![
                    "111".to_string().encode()?,
                    "333".to_string().encode()?,
                    "222".to_string().encode()?
                ],
                kind: EntryKind::Data
            })
        );

        Ok(())
    }
}
