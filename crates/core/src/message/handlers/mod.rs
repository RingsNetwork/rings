#![deny(missing_docs)]
//! This module implemented message handler of rings network.

use std::sync::Arc;

use async_trait::async_trait;

use super::effects::core_actor_steps;
use super::effects::lower_dht_action;
use super::effects::yield_core_actor_step;
use super::effects::CoreEffect;
use super::effects::CoreEffectInterpreter;
use super::MessagePayload;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::PendingConnectionAttempt;
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
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
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
        self.run_effects([CoreEffect::connect_dht_peer(peer)]).await
    }

    /// Idempotently establish DHT-driven transport connections in local quality order.
    pub(crate) async fn connect_dht_peers(
        &self,
        peers: impl IntoIterator<Item = Did>,
    ) -> Result<()> {
        for (peer, has_next) in
            core_actor_steps(self.transport.order_dht_candidates_by_quality(peers).await)
        {
            self.connect_dht_peer(peer).await?;
            if has_next {
                yield_core_actor_step().await;
            }
        }
        Ok(())
    }

    pub(crate) async fn join_dht(&self, peer: Did) -> Result<()> {
        // Default HMCC/Zave join path: maps to the JoinThenSync operation in
        // the CorrectChord spec (see tests/default/test_dht_convergence.rs).
        let Some(dht_ev) = self.transport.join_routable_peer(peer)? else {
            return Err(Error::SwarmMissDidInTable(peer));
        };
        // The local join has completed. Follow-up convergence messages are
        // best-effort: a peer can churn before these sends complete, and that
        // must not suppress the application-level Connected event.
        if let Err(e) = self.handle_dht_events(&dht_ev).await {
            tracing::warn!("Failed to handle dht events while joining {peer}: {e:?}");
        }
        Ok(())
    }

    pub(crate) async fn admit_dht_attempt(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let Some(dht_ev) = self.transport.commit_connection_admission(attempt)? else {
            return Ok(false);
        };
        // Local topology and lifecycle state are committed together. Remote
        // convergence remains best-effort because the peer may churn immediately.
        if let Err(error) = self.handle_dht_events(&dht_ev).await {
            tracing::warn!(
                peer = %attempt.peer(),
                generation = attempt.generation(),
                error = ?error,
                "failed to handle DHT events after connection admission"
            );
        }
        Ok(true)
    }

    pub(crate) async fn leave_dht_attempt(&self, attempt: PendingConnectionAttempt) -> Result<()> {
        let should_repair = self
            .dht
            .peer_may_share_storage_responsibility(
                attempt.peer(),
                self.transport.storage_redundancy(),
            )
            .await?;
        let removed = if self.transport.disconnect_attempt(attempt).await? {
            true
        } else {
            self.transport.remove_retired_attempt_topology(attempt)?
        };
        if removed && should_repair {
            self.transport.request_storage_repair();
        }
        Ok(())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) async fn leave_dht(&self, peer: Did) -> Result<()> {
        let should_repair = self
            .dht
            .peer_may_share_storage_responsibility(peer, self.transport.storage_redundancy())
            .await?;
        self.dht.remove(peer)?;
        if should_repair {
            self.transport.request_storage_repair();
        }
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
                CoreEffect::ConnectDhtPeer { peer } => {
                    connection_peers.push(peer);
                }
                effect => other_effects.push(effect),
            }
        }

        let ordered_peers = self
            .transport
            .order_dht_candidates_by_quality(connection_peers)
            .await;
        for (peer, has_next) in core_actor_steps(ordered_peers) {
            if let Err(e) = self.connect_dht_peer(peer).await {
                tracing::error!("Failed on handle multi connection action: {e:?}");
            }
            if has_next || !other_effects.is_empty() {
                yield_core_actor_step().await;
            }
        }

        for (effect, has_next) in core_actor_steps(other_effects) {
            if let Err(e) = self.run_effects([effect]).await {
                tracing::error!("Failed on handle multi action: {e:?}");
            }
            if has_next {
                yield_core_actor_step().await;
            }
        }

        Ok(())
    }

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
