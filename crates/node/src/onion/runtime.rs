//! The onion runtime of one node: its data plane installed in the protocol registry, and the
//! client its own sessions dial through. One construction for native and browser nodes; the
//! platform decides only which worlds the exit's algebra registers.
//!
//! ```text
//! install(extensions):
//!   link sender, tag table T, exit accounting            shared by every part below
//!   ⟦−⟧ ← { https ↦ sessions over the fetch world,        if the role's offer names https
//!           tcp   ↦ sessions over the socket world }      if it names tcp (native only)
//!   register (OnionCircuitProtocol, shell(A over 2·𝓡, T, ⟦−⟧))
//!   link feed ─observes─▶ core's link facts ─injects─▶ the data plane
//!   loop client (me, T, link sender)                      what sessions dial through
//! ```
//!
//! Law: what the node registers and what it evaluates agree by construction: the algebra holds
//! exactly the offered services the platform can interpret, and the admission epoch is the
//! epoch the registrations publish.

use std::num::NonZeroUsize;
use std::sync::Arc;

use super::circuit::OnionAlgebra;
use super::circuit::OnionCircuitProtocol;
use super::circuit::OnionCircuitShell;
use super::circuit::OnionClientTags;
use super::circuit::OnionLinkFeed;
use super::circuit::OnionLinkSender;
#[cfg(all(test, rings_native))]
use super::circuit::OnionLinkWitness;
use super::circuit::OnionLoopClient;
use super::circuit::ONION_CIRCUIT_NAMESPACE;
use super::exit_accounting::OnionExitAccounting;
use super::https::OnionHttpsWorld;
use super::session::dial;
use super::session::dial::OnionClientStream;
use super::session::dial::OnionSessionRequest;
use super::session::serve::OnionExitSessions;
use super::OnionExitOffer;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::DynLinkObserver;
use crate::extension::ext::Extensions;
use crate::extension::ext::Scope;

/// The installed onion runtime of one node.
#[derive(Clone)]
pub(crate) struct OnionRuntime {
    /// The data plane's scope, which the node's own loops are sent under.
    scope: Scope,
    /// The node's loop client.
    loops: OnionLoopClient,
    /// What the runtime's clones own together: when the last one drops, the data plane's
    /// tasks end.
    _life: Arc<OnionRuntimeLife>,
    /// The witness of the data plane's link table.
    #[cfg(all(test, rings_native))]
    links: OnionLinkWitness,
}

impl OnionRuntime {
    /// Install the data plane of the processor behind `extensions` (see the module diagram).
    ///
    /// # Errors
    ///
    /// A registry or runtime failure, or a connection registry of capacity zero.
    pub(crate) fn install(extensions: &Extensions) -> Result<Self> {
        let core = extensions.core();
        Self::install_with(extensions, |accounting, link_sender| {
            exit_algebra(core.onion_role().exit(), accounting, link_sender)
        })
    }

    /// [`Self::install`] with the exit algebra `algebra` builds over the node's exit accounting
    /// and link sender: the worlds are a parameter, which the platform fixes and a test chooses.
    ///
    /// # Errors
    ///
    /// As for [`Self::install`].
    pub(crate) fn install_with(
        extensions: &Extensions,
        algebra: impl FnOnce(&OnionExitAccounting, &OnionLinkSender) -> OnionAlgebra,
    ) -> Result<Self> {
        let core = extensions.core();
        let link_sender = OnionLinkSender::new(core.onion_idle_floor());
        let tags = OnionClientTags::default();
        let accounting = OnionExitAccounting::default();
        let capacity = NonZeroUsize::new(core.connection_registry_capacity()?)
            .ok_or_else(|| Error::InvalidConfig("connection registry of capacity 0".to_string()))?;
        let shell = OnionCircuitShell::new(
            core.delegatee_key().clone(),
            core.onion_role().registers_relay(),
            core.onion_process_epoch(),
            capacity,
            tags.clone(),
            link_sender.clone(),
            algebra(&accounting, &link_sender),
        );
        #[cfg(all(test, rings_native))]
        let links = shell.link_witness();
        extensions.register(OnionCircuitProtocol, shell)?;
        let scope = Scope::new(core.clone(), ONION_CIRCUIT_NAMESPACE.to_string());
        let feed = OnionLinkFeed::start(scope.clone())?;
        let observer: Arc<DynLinkObserver> = feed.clone();
        extensions.observe_links(&observer)?;
        Ok(Self {
            scope,
            loops: OnionLoopClient::new(core.did(), tags, link_sender.clone()),
            _life: Arc::new(OnionRuntimeLife {
                _feed: feed,
                link_sender,
            }),
            #[cfg(all(test, rings_native))]
            links,
        })
    }

    /// The witness of the data plane's link table.
    #[cfg(all(test, rings_native))]
    pub(crate) fn links(&self) -> &OnionLinkWitness {
        &self.links
    }

    /// Open one session over its selected route and wait for the exit's answer.
    ///
    /// # Errors
    ///
    /// The exit's refusal, a timeout, or a send failure (see [`dial::open`]).
    pub(crate) async fn open(&self, request: OnionSessionRequest) -> Result<OnionClientStream> {
        dial::open(self.loops.clone(), self.scope.clone(), request).await
    }
}

/// The data plane's long-lived parts, owned by the runtime's clones together.
///
/// Law (lifetime): the feed's drain and tick end when the feed drops, and every emitter ends
/// when its lane closes; so once the last runtime clone drops, no onion task keeps the node
/// alive.
struct OnionRuntimeLife {
    /// The link feed, held for its lifetime: this is its only strong owner.
    _feed: Arc<OnionLinkFeed>,
    /// The node's link emitter.
    link_sender: OnionLinkSender,
}

impl Drop for OnionRuntimeLife {
    /// Close every lane, which stops its emitter; the feed drops after this.
    fn drop(&mut self) {
        if let Err(error) = self.link_sender.close_all() {
            tracing::debug!(%error, "onion runtime could not close its link lanes");
        }
    }
}

/// `⟦−⟧` of the offer: one session interpretation per offered service this platform
/// interprets, all sharing the exit accounting and the link sender.
pub(super) fn exit_algebra(
    offer: Option<&OnionExitOffer>,
    accounting: &OnionExitAccounting,
    link_sender: &OnionLinkSender,
) -> OnionAlgebra {
    let Some(offer) = offer else {
        return OnionAlgebra::default();
    };
    offer
        .services()
        .iter()
        .fold(OnionAlgebra::default(), |algebra, service| {
            let policy = offer.policy().clone();
            if *service == OnionServiceName::https() {
                return algebra.register(
                    service.clone(),
                    OnionExitSessions::new(
                        OnionHttpsWorld::new(policy.clone(), accounting.clone()),
                        policy,
                        accounting.clone(),
                        link_sender.clone(),
                    ),
                );
            }
            // Σ_W = {tcp, https} is closed, so the other service is `tcp`, which needs sockets:
            // the processor builder refuses a browser offer naming it.
            #[cfg(rings_native)]
            {
                algebra.register(
                    service.clone(),
                    OnionExitSessions::new(
                        super::tcp::OnionTcpWorld,
                        policy,
                        accounting.clone(),
                        link_sender.clone(),
                    ),
                )
            }
            #[cfg(not(rings_native))]
            {
                let _ = policy;
                algebra
            }
        })
}
