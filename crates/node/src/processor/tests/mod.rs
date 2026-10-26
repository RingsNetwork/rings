use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(feature = "dummy")]
use rings_core::dht::Chord;
#[cfg(feature = "dummy")]
use rings_core::dht::PeerRingAction;
#[cfg(feature = "dummy")]
use rings_core::dht::PeerRingRemoteAction;
#[cfg(feature = "dummy")]
use rings_core::message::e2e;
use rings_core::message::e2e::E2eStreamFrame;
use rings_core::storage::MemStorage;
use rings_core::swarm::application::ApplicationData;
use rings_core::swarm::application::ApplicationDelivery;
#[cfg(feature = "dummy")]
use rings_rpc::method::Method;

use super::*;
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
#[cfg(rings_native)]
mod test_close;
mod test_config;
#[cfg(all(feature = "dummy", rings_native))]
mod test_dns_relay;
#[cfg(feature = "dummy")]
mod test_first_edge;
// The gateway tests reach a real public network by design; the `dummy` build has none.
#[cfg(all(rings_native, not(feature = "dummy")))]
mod test_gateway;
#[cfg(all(feature = "dummy", rings_native))]
mod test_http_handshake;
mod test_network;
mod test_onion;
mod test_registry;
#[cfg(feature = "dummy")]
mod test_storage_visibility;
