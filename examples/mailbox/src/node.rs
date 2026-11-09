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
use std::time::Duration;

use rings_node::mail::registration::MailOffer;
use rings_node::native::bootstrap::BootstrapSupervisor;
use rings_node::native::bootstrap::BootstrapTargets;
use rings_node::native::config::Config;
use rings_node::native::stores::NodeStores;
use rings_node::prelude::rings_core::lifecycle::StopSource;
use rings_node::prelude::rings_core::lifecycle::StopToken;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use thiserror::Error;
use tokio::task::JoinSet;

const NODE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the node could not start or close cleanly.
#[derive(Debug, Error)]
pub enum NodeError {
    /// The configuration, a store, or the processor failed.
    #[error("rings node failed: {0}")]
    Node(#[from] rings_node::error::Error),
    /// Closing the node timed out.
    #[error("rings node shutdown did not finish within {NODE_CLEANUP_TIMEOUT:?}")]
    CloseTimeout,
}

/// A started node: its processor, and the tasks serving it until it is closed or dropped.
pub struct EmbeddedNode {
    /// The processor.
    pub processor: Arc<Processor>,
    stop: StopSource,
    /// The application inbox, the bootstrap supervisor and the listener.
    pub tasks: JoinSet<()>,
}

impl EmbeddedNode {
    /// Cooperative stop token shared with the node's run tasks.
    pub fn stop_token(&self) -> StopToken {
        self.stop.token()
    }

    /// Cooperatively stop the node's tasks and invoke [`Processor::close`] (`leave`,
    /// relay-carrier handoff, `swarm.close`, and measurement flush).
    pub async fn close(mut self) -> Result<(), NodeError> {
        self.stop.request_stop();
        if tokio::time::timeout(NODE_CLEANUP_TIMEOUT, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            self.tasks.abort_all();
            while self.tasks.join_next().await.is_some() {}
        }
        tokio::time::timeout(NODE_CLEANUP_TIMEOUT, self.processor.close())
            .await
            .map_err(|_| NodeError::CloseTimeout)??;
        Ok(())
    }
}

impl Drop for EmbeddedNode {
    /// Ask every task to stop; dropping the task set aborts what is left.
    fn drop(&mut self) {
        self.stop.request_stop();
    }
}

/// Wait for `SIGINT` (`Ctrl-C`) or, on Unix, `SIGTERM`.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
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
        .replay_storage(stores.replay)
        .stores_released(stores.released);
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

#[cfg(test)]
mod tests {
    use rings_node::prelude::rings_core::delegation::DelegateeKey;
    use rings_node::prelude::rings_core::ecc::SecretKey;

    use super::*;

    #[tokio::test]
    async fn embedded_node_close_shuts_down_processor_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let delegatee_key =
            DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegatee key");
        let key_path = dir.path().join("delegatee.json");
        std::fs::write(&key_path, delegatee_key.dump().expect("dump delegatee key"))
            .expect("write delegatee key");
        let mut cfg = Config::new(&key_path);
        cfg.data_storage.path = dir.path().join("data").to_string_lossy().into_owned();
        cfg.measure_storage.path = dir.path().join("measure").to_string_lossy().into_owned();
        let cfg_path = dir.path().join("config.yaml");
        cfg.write_fs(cfg_path.to_str().expect("utf8 path"))
            .expect("write config");

        let node = start(&cfg_path, None).await.expect("start embedded node");
        let processor = Arc::clone(&node.processor);
        node.close().await.expect("close embedded node");

        let peer = SecretKey::random().address().into();
        assert!(matches!(
            processor.swarm.create_offer(peer).await,
            Err(rings_node::prelude::rings_core::error::Error::SwarmClosed)
        ));
    }
}
