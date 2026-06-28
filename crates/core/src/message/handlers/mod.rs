#![warn(missing_docs)]
//! This module implemented message handler of rings network.

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use super::effects::lower_dht_action;
use super::effects::ConnectionFunctor;
use super::effects::CoreEffect;
use super::effects::CoreEffectInterpreter;
use super::MessagePayload;
use crate::dht::ChordStorageRepair;
use crate::dht::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::SwarmTransport;

/// Operator and Handler for Connection
pub mod connection;
/// Operator and Handler for CustomMessage
pub mod custom;
/// Operator and Handler for E2E encrypted messages
pub mod e2e;
/// Operator and handler for DHT stabilization
pub mod stabilization;
/// Operator and Handler for Storage
pub mod storage;
/// Operator and Handler for Subring
pub mod subring;

/// Shared message-handler handle.
///
/// Clone law: cloning duplicates `Arc` handles to the same transport, DHT
/// state, and callback. It never forks protocol state or transfers ownership.
#[derive(Clone)]
pub struct MessageHandler {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
    swarm_callback: SharedSwarmCallback,
}

/// Generic trait for handle message ,inspired by Actor-Model.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait HandleMsg<T> {
    /// Message handler.
    async fn handle(&self, ctx: &MessagePayload, msg: &T) -> Result<()>;
}

impl MessageHandler {
    /// Create a new MessageHandler instance.
    pub fn new(transport: Arc<SwarmTransport>, swarm_callback: SharedSwarmCallback) -> Self {
        let dht = transport.dht.clone();
        Self {
            transport,
            dht,
            swarm_callback,
        }
    }

    fn inner_callback(&self) -> InnerSwarmCallback {
        InnerSwarmCallback::new(self.transport.clone(), self.swarm_callback.clone())
    }

    pub(crate) async fn run_effects<'payload>(
        &self,
        effects: impl IntoIterator<Item = CoreEffect<'payload>>,
    ) -> Result<()> {
        CoreEffectInterpreter::new(&self.transport, &self.swarm_callback)
            .run_all(effects)
            .await
    }

    /// Idempotently establish a DHT-driven transport connection.
    ///
    /// Self and already-connected peers are no-ops. `AlreadyConnected` is treated
    /// as success so concurrent DHT actions racing through `MultiActions` do not
    /// fail the whole handler.
    pub(crate) async fn connect_dht_peer(&self, peer: Did) -> Result<()> {
        self.run_effects([ConnectionFunctor::connect_dht_peer(peer).into()])
            .await
    }

    /// Idempotently establish DHT-driven transport connections in local quality order.
    pub(crate) async fn connect_dht_peers(
        &self,
        peers: impl IntoIterator<Item = Did>,
    ) -> Result<()> {
        for peer in self.transport.order_dht_candidates_by_quality(peers).await {
            self.connect_dht_peer(peer).await?;
        }
        Ok(())
    }

    pub(crate) async fn join_dht(&self, peer: Did) -> Result<()> {
        // Default HMCC/Zave join path: maps to the JoinThenSync operation in
        // the CorrectChord spec (see tests/default/dht_convergence.rs).
        let conn = self
            .transport
            .get_connection(peer)
            .ok_or(Error::SwarmMissDidInTable(peer))?;
        let dht_ev = self.dht.join_then_sync(conn).await?;
        // The local join has completed. Follow-up convergence messages are
        // best-effort: a peer can churn before these sends complete, and that
        // must not suppress the application-level Connected event.
        if let Err(e) = self.handle_dht_events(&dht_ev).await {
            tracing::warn!("Failed to handle dht events while joining {peer}: {e:?}");
        }
        Ok(())
    }

    pub(crate) async fn leave_dht(&self, peer: Did) -> Result<()> {
        if self
            .transport
            .get_and_check_connection(peer)
            .await
            .is_none()
        {
            let should_repair = self
                .dht
                .peer_may_share_storage_responsibility(peer, self.transport.storage_redundancy())
                .await?;
            self.dht.remove(peer)?;
            if should_repair {
                let repair = self
                    .dht
                    .republish_local_entries(self.transport.storage_redundancy())
                    .await?;
                storage::handle_storage_repair_act(self.transport.clone(), repair).await?;
            }
        };
        Ok(())
    }

    fn collect_dht_effects(
        &self,
        act: &PeerRingAction,
        effects: &mut Vec<CoreEffect<'static>>,
    ) -> Result<()> {
        match act {
            PeerRingAction::MultiActions(acts) => {
                for act in acts {
                    self.collect_dht_effects(act, effects)?;
                }
                Ok(())
            }
            act => {
                if let Some(effect) =
                    lower_dht_action(act, |did| self.transport.get_connection(did).is_some())?
                {
                    effects.push(effect);
                }
                Ok(())
            }
        }
    }

    async fn run_prioritized_dht_effects(&self, effects: Vec<CoreEffect<'static>>) -> Result<()> {
        let mut connection_peers = Vec::new();
        let mut other_effects = Vec::new();
        for effect in effects {
            match effect {
                CoreEffect::Connection(ConnectionFunctor::ConnectDhtPeer { peer }) => {
                    connection_peers.push(peer);
                }
                effect => other_effects.push(effect),
            }
        }

        for peer in self
            .transport
            .order_dht_candidates_by_quality(connection_peers)
            .await
        {
            if let Err(e) = self.connect_dht_peer(peer).await {
                tracing::error!("Failed on handle multi connection action: {e:?}");
            }
        }

        for effect in other_effects {
            if let Err(e) = self.run_effects([effect]).await {
                tracing::error!("Failed on handle multi action: {e:?}");
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub(crate) async fn handle_dht_events(&self, act: &PeerRingAction) -> Result<()> {
        if matches!(act, PeerRingAction::MultiActions(_)) {
            let mut effects = Vec::new();
            self.collect_dht_effects(act, &mut effects)?;
            self.run_prioritized_dht_effects(effects).await
        } else {
            let effects =
                lower_dht_action(act, |did| self.transport.get_connection(did).is_some())?;
            self.run_effects(effects).await
        }
    }
}
