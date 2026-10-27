//! The ingress: accepts TCP for the whole prefix and relays each connection to the owner and
//! port its destination names.
//!
//! ```text
//!   route : (Address, Port) ⇀ (Owner, Port, ProxyProtocol)
//!   route(a, p) = (o, p, x)    if B a = o  ∧  serves(announcement(o), p) = x
//!
//!   accept ─▶ enter ─▶ dst = (a, p) ─▶ B a ─▶ announcement(o) ─▶ serves p ─▶ bind o ─▶ forward
//!              │ full             │ ⊥        │ ⊥ / Err            │ ⊥         │ full
//!              ▼                  ▼          ▼                    ▼           ▼
//!            close              close      close                close       close
//! ```
//!
//! A listener is bound to the IPv6 wildcard of its port, IPv6 only. On Linux it receives every
//! address routed to the host as local, the whole prefix included (`ip -6 route add local P dev
//! lo`), and nothing over IPv4, so a host service on the same port's IPv4 wildcard keeps it. An
//! IPv6 host service keeps its connections when it listens on the host's own address: the
//! kernel prefers the listener bound to the exact address. A connection to an address the book
//! does not hold is closed at once, so the relay never reaches an owner its DNS did not name,
//! nor a port the owner does not serve.
//!
//! Admission bounds the connections in flight in two steps: every accepted connection takes a
//! slot of the total before anything else, so a flood cannot pile up routing work, and a routed
//! connection then takes a slot of its owner, so one owner's popularity, or a flood aimed at it,
//! cannot take every slot. A slot is held until the forward returns, which is when the
//! connection is done.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::announcement::ProxyProtocol;
use crate::capability::Connection;
use crate::capability::Directory;
use crate::capability::Forward;
use crate::owner::Owner;
use crate::proxy::proxy_v2_header;
use crate::registry::AddressRegistry;
use crate::socket::listen_tcp;

/// Default bound on the connections the relay carries at once.
pub const DEFAULT_MAX_CONNECTIONS: usize = 4_096;
/// Default bound on the connections the relay carries at once to one owner.
pub const DEFAULT_MAX_CONNECTIONS_PER_OWNER: usize = 64;
/// First pause after a failed accept; it doubles on each further failure.
const ACCEPT_BACKOFF_FIRST: Duration = Duration::from_millis(10);
/// Longest pause after a failed accept.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// The ingress could not start.
#[derive(Debug, Error)]
#[error("ingress cannot listen on {address}: {source}")]
pub struct IngressError {
    /// The address it tried.
    pub address: SocketAddrV6,
    /// The failure.
    pub source: std::io::Error,
}

/// The bounds on concurrent connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionLimits {
    /// Most connections at once, over all owners, routed or not.
    pub total: usize,
    /// Most connections at once to one owner.
    pub per_owner: usize,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            total: DEFAULT_MAX_CONNECTIONS,
            per_owner: DEFAULT_MAX_CONNECTIONS_PER_OWNER,
        }
    }
}

/// The connections in flight. Pure: [`Admission::enter`], [`Admission::bind`] and
/// [`Admission::leave`] are its only transitions.
///
/// ```text
///   Invariant:  live ≤ total  ∧  ∀o. live(o) ≤ per_owner  ∧  Σₒ live(o) ≤ live
///               ∀o ∈ dom live. live(o) > 0
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admission<O> {
    limits: AdmissionLimits,
    live: usize,
    per_owner: BTreeMap<O, usize>,
}

impl<O> Admission<O>
where O: Owner
{
    /// No connection, under `limits`.
    pub fn new(limits: AdmissionLimits) -> Self {
        Self {
            limits,
            live: 0,
            per_owner: BTreeMap::new(),
        }
    }

    /// Admit one more connection to the total; return whether it was admitted.
    pub fn enter(&mut self) -> bool {
        if self.live >= self.limits.total {
            return false;
        }
        self.live = self.live.saturating_add(1);
        true
    }

    /// Admit an entered connection to `owner`; return whether it was admitted.
    ///
    /// Pre: the connection entered and is bound to no owner yet.
    pub fn bind(&mut self, owner: O) -> bool {
        let to_owner = self.per_owner.entry(owner).or_insert(0);
        if *to_owner >= self.limits.per_owner {
            if *to_owner == 0 {
                self.per_owner.remove(&owner);
            }
            return false;
        }
        *to_owner = to_owner.saturating_add(1);
        true
    }

    /// Release an entered connection, and its slot of `owner` when it was bound to one.
    pub fn leave(&mut self, owner: Option<&O>) {
        if let Some(owner) = owner {
            if let Some(to_owner) = self.per_owner.get_mut(owner) {
                *to_owner = to_owner.saturating_sub(1);
                if *to_owner == 0 {
                    self.per_owner.remove(owner);
                }
            }
        }
        self.live = self.live.saturating_sub(1);
    }

    /// The connections in flight.
    pub fn live(&self) -> usize {
        self.live
    }

    /// The connections in flight to `owner`.
    pub fn live_for(&self, owner: &O) -> usize {
        self.per_owner.get(owner).copied().unwrap_or(0)
    }
}

/// One admitted connection's slots: released when dropped, so a connection that ends any way,
/// a panicking forward included, gives its slots back.
struct Slot<O>
where O: Owner
{
    admission: Arc<Mutex<Admission<O>>>,
    owner: Option<O>,
}

impl<O> Slot<O>
where O: Owner
{
    /// A slot of the total, if `admission` admits one more connection.
    fn enter(admission: &Arc<Mutex<Admission<O>>>) -> Option<Self> {
        let entered = admission
            .lock()
            .map(|mut admission| admission.enter())
            .unwrap_or(false);
        entered.then(|| Self {
            admission: Arc::clone(admission),
            owner: None,
        })
    }

    /// Take a slot of `owner` too; return whether the owner admits one more connection.
    fn bind(&mut self, owner: O) -> bool {
        let bound = self
            .admission
            .lock()
            .map(|mut admission| admission.bind(owner))
            .unwrap_or(false);
        if bound {
            self.owner = Some(owner);
        }
        bound
    }
}

impl<O> Drop for Slot<O>
where O: Owner
{
    fn drop(&mut self) {
        if let Ok(mut admission) = self.admission.lock() {
            admission.leave(self.owner.as_ref());
        }
    }
}

/// Where a connection goes: `route` in the module doc.
struct Route<O> {
    owner: O,
    port: NonZeroU16,
    proxy: ProxyProtocol,
}

/// The ingress of a relay: where it listens, and what it needs to route and carry connections.
pub struct Ingress<D, F>
where
    D: Directory,
    F: Forward<Owner = D::Owner>,
{
    listen: Ipv6Addr,
    ports: BTreeSet<NonZeroU16>,
    registry: Arc<AddressRegistry<D::Owner>>,
    directory: Arc<D>,
    forward: Arc<F>,
    admission: Arc<Mutex<Admission<D::Owner>>>,
}

impl<D, F> Ingress<D, F>
where
    D: Directory,
    F: Forward<Owner = D::Owner>,
{
    /// An ingress listening on `listen` at `ports`.
    pub fn new(
        listen: Ipv6Addr,
        ports: BTreeSet<NonZeroU16>,
        registry: Arc<AddressRegistry<D::Owner>>,
        directory: Arc<D>,
        forward: Arc<F>,
        limits: AdmissionLimits,
    ) -> Self {
        Self {
            listen,
            ports,
            registry,
            directory,
            forward,
            admission: Arc::new(Mutex::new(Admission::new(limits))),
        }
    }

    /// Bind an IPv6-only listener on every configured port.
    ///
    /// Binding fails as a whole, so a relay never runs with some of its ports silently missing.
    pub fn bind(&self) -> Result<Vec<TcpListener>, IngressError> {
        self.ports
            .iter()
            .map(|port| {
                let address = SocketAddrV6::new(self.listen, port.get(), 0, 0);
                listen_tcp(SocketAddr::V6(address))
                    .map_err(|source| IngressError { address, source })
            })
            .collect()
    }

    /// Serve on `listeners` until the returned set is aborted or dropped. A connection is routed
    /// by the port it arrived on, whichever listener accepted it.
    pub fn serve(self, listeners: Vec<TcpListener>) -> JoinSet<()> {
        let ingress = Arc::new(self);
        let mut tasks = JoinSet::new();
        for listener in listeners {
            tasks.spawn(Arc::clone(&ingress).accept_loop(listener));
        }
        tasks
    }

    /// Accept on `listener` forever, admitting each connection to the total and handing it to
    /// its own task. A failed accept (out of descriptors, say) pauses the loop, longer on each
    /// further failure, so it never spins.
    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        let mut connections = JoinSet::new();
        let mut backoff = ACCEPT_BACKOFF_FIRST;
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _peer)) => {
                        backoff = ACCEPT_BACKOFF_FIRST;
                        match Slot::enter(&self.admission) {
                            Some(slot) => {
                                connections.spawn(Arc::clone(&self).carry(stream, slot));
                            }
                            None => tracing::debug!("dns relay ingress: at its connection limit"),
                        }
                    }
                    Err(error) => {
                        tracing::warn!("dns relay ingress: accept failed: {error}");
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2).min(ACCEPT_BACKOFF_MAX);
                    }
                },
                // Reap finished connections so the set does not grow with every one served.
                Some(_finished) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    }

    /// Route `stream` by its destination and carry it, holding `slot`, or close it.
    async fn carry(self: Arc<Self>, stream: TcpStream, mut slot: Slot<D::Owner>) {
        let (Ok(client), Ok(destination)) = (stream.peer_addr(), stream.local_addr()) else {
            return;
        };
        let Some(route) = self.route(destination).await else {
            return;
        };
        if !slot.bind(route.owner) {
            tracing::debug!(
                "dns relay ingress: {} is at its connection limit",
                route.owner.label()
            );
            return;
        }
        let preamble = match route.proxy {
            ProxyProtocol::Off => Vec::new(),
            ProxyProtocol::V2 => match proxy_v2_header(client, destination) {
                Ok(header) => header,
                Err(error) => {
                    tracing::warn!("dns relay ingress: {error}");
                    return;
                }
            },
        };
        let connection = Connection {
            stream,
            owner: route.owner,
            port: route.port,
            preamble,
        };
        if let Err(error) = self.forward.forward(connection).await {
            tracing::debug!("dns relay ingress: {}: {error}", route.owner.label());
        }
    }

    /// `route(destination)` in the module doc.
    async fn route(&self, destination: SocketAddr) -> Option<Route<D::Owner>> {
        let SocketAddr::V6(destination) = destination else {
            return None;
        };
        let port = NonZeroU16::new(destination.port())?;
        let owner = self.registry.owner_at(destination.ip()).await?;
        let announcement = match self.directory.announcement(owner).await {
            Ok(announcement) => announcement?,
            Err(error) => {
                tracing::debug!("dns relay ingress: {}: {error}", owner.label());
                return None;
            }
        };
        let proxy = announcement.serves(port.get())?;
        Some(Route { owner, port, proxy })
    }
}

#[cfg(test)]
mod test_ingress;
