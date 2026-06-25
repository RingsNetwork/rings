#![warn(missing_docs)]

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::dht::entry::Entry;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageCache;
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
async fn handle_storage_fetch_act(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(v) => {
            transport.dht.local_cache_put(v).await?;
        }
        PeerRingAction::RemoteAction(next, dht_act) => {
            if let PeerRingRemoteAction::FindEntry(entry_key) = dht_act {
                tracing::debug!(
                    "storage_fetch send_message: SearchEntry({:?}) to {:?}",
                    entry_key,
                    next
                );
                transport
                    .send_message(Message::SearchEntry(SearchEntry { key: entry_key }), next)
                    .await?;
            }
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_fetch_act(transport.clone(), act).await?;
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

/// Execute storage search actions emitted by inbound message handlers.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_search_act(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(v) => {
            handler
                .run_effects([PayloadRelayFunctor::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry { data: vec![v] }),
                )
                .into()])
                .await
        }
        PeerRingAction::RemoteAction(next, _) => {
            reset_storage_relay_destination(handler, ctx, next).await
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
        // If peer found that data is on it's localstore, copy it to the cache
        let act =
            <PeerRing as ChordStorage<_, REDUNDANT>>::entry_lookup(&self.dht, entry_key).await?;
        handle_storage_fetch_act(self.transport.clone(), act).await?;
        Ok(())
    }

    /// Store Entry, `TryInto<Entry>` is implemented for alot of types
    async fn storage_store(&self, entry: Entry) -> Result<()> {
        let op = EntryOperation::Overwrite(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        let op = EntryOperation::Extend(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
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
            Ok(action) => handle_storage_search_act(self, ctx, action).await,
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
            self.dht.local_cache_put(data).await?;
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
    async fn handle(&self, _ctx: &MessagePayload, msg: &SyncEntriesWithSuccessor) -> Result<()> {
        for placed in msg.data.iter() {
            self.dht
                .storage
                .put(&placed.key.to_string(), &placed.entry)
                .await?;
        }
        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::dht::entry::PlacedEntry;
    use crate::ecc::tests::gen_ordered_keys;
    use crate::ecc::SecretKey;
    use crate::message::Encoder;
    use crate::prelude::entry::EntryKind;
    use crate::session::SessionSk;
    use crate::swarm::callback::SwarmCallback;
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

    #[cfg(not(feature = "redundant"))]
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

    #[cfg(not(feature = "redundant"))]
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
