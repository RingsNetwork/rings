//! A Rings node run inside this program from a `rings` configuration file: what the mail gateway
//! stands on, and a mailbox that receives through gateways.
//!
//! ```text
//!   config.yaml ─▶ processor over the node's stores ─▶ application inbox, bootstrap, listener
//!                  (+ the mail descriptor's heartbeat, for a mailbox that receives mail)
//! ```
//!
//! It runs what `rings run` runs for the overlay, and none of its APIs: this program is the
//! node, so it is started instead of `rings run` with that node's configuration, never beside
//! it (one DID, one node).

use std::path::Path;
use std::sync::Arc;

use rings_node::mail::registration::MailOffer;
use rings_node::native::bootstrap::BootstrapSupervisor;
use rings_node::native::bootstrap::BootstrapTargets;
use rings_node::native::config::Config;
use rings_node::native::stores::NodeStores;
use rings_node::prelude::rings_core::lifecycle::StopSource;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use thiserror::Error;
use tokio::task::JoinSet;

/// Why the node could not start.
#[derive(Debug, Error)]
pub enum NodeError {
    /// The configuration, a store, or the processor could not be set up.
    #[error("cannot start the Rings node: {0}")]
    Node(#[from] rings_node::error::Error),
}

/// A started node: its processor, and the tasks serving it until it is dropped.
pub struct EmbeddedNode {
    /// The processor.
    pub processor: Arc<Processor>,
    stop: StopSource,
    /// The application inbox, the bootstrap supervisor and the listener.
    pub tasks: JoinSet<()>,
}

impl Drop for EmbeddedNode {
    /// Ask every task to stop; dropping the task set aborts what is left.
    fn drop(&mut self) {
        self.stop.request_stop();
    }
}

/// Start the node `config` describes, publishing a mail descriptor on `receive_mail`'s schedule
/// when given.
pub async fn start(
    config: &Path,
    receive_mail: Option<MailOffer>,
) -> Result<EmbeddedNode, NodeError> {
    let config = Config::read_fs(config)?;
    let processor_config = ProcessorConfig::try_from(config.clone())?;
    let stores = NodeStores::open(&config.data_storage, &config.measure_storage).await?;
    let builder = ProcessorBuilder::from_config(&processor_config)?
        .storage(stores.data)
        .relay_storage(stores.relay)
        .removal_storage(stores.removals)
        .onion_entry_guard_storage(stores.guards)
        .replay_storage(stores.replay);
    let builder = match receive_mail {
        Some(offer) => builder.receive_mail(offer),
        None => builder,
    };
    let processor = Arc::new(builder.build()?);
    let provider = Provider::from_processor(Arc::clone(&processor));
    let targets = BootstrapTargets::from_config(config.bootstrap, processor.did())?;
    let bootstrap = BootstrapSupervisor::over_processor(targets, Arc::clone(&processor));
    let stop = StopSource::new();
    let application = provider.application_service(
        bootstrap.as_ref().map(BootstrapSupervisor::observer),
        stop.token(),
    )?;
    let mut tasks = JoinSet::new();
    tasks.spawn(application);
    if let Some(supervisor) = bootstrap {
        tasks.spawn(supervisor.run(stop.token()));
    }
    let listener_stop = stop.token();
    tasks.spawn(async move {
        if let Err(error) = provider.listen_with(listener_stop).await {
            tracing::error!("rings node stopped: {error}");
        }
    });
    Ok(EmbeddedNode {
        processor,
        stop,
        tasks,
    })
}
