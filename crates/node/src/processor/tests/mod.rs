use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use rings_core::dht::Chord;
use rings_core::dht::PeerRingAction;
use rings_core::dht::PeerRingRemoteAction;
use rings_core::storage::MemStorage;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::swarm::callback::SwarmEvent;
use rings_rpc::method::Method;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::sync::Mutex as AsyncTestMutex;
use tokio::sync::Notify;

use super::*;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitTransport;
use crate::onion::OnionRouteError;
use crate::online::OnlineNodeDescriptorBody;
use crate::prelude::*;
use crate::provider::Provider;
use crate::tests::native::prepare_processor;

mod common;
mod test_config;
mod test_network;
mod test_onion;
mod test_registry;
