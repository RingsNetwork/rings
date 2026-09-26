//! The client side of the data plane: loops over a selected route, their reply keys in the tag
//! table, their first cells on the link to the guard (#834 D4, D6′, D8).
//!
//! ```text
//! send(route, (f, ā), b, v, s):  x ← ⌈now / Q⌉·Q + X₀
//!                                (cell, t_⋄, k) ← build_loop(route, (f, ā), me, b, x, v)
//!                                T ← T ∪ {t_⋄ ↦ (k, x, s)};  guard ← cell   (awaited)
//! enqueue(…):                    the same, with the cell queued on the guard's lane
//! surbs(route, b, n, s):         n × (υ, t, k) ← build_surb(return path, me, b, x)
//!                                T ← T ∪ {t ↦ (k, x, s)};  return υ₁ … υₙ
//! ```
//!
//! Law: every loop and reply block leaves its reply key in `T` before its cell leaves the node,
//! so a reply can never arrive before its entry.

use std::slice;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;

use super::OnionClientTags;
use super::OnionExpiry;
use super::OnionLink;
use super::OnionLinkSender;
use super::OnionReplySink;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::sphinx::builder::build_loop;
use crate::onion::sphinx::builder::build_surb;
use crate::onion::sphinx::builder::OnionApplication;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;

/// The loop client of one node; clones share its tag table and link emitter.
#[derive(Clone)]
pub(crate) struct OnionLoopClient {
    /// This node's DID, the position `H + 1` of its loops.
    local: Did,
    /// `T`.
    tags: OnionClientTags,
    /// The link emitter the first cells leave through.
    link_sender: OnionLinkSender,
}

impl OnionLoopClient {
    /// The loop client of the node `local`.
    pub(crate) const fn new(
        local: Did,
        tags: OnionClientTags,
        link_sender: OnionLinkSender,
    ) -> Self {
        Self {
            local,
            tags,
            link_sender,
        }
    }

    /// Send `value` into a fresh loop of `route` that applies `application` at its symbol hop,
    /// in class `class`, awaiting its reply at `sink`; returns the loop's expiry `x` once the
    /// first cell has left for the guard. The caller waits for the link: this is the departure
    /// of stream data, whose waiting is the upload's backpressure.
    ///
    /// # Errors
    ///
    /// A build failure (a value too wide for the class, or a negligible key failure), or the
    /// link send's error.
    pub(crate) async fn send(
        &self,
        scope: &Scope,
        route: &OnionRoute,
        application: &OnionApplication,
        class: OnionLoopClass,
        value: &[u8],
        sink: &OnionReplySink,
    ) -> Result<OnionExpiry> {
        let (link, cell, expiry) = self.prepare(route, application, class, value, sink)?;
        self.link_sender.send(scope.clone(), link, cell).await?;
        Ok(expiry)
    }

    /// [`Self::send`] without waiting for the link: the loop is queued on the guard's lane. This
    /// is the departure of a session's control loops (its open, credit and keep-alive), which
    /// must never hold up the session's replies.
    ///
    /// # Errors
    ///
    /// A build failure, or a full lane.
    pub(crate) fn enqueue(
        &self,
        scope: &Scope,
        route: &OnionRoute,
        application: &OnionApplication,
        class: OnionLoopClass,
        value: &[u8],
        sink: &OnionReplySink,
    ) -> Result<OnionExpiry> {
        let (link, cell, expiry) = self.prepare(route, application, class, value, sink)?;
        self.link_sender.enqueue(scope.clone(), link, cell)?;
        Ok(expiry)
    }

    /// Build one loop and register its reply key in `T` (the module law: before its cell
    /// leaves); return the guard's link, the first cell and the loop's expiry.
    fn prepare(
        &self,
        route: &OnionRoute,
        application: &OnionApplication,
        class: OnionLoopClass,
        value: &[u8],
        sink: &OnionReplySink,
    ) -> Result<(OnionLink, Bytes, OnionExpiry)> {
        let now_ms = get_epoch_ms();
        let expiry = OnionExpiry::of_build(now_ms);
        let built = build_loop(
            route.hops(),
            slice::from_ref(application),
            self.local,
            class,
            expiry,
            value,
            &mut rand::thread_rng(),
        )
        .map_err(|error| Error::OnionRouteError(OnionRouteError::LoopBuild(error.to_string())))?;
        self.tags
            .register(now_ms, built.guard, built.reply, sink.clone())?;
        Ok((
            OnionLink::new(built.guard),
            Bytes::from(built.cell.into_bytes()),
            expiry,
        ))
    }

    /// Build `count` reply blocks over `route`'s return path in class `class`, registering each
    /// block's reply key for `sink`; returns the blocks and their common expiry.
    ///
    /// # Errors
    ///
    /// A build failure of negligible probability.
    pub(crate) fn surbs(
        &self,
        route: &OnionRoute,
        class: OnionLoopClass,
        count: usize,
        sink: &OnionReplySink,
    ) -> Result<(Vec<OnionSurb>, OnionExpiry)> {
        let now_ms = get_epoch_ms();
        let expiry = OnionExpiry::of_build(now_ms);
        let guard = route.hops().guard().did;
        let mut rng = rand::thread_rng();
        // Sized once: a growing vector would leave copies of the blocks' seeds in the buffers
        // it frees.
        let mut surbs = Vec::with_capacity(count);
        for _ in 0..count {
            let (surb, reply) = build_surb(
                route.hops().return_path(),
                self.local,
                class,
                expiry,
                &mut rng,
            )
            .map_err(|error| {
                Error::OnionRouteError(OnionRouteError::LoopBuild(error.to_string()))
            })?;
            self.tags.register(now_ms, guard, reply, sink.clone())?;
            surbs.push(surb);
        }
        Ok((surbs, expiry))
    }
}
