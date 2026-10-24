//! The ingress: accepts TCP for the whole prefix and relays each connection to the service its
//! destination names.
//!
//! ```text
//!   route : (Address, Port) ⇀ (Owner, ServiceName)
//!   route(a, p) = (o, s)    if B a = o  ∧  service_at(announcement(o), p) = s
//!
//!   accept ─▶ dst = (a, p) ─▶ B a ─▶ announcement(o) ─▶ service_at p ─▶ admit o ─▶ forward
//!                              │ ⊥        │ ⊥ / Err          │ ⊥            │ full
//!                              ▼          ▼                  ▼              ▼
//!                            close      close              close          close
//! ```
//!
//! The listener for a port is bound to the wildcard address, which on Linux receives every
//! address routed to the host as local, the whole prefix included (`ip -6 route add local P dev
//! lo`). A host service bound to the host's own address on the same port keeps its connections:
//! the kernel prefers the listener bound to the exact address. A connection to an address the
//! book does not hold is closed at once, so the relay never reaches an owner its DNS did not
//! name, nor a port the owner does not serve.
//!
//! Admission bounds the connections the relay carries at once, in total and per owner, so one
//! owner's popularity, or a flood aimed at it, cannot take every slot.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::announcement::ServiceName;
use crate::capability::Directory;
use crate::capability::Forward;
use crate::owner::Owner;
use crate::registry::AddressRegistry;

/// Default bound on the connections the relay carries at once.
pub const DEFAULT_MAX_CONNECTIONS: usize = 4_096;
/// Default bound on the connections the relay carries at once to one owner.
pub const DEFAULT_MAX_CONNECTIONS_PER_OWNER: usize = 64;

/// The ingress could not start.
#[derive(Debug, Error)]
#[error("ingress cannot listen on {address}: {source}")]
pub struct IngressError {
    /// The address it tried.
    pub address: SocketAddr,
    /// The failure.
    pub source: std::io::Error,
}

/// The bounds on concurrent connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionLimits {
    /// Most connections at once, over all owners.
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

/// The connections the relay carries now, per owner. Pure: [`Admission::admit`] and
/// [`Admission::release`] are its only transitions.
///
/// ```text
///   Invariant:  Σ live ≤ total  ∧  ∀o. live(o) ≤ per_owner  ∧  ∀o ∈ dom live. live(o) > 0
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admission<O> {
    limits: AdmissionLimits,
    live: BTreeMap<O, usize>,
    total: usize,
}

impl<O> Admission<O>
where O: Owner
{
    /// No connection, under `limits`.
    pub fn new(limits: AdmissionLimits) -> Self {
        Self {
            limits,
            live: BTreeMap::new(),
            total: 0,
        }
    }

    /// Whether one more connection to `owner` fits the limits.
    pub fn permits(&self, owner: &O) -> bool {
        let to_owner = self.live.get(owner).copied().unwrap_or(0);
        self.total < self.limits.total && to_owner < self.limits.per_owner
    }

    /// Admit one connection to `owner`; return whether it was admitted.
    ///
    /// Preservation: it counts the connection only when [`Self::permits`] holds.
    pub fn admit(&mut self, owner: O) -> bool {
        if !self.permits(&owner) {
            return false;
        }
        let to_owner = self.live.entry(owner).or_insert(0);
        *to_owner = to_owner.saturating_add(1);
        self.total = self.total.saturating_add(1);
        true
    }

    /// Release one admitted connection to `owner`.
    ///
    /// Pre: a connection to `owner` was admitted and not yet released.
    pub fn release(&mut self, owner: &O) {
        let Some(to_owner) = self.live.get_mut(owner) else {
            return;
        };
        *to_owner = to_owner.saturating_sub(1);
        if *to_owner == 0 {
            self.live.remove(owner);
        }
        self.total = self.total.saturating_sub(1);
    }

    /// The connections carried now to `owner`.
    pub fn live(&self, owner: &O) -> usize {
        self.live.get(owner).copied().unwrap_or(0)
    }
}

/// One admitted connection: releases its slot when dropped, so a forward that ends any way,
/// panics included, gives its slot back.
struct Slot<O>
where O: Owner
{
    admission: Arc<Mutex<Admission<O>>>,
    owner: O,
}

impl<O> Slot<O>
where O: Owner
{
    /// A slot for `owner`, if `admission` admits one.
    fn take(admission: &Arc<Mutex<Admission<O>>>, owner: O) -> Option<Self> {
        let admitted = admission
            .lock()
            .map(|mut admission| admission.admit(owner))
            .unwrap_or(false);
        admitted.then(|| Self {
            admission: Arc::clone(admission),
            owner,
        })
    }
}

impl<O> Drop for Slot<O>
where O: Owner
{
    fn drop(&mut self) {
        if let Ok(mut admission) = self.admission.lock() {
            admission.release(&self.owner);
        }
    }
}

/// The ingress of a relay: where it listens, and what it needs to route and carry connections.
pub struct Ingress<D, F>
where
    D: Directory,
    F: Forward<Owner = D::Owner>,
{
    listen: IpAddr,
    ports: BTreeSet<u16>,
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
        listen: IpAddr,
        ports: BTreeSet<u16>,
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

    /// Bind a listener on every configured port.
    ///
    /// Binding fails as a whole, so a relay never runs with some of its ports silently missing.
    pub async fn bind(&self) -> Result<Vec<TcpListener>, IngressError> {
        let mut listeners = Vec::with_capacity(self.ports.len());
        for port in &self.ports {
            let address = SocketAddr::new(self.listen, *port);
            let listener = TcpListener::bind(address)
                .await
                .map_err(|source| IngressError { address, source })?;
            listeners.push(listener);
        }
        Ok(listeners)
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

    /// Accept on `listener` forever, handing each connection to its own task.
    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _peer)) => {
                        connections.spawn(Arc::clone(&self).carry(stream));
                    }
                    Err(error) => tracing::warn!("dns relay ingress: accept failed: {error}"),
                },
                // Reap finished connections so the set does not grow with every one served.
                Some(_finished) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    }

    /// Route `stream` by its destination and carry it, or close it.
    async fn carry(self: Arc<Self>, stream: TcpStream) {
        let Ok(destination) = stream.local_addr() else {
            return;
        };
        let Some((owner, service)) = self.route(destination).await else {
            return;
        };
        let Some(_slot) = Slot::take(&self.admission, owner) else {
            tracing::debug!(
                "dns relay ingress: {} is at its connection limit",
                owner.label()
            );
            return;
        };
        if let Err(error) = self.forward.forward(stream, owner, &service).await {
            tracing::debug!("dns relay ingress: {}: {error}", owner.label());
        }
    }

    /// `route(destination)` in the module doc.
    async fn route(&self, destination: SocketAddr) -> Option<(D::Owner, ServiceName)> {
        let IpAddr::V6(address) = destination.ip() else {
            return None;
        };
        let owner = self.registry.owner_at(&address).await?;
        let announcement = match self.directory.announcement(owner).await {
            Ok(announcement) => announcement?,
            Err(error) => {
                tracing::debug!("dns relay ingress: {error}");
                return None;
            }
        };
        let service = announcement.service_at(destination.port())?.clone();
        Some((owner, service))
    }
}

#[cfg(test)]
mod test_ingress;
