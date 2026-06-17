#![warn(missing_docs)]
//! The imperative shell — [`Interpreter`], the single side-effecting boundary that
//! executes the [`Effect`]s a pure `step` produced.

use std::sync::Arc;

use rings_core::dht::Did;

use super::compute::ComputeServices;
use super::envelope::Envelope;
use super::protocol::Effect;
use super::protocol::Event;
use super::protocol::Inbound;
use crate::error::Result;
use crate::processor::Processor;

/// Executes [`Effect`]s. The single side-effecting boundary of the extension layer.
///
/// It holds the platform transport engine that owns live relay endpoints: OS sockets
/// natively ([`engine::TransportSessions`](crate::backend::transport::engine)), or
/// WebTransport sessions in the browser
/// (`wt::WtSessions`, browser). `Write`/`Shutdown`/`Close`
/// dispatch to it on both targets; only the *open* effect differs (`Connect`/`Listen`
/// natively, `WtConnect` in the browser).
#[derive(Clone)]
pub struct Interpreter {
    processor: Arc<Processor>,
    compute: ComputeServices,
    #[cfg(feature = "node")]
    transport: Arc<crate::backend::transport::engine::TransportSessions>,
    #[cfg(feature = "browser")]
    transport: Arc<crate::backend::transport::wt::WtSessions>,
}

impl Interpreter {
    /// Build an interpreter over a processor and the shared native transport engine.
    #[cfg(feature = "node")]
    pub fn new(
        processor: Arc<Processor>,
        transport: Arc<crate::backend::transport::engine::TransportSessions>,
        compute: ComputeServices,
    ) -> Self {
        Self {
            processor,
            compute,
            transport,
        }
    }

    /// Build an interpreter over a processor and the shared browser WebTransport engine.
    #[cfg(feature = "browser")]
    pub fn new(
        processor: Arc<Processor>,
        transport: Arc<crate::backend::transport::wt::WtSessions>,
        compute: ComputeServices,
    ) -> Self {
        Self {
            processor,
            compute,
            transport,
        }
    }

    /// This node's DID (read-only fact handed to `step` via [`Ctx`](super::Ctx)).
    pub fn did(&self) -> Did {
        self.processor.did()
    }

    /// Run effects in order, returning any locally re-injected messages (the event
    /// trace fed back into the router). `run : [Effect] → IO [Inbound]`.
    ///
    /// `Send` is terminal (goes to a peer) and re-injects nothing; transport effects
    /// drive native sockets (no-ops on browser). Local reads re-enter the router from
    /// the engine's own tasks rather than via this return value.
    pub async fn run(&self, effects: Vec<Effect>) -> Result<Vec<Inbound>> {
        let mut reinjected = Vec::new();
        for effect in effects {
            match effect {
                Effect::Compute { namespace, input } => {
                    // Impure region: run the namespace's registered job, then feed its
                    // result back as a self-event so the pure `step` can act on it.
                    match self.compute.get(namespace.as_str()) {
                        Some(job) => {
                            let output = job(input).await?;
                            reinjected.push(Inbound {
                                namespace: namespace.clone(),
                                event: Event {
                                    from: self.did(),
                                    payload: output,
                                },
                            });
                        }
                        None => tracing::warn!(
                            "no compute service registered for namespace {:?}, dropping",
                            namespace
                        ),
                    }
                }
                Effect::Send {
                    to,
                    namespace,
                    payload,
                } => {
                    let envelope = Envelope::new(namespace, payload);
                    self.processor.send_envelope(to, &envelope).await?;
                }
                Effect::Connect { key, addr, kind } => {
                    #[cfg(feature = "node")]
                    self.transport
                        .clone()
                        .connect(self.processor.clone(), key, addr, kind)
                        .await;
                    #[cfg(feature = "browser")]
                    {
                        let _ = (key, addr, kind);
                        tracing::warn!("transport Connect (socket) is unsupported on browser");
                    }
                }
                Effect::WtConnect { key, url, kind } => {
                    #[cfg(feature = "browser")]
                    self.transport
                        .clone()
                        .connect(self.processor.clone(), key, url, kind)
                        .await;
                    #[cfg(not(feature = "browser"))]
                    {
                        let _ = (key, url, kind);
                        tracing::warn!("WtConnect (WebTransport) is only supported on browser");
                    }
                }
                // Write/Shutdown/Close address an existing session by its full key; the
                // platform engine handles them identically on both targets, and a key
                // whose `peer` did not open the session simply misses (owner rejection).
                Effect::Write { key, bytes } => {
                    self.transport.write(&key, bytes).await;
                }
                Effect::Shutdown { key } => {
                    self.transport.shutdown(&key).await;
                }
                Effect::Close { key } => {
                    self.transport.close(&key);
                }
                Effect::Listen {
                    local_addr,
                    peer,
                    service,
                    namespace,
                    kind,
                } => {
                    #[cfg(feature = "node")]
                    self.transport
                        .clone()
                        .listen(
                            self.processor.clone(),
                            local_addr,
                            peer,
                            service,
                            namespace,
                            kind,
                        )
                        .await;
                    #[cfg(feature = "browser")]
                    {
                        let _ = (local_addr, peer, service, namespace, kind);
                        tracing::warn!("transport Listen is unsupported on browser");
                    }
                }
            }
        }
        Ok(reinjected)
    }
}
