use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(feature = "dummy")]
use rings_core::dht::Chord;
#[cfg(feature = "dummy")]
use rings_core::dht::PeerRingAction;
#[cfg(feature = "dummy")]
use rings_core::dht::PeerRingRemoteAction;
use rings_core::storage::MemStorage;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::swarm::callback::SwarmEvent;
#[cfg(feature = "dummy")]
use rings_rpc::method::Method;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::sync::Mutex as AsyncTestMutex;

use super::*;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionRouteError;
use crate::online::OnlineNodeDescriptorBody;
use crate::prelude::*;
use crate::provider::Provider;
use crate::tests::native::prepare_processor;

mod common;
#[cfg(feature = "dummy")]
mod controlled;
#[cfg(rings_native)]
mod test_bootstrap_probe;
mod test_config;
// The gateway tests reach a real public network by design; the `dummy` build has none.
#[cfg(all(rings_native, not(feature = "dummy")))]
mod test_gateway;
mod test_network;
mod test_onion;
mod test_registry;
