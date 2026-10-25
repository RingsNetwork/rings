//! The swarm's interpretation of the storage maintenance intent to deliver this node's inbox.
//!
//! The inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so the old
//! holder's storage repair pass hands the held messages to `d` when `d` holds relay mail, and to
//! the next node along `d`'s successor list that does otherwise (`dht::relay_holder`). Every
//! storage maintenance pass the DHT emits [`InboxDelivery::deliver_inbox`]; this interpreter
//! reads the inbox from local storage when `d` holds its own inbox key, or fetches it from the
//! holder its view names, decodes the elements, and offers each it has not handed over yet
//! through the inbound
//! pipeline (handler dispatch, then the application inbox), verifying its witness first (an
//! element held ahead of this clock waits for a later pass, as
//! [`InboxElement::is_deliverable_at`] states). An element is retired by its add dot once core
//! is done with it: a message core handles itself at once, an application message only once the
//! application took it from the inbox (its consumption signal, read by a later pass). Taking
//! ends core's custody; what the application does with the message afterwards is its own.
//!
//! ```text
//!   pass:  elements ──poll──▶ Consumed ─────────────────────────────┐
//!                          ├─▶ Untaken: kept                        ├─▶ one removal of their dots
//!                          └─▶ Unknown ──witness ▸ deliver──▶ Handled or refused ┘
//!                                                       ├─▶ Queued: kept, now Untaken
//!                                                       ├─▶ origin quota spent: origin skipped
//!                                                       └─▶ no room, no reader or a local
//!                                                           failure: the pass ends
//! ```
//!
//! Laws:
//! - **At least once across restarts (#936).** An element is retired only once core handled it,
//!   refused it as the live path would drop it, or the application took it and its release from
//!   relay custody committed, so a later pass, a released inbox, a failed retirement or a restart
//!   before that never loses it: an element admitted before a restart and never taken is in
//!   custody, so its redelivery is queued again rather than refused as a replay (the custody
//!   law of `message::replay`). A crash after a take and before its release delivers that
//!   message once more, with the same transaction id. An element held again between the read
//!   and its tombstone is delivered by the next pass.
//! - **Custody follows the carrier.** Each pass over the carrier of the key's holder (this
//!   node's own store, or the holder's report) first restricts relay custody, and the set of
//!   elements handed over in this process, to the elements still stored, so custody never
//!   outlives the mail it keeps, and a pass that finds custody full ends and leaves the rest
//!   for a later pass. A pass over the report of another member of the key (#777), read when
//!   the holder is out of reach, restricts neither: that member may lag behind the holder, and
//!   an element missing from its copy is not gone (#1059).
//! - **Removal at the source.** A pass removes the elements it is done with at the member it
//!   read them from, which applies the removal as a member of the key; repair carries the
//!   tombstone to the other members (`Law (Chain repair)`). An element a lagging member
//!   still offers after its removal is refused by the receiver window and retired again.
//! - **Never waits on the application.** An application message is offered only through a
//!   reservation in the application inbox; when it finds no room or no reader, the pass ends
//!   and leaves this element and the rest stored for a later pass.
//! - **Per-origin quota.** An origin whose quota is spent for now keeps its later elements stored
//!   for a later pass; the elements of other origins are still offered.
//! - **One retirement per pass.** The pass retires every element it is done with by one removal
//!   of their dots, so a pass costs one storage write. A pass cut short by its step deadline
//!   retires nothing: the next pass finds the taken elements still consumed, and refuses the
//!   ones core handled as replays, retiring both.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::oneshot;

use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::inbox::InboxElement;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryDot;
use crate::dht::relay_holder::RelayMembers;
use crate::dht::Did;
use crate::dht::InboxDelivery;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::handlers::storage::remove_relay_elements_at;
use crate::message::FetchRelayInbox;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::swarm::application::RelayConsumption;
use crate::swarm::callback::LocalDelivered;
use crate::swarm::callback::LocalDelivery;
use crate::swarm::detached::spawn_detached;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Delivery of this node's inbox to the application inbox.
pub(crate) struct SwarmInboxDelivery {
    transport: Arc<SwarmTransport>,
}

/// What a pass does after offering one element.
enum Offered {
    /// Core is done with it: retire it.
    Retire,
    /// Keep it stored for a later pass and go on with the next element.
    Keep,
    /// Keep it and every later element stored for a later pass.
    EndPass,
}

/// Whether `error` defers the payload rather than refusing it: a failure or the absence of this
/// node's own replay store, or a full relay custody. Nothing was committed, so the element waits for a
/// later pass instead of being retired as one the live path would drop.
fn defers(error: &Error) -> bool {
    matches!(
        error,
        Error::TransactionReplayPersistence { .. }
            | Error::RelayCustodyFull { .. }
            | Error::TransactionReplayStorageUnavailable
    )
}

impl SwarmInboxDelivery {
    /// Deliver through `transport` to its application inbox.
    pub(crate) fn new(transport: Arc<SwarmTransport>) -> Self {
        Self { transport }
    }

    /// Tombstone `removal` at `source`, the member the carrier was read from (this node for its
    /// own store), in one attempt (`Attempts::Single`): a refusal before acceptance is
    /// `SingleAttemptRefused`, and the removal had no effect.
    async fn retire(&self, removal: Entry, source: Did) -> Result<()> {
        remove_relay_elements_at(&self.transport, removal, source).await
    }

    /// The signal that `element` was taken and released from relay custody: a detached task
    /// waits for `taken`, the application's take, then commits the release (the custody law), and
    /// only then fires the signal, so the element is retired only after its release.
    ///
    /// Post: a take whose release fails, or that no runtime can schedule, never fires: the
    /// element stays stored and in custody, and is delivered again after a restart.
    fn release_on_take(
        &self,
        element: &InboxElement,
        taken: oneshot::Receiver<()>,
    ) -> oneshot::Receiver<()> {
        let (release, released) = oneshot::channel();
        let transport = self.transport.clone();
        let transaction = element.held.payload.transaction.clone();
        let task = async move {
            if taken.await.is_err() {
                return;
            }
            match transport.release_relayed_transaction(&transaction).await {
                Ok(()) => {
                    // A receiver dropped meanwhile is a pass that forgot the element.
                    release.send(()).ok();
                }
                Err(error) => tracing::warn!(
                    tx_id = %transaction.tx_id,
                    error = ?error,
                    "relay delivery taken but not released from custody"
                ),
            }
        };
        if spawn_detached(Box::pin(task)).is_err() {
            tracing::warn!(
                tx_id = %element.tx_id(),
                "no runtime to release a relay delivery from custody"
            );
        }
        released
    }

    /// Offer one element not yet handed over, unless its origin's quota is spent for this pass
    /// (`spent`) or its witness does not admit delivery now.
    async fn offer(
        &self,
        delivery: &LocalDelivery,
        element: &InboxElement,
        now_ms: u128,
        spent: &mut BTreeSet<Did>,
    ) -> Offered {
        let origin = element.held.payload.transaction.origin();
        if spent.contains(&origin) || !element.is_deliverable_at(now_ms, self.transport.network_id)
        {
            return Offered::Keep;
        }
        let local = self.transport.dht.did;
        let tx_id = element.tx_id();
        match delivery.deliver(&element.held.payload).await {
            Ok(LocalDelivered::Handled) => Offered::Retire,
            Ok(LocalDelivered::Queued(taken)) => {
                let released = self.release_on_take(element, taken);
                self.transport
                    .application()
                    .relay_in_flight
                    .queued(tx_id, released);
                Offered::Keep
            }
            Ok(LocalDelivered::Deferred) => Offered::EndPass,
            Err(Error::OriginQuota(_)) => {
                spent.insert(origin);
                Offered::Keep
            }
            Err(error) if defers(&error) => {
                tracing::warn!(%local, %tx_id, error = ?error, "relay inbox delivery deferred");
                Offered::EndPass
            }
            Err(error) => {
                tracing::warn!(%local, %tx_id, error = ?error, "relay inbox message refused by core");
                Offered::Retire
            }
        }
    }
}

impl SwarmInboxDelivery {
    /// Ask the members of this node's inbox key for the inbox, the holder first, then each next
    /// member the request cannot reach (#777, `Law (Chain repair)`): the report is drained by
    /// [`Self::deliver_fetched`].
    ///
    /// Post: `Ok` once one request was sent; otherwise the last send's error.
    async fn fetch(&self, members: &[Did]) -> Result<()> {
        let mut last = Ok(());
        for member in members {
            last = self
                .transport
                .protocol_egress()
                .send_message(Message::FetchRelayInbox(FetchRelayInbox), *member)
                .await
                .map(drop);
            if last.is_ok() {
                break;
            }
        }
        last
    }

    /// Drain the inbox `member` reported, iff `member` is a member of this node's inbox key under
    /// this node's own view (`Law (Agreement)` of `dht::relay_holder`, `Law (Members)`), and the
    /// carrier is this node's inbox and passes admission, as a hand-off's would. Only the
    /// holder's report restricts custody (the module's laws).
    ///
    /// Post: a report from any other node, or of any other carrier, is refused and drains
    /// nothing; custody is left as it is until a pass sees the holder's carrier.
    pub(crate) async fn deliver_fetched(&self, member: Did, inbox: Option<Entry>) -> Result<()> {
        let local = self.transport.dht.did;
        let key = inbox_key(local);
        let RelayMembers::Members(members) = self.transport.dht.relay_members(key)? else {
            return Err(Error::InvalidMessage(format!(
                "relay inbox report from {member}, which this view places no member of"
            )));
        };
        if member == local || !members.contains(&member) {
            return Err(Error::InvalidMessage(format!(
                "relay inbox report from {member}, which does not hold this inbox"
            )));
        }
        let restricts = members.first() == Some(&member);
        let inbox = match inbox {
            Some(entry) if entry.kind.is_relay_inbox() && entry.did == key => Some(
                entry
                    .validate_admissible_at(get_epoch_ms(), self.transport.network_id)?
                    .into_entry(),
            ),
            Some(_) => {
                return Err(Error::InvalidMessage(format!(
                    "relay inbox report from {member} carries another carrier"
                )));
            }
            None => None,
        };
        self.drain(inbox, member, restricts).await
    }

    /// One pass over the inbox `inbox`, read from `source` (this node's own store, or a member of
    /// the key that reported it), as the module laws say: custody and the handed-over set are
    /// restricted to it iff `restricts`, `source` being the key's holder. A pass that finds
    /// another running leaves the elements to the next.
    ///
    /// Post: the elements were taken in order until one found no room, no reader or a local
    /// failure; every element core is done with was tombstoned at the holder by one removal.
    ///
    /// A removal refused before acceptance (`SingleAttemptRefused`, a detached `Cancelled`
    /// included) ends the drain with that error: the element stays at its holder and is taken
    /// again on the next drain. No element is lost, and none is tombstoned twice.
    async fn drain(&self, inbox: Option<Entry>, source: Did, restricts: bool) -> Result<()> {
        let Some(_pass) = self.transport.application().relay_drain.try_lock() else {
            return Ok(());
        };
        let now_ms = get_epoch_ms();
        let Some(inbox) = inbox else {
            // No carrier at the holder (expired, never held, or no holder to keep one): nothing
            // is stored, so custody restricts to nothing (the custody law's first step).
            if restricts {
                return self.transport.restrict_relay_custody([]).await;
            }
            return Ok(());
        };
        let stored = inbox.stored_inbox_elements();
        let relay = &self.transport.application().relay_in_flight;
        if restricts {
            self.transport
                .restrict_relay_custody(
                    stored
                        .iter()
                        .map(|element| &element.held.payload.transaction),
                )
                .await?;
            relay.retain(&stored.iter().map(InboxElement::tx_id).collect());
        }
        let delivery = LocalDelivery::new(self.transport.clone());
        let mut retired: Vec<EntryDot> = Vec::new();
        let mut spent = BTreeSet::new();
        for element in stored {
            let offered = match relay.poll(element.tx_id()) {
                RelayConsumption::Consumed => Offered::Retire,
                RelayConsumption::Untaken => Offered::Keep,
                RelayConsumption::Unknown => {
                    self.offer(&delivery, &element, now_ms, &mut spent).await
                }
            };
            match offered {
                Offered::Retire => retired.push(element.dot),
                Offered::Keep => {}
                Offered::EndPass => break,
            }
        }
        if retired.is_empty() {
            return Ok(());
        }
        self.retire(inbox.removal_of(retired), source).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl InboxDelivery for SwarmInboxDelivery {
    /// Post: the inbox was drained from local storage when this node holds its own inbox key,
    /// fetched from the members its view names otherwise, the holder first
    /// (`dht::relay_holder`, `Law (Members)`), and drained as nothing when its view names the
    /// key's owner and no member, so custody follows a carrier that cannot exist. A view that
    /// cannot place the key leaves custody as it is.
    async fn deliver_inbox(&self) -> Result<()> {
        let local = self.transport.dht.did;
        match self.transport.dht.relay_members(inbox_key(local))? {
            RelayMembers::Members(members) if members.first() == Some(&local) => {
                let key = StorageKey::inbox_of(local);
                let inbox = self
                    .transport
                    .dht
                    .live_storage_entry(key, get_epoch_ms())
                    .await?;
                self.drain(inbox, local, true).await
            }
            RelayMembers::Members(members) => self.fetch(&members).await,
            RelayMembers::Unheld => self.drain(None, local, true).await,
            RelayMembers::Unknown => Ok(()),
        }
    }
}
