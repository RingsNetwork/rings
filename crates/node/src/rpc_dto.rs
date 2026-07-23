//! Conversions from node/core domain values to RPC wire DTOs.

#[cfg(all(feature = "browser", target_family = "wasm"))]
use std::str::FromStr;

#[cfg(all(feature = "browser", target_family = "wasm"))]
use rings_core::dht::Did;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use rings_core::ecc::PublicKey;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use rings_core::ecc::VerificationPublicKey;
use rings_core::measure::PeerMeasurement;
use rings_core::measure::PeerQualityEvidence;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use rings_core::message::MessageVerification;
use rings_rpc::protos::rings_node::BuildOnionRouteResponse;
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use rings_rpc::protos::rings_node::OnionExitPolicyInfo;
use rings_rpc::protos::rings_node::OnionExitServiceInfo;
use rings_rpc::protos::rings_node::OnionExitTransportInfo;
use rings_rpc::protos::rings_node::OnlineNodeDescriptorInfo;
use rings_rpc::protos::rings_node::OnlineNodeTypeInfo;
use rings_rpc::protos::rings_node::PeerMeasurementCountersInfo;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitService;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use crate::onion::OnionExitTarget;
use crate::onion::OnionExitTransport;
use crate::onion::OnionRoute;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeType;

fn json_value(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(Error::SerdeJsonError)
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn from_json_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(Error::SerdeJsonError)
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn did_from_string(value: &str) -> Result<Did> {
    Did::from_str(value).map_err(Error::CoreError)
}

fn online_node_type_info(node_type: OnlineNodeType) -> OnlineNodeTypeInfo {
    match node_type {
        OnlineNodeType::Browser => OnlineNodeTypeInfo::Browser,
        OnlineNodeType::Native => OnlineNodeTypeInfo::Native,
        OnlineNodeType::Ffi => OnlineNodeTypeInfo::Ffi,
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn online_node_type_from_info(node_type: OnlineNodeTypeInfo) -> OnlineNodeType {
    match node_type {
        OnlineNodeTypeInfo::Browser => OnlineNodeType::Browser,
        OnlineNodeTypeInfo::Native => OnlineNodeType::Native,
        OnlineNodeTypeInfo::Ffi => OnlineNodeType::Ffi,
    }
}

fn descriptor_timestamp_ms(value: u128) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidData)
}

pub(crate) fn online_node_descriptor_info(
    descriptor: OnlineNodeDescriptor,
) -> Result<OnlineNodeDescriptorInfo> {
    Ok(OnlineNodeDescriptorInfo {
        did: descriptor.did.to_string(),
        public_key: json_value(descriptor.public_key)?,
        session_public_key: json_value(descriptor.session_public_key)?,
        node_type: online_node_type_info(descriptor.node_type),
        network_id: descriptor.network_id,
        storage_redundancy: descriptor.storage_redundancy,
        dht_virtual_nodes: descriptor.dht_virtual_nodes,
        capabilities: descriptor.capabilities,
        endpoint_hint: descriptor.endpoint_hint,
        started_at_ms: descriptor_timestamp_ms(descriptor.started_at_ms)?,
        heartbeat_at_ms: descriptor_timestamp_ms(descriptor.heartbeat_at_ms)?,
        expires_at_ms: descriptor_timestamp_ms(descriptor.expires_at_ms)?,
        version: descriptor.version,
        signature: json_value(descriptor.signature)?,
    })
}

pub(crate) fn online_node_descriptor_infos(
    descriptors: impl IntoIterator<Item = OnlineNodeDescriptor>,
) -> Result<Vec<OnlineNodeDescriptorInfo>> {
    descriptors
        .into_iter()
        .map(online_node_descriptor_info)
        .collect()
}

fn onion_exit_transport_info(transport: OnionExitTransport) -> OnionExitTransportInfo {
    match transport {
        OnionExitTransport::Tcp => OnionExitTransportInfo::Tcp,
        OnionExitTransport::Udp => OnionExitTransportInfo::Udp,
        OnionExitTransport::WebTransport => OnionExitTransportInfo::WebTransport,
        OnionExitTransport::RequestResponse => OnionExitTransportInfo::RequestResponse,
        OnionExitTransport::Https => OnionExitTransportInfo::Https,
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn onion_exit_transport_from_info(transport: OnionExitTransportInfo) -> OnionExitTransport {
    match transport {
        OnionExitTransportInfo::Tcp => OnionExitTransport::Tcp,
        OnionExitTransportInfo::Udp => OnionExitTransport::Udp,
        OnionExitTransportInfo::WebTransport => OnionExitTransport::WebTransport,
        OnionExitTransportInfo::RequestResponse => OnionExitTransport::RequestResponse,
        OnionExitTransportInfo::Https => OnionExitTransport::Https,
    }
}

fn onion_exit_service_info(service: OnionExitService) -> OnionExitServiceInfo {
    OnionExitServiceInfo {
        name: service.name.into(),
        transport: onion_exit_transport_info(service.transport),
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn onion_exit_service_from_info(service: OnionExitServiceInfo) -> Result<OnionExitService> {
    OnionExitService::new(
        service.name.as_str(),
        onion_exit_transport_from_info(service.transport),
    )
}

fn onion_exit_policy_info(policy: OnionExitPolicy) -> OnionExitPolicyInfo {
    OnionExitPolicyInfo {
        allowed_targets: policy
            .allowed_targets
            .into_iter()
            .map(|target| target.authority().to_string())
            .collect(),
        denied_targets: policy
            .denied_targets
            .into_iter()
            .map(|target| target.authority().to_string())
            .collect(),
        max_circuits: policy.max_circuits,
        max_streams_per_circuit: policy.max_streams_per_circuit,
        max_bytes_per_minute: policy.max_bytes_per_minute,
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn onion_exit_policy_from_info(policy: OnionExitPolicyInfo) -> Result<OnionExitPolicy> {
    Ok(OnionExitPolicy {
        allowed_targets: policy
            .allowed_targets
            .into_iter()
            .map(|target| OnionExitTarget::parse(target.as_str()))
            .collect::<Result<Vec<_>>>()?,
        denied_targets: policy
            .denied_targets
            .into_iter()
            .map(|target| OnionExitTarget::parse(target.as_str()))
            .collect::<Result<Vec<_>>>()?,
        max_circuits: policy.max_circuits,
        max_streams_per_circuit: policy.max_streams_per_circuit,
        max_bytes_per_minute: policy.max_bytes_per_minute,
    })
}

pub(crate) fn onion_exit_descriptor_info(
    descriptor: OnionExitDescriptor,
) -> Result<OnionExitDescriptorInfo> {
    Ok(OnionExitDescriptorInfo {
        did: descriptor.did.to_string(),
        public_key: json_value(descriptor.public_key)?,
        session_public_key: json_value(descriptor.session_public_key)?,
        node_type: online_node_type_info(descriptor.node_type),
        network_id: descriptor.network_id,
        services: vec![onion_exit_service_info(descriptor.service)],
        policy: onion_exit_policy_info(descriptor.policy),
        started_at_ms: descriptor_timestamp_ms(descriptor.started_at_ms)?,
        heartbeat_at_ms: descriptor_timestamp_ms(descriptor.heartbeat_at_ms)?,
        expires_at_ms: descriptor_timestamp_ms(descriptor.expires_at_ms)?,
        version: descriptor.version,
        signature: json_value(descriptor.signature)?,
    })
}

pub(crate) fn onion_exit_descriptor_infos(
    descriptors: impl IntoIterator<Item = OnionExitDescriptor>,
) -> Result<Vec<OnionExitDescriptorInfo>> {
    descriptors
        .into_iter()
        .map(onion_exit_descriptor_info)
        .collect()
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) fn online_node_descriptor_from_info(
    descriptor: OnlineNodeDescriptorInfo,
) -> Result<OnlineNodeDescriptor> {
    Ok(OnlineNodeDescriptor {
        did: did_from_string(descriptor.did.as_str())?,
        public_key: from_json_value::<VerificationPublicKey>(descriptor.public_key)?,
        session_public_key: from_json_value::<PublicKey<33>>(descriptor.session_public_key)?,
        node_type: online_node_type_from_info(descriptor.node_type),
        network_id: descriptor.network_id,
        storage_redundancy: descriptor.storage_redundancy,
        dht_virtual_nodes: descriptor.dht_virtual_nodes,
        capabilities: descriptor.capabilities,
        endpoint_hint: descriptor.endpoint_hint,
        started_at_ms: u128::from(descriptor.started_at_ms),
        heartbeat_at_ms: u128::from(descriptor.heartbeat_at_ms),
        expires_at_ms: u128::from(descriptor.expires_at_ms),
        version: descriptor.version,
        signature: from_json_value::<MessageVerification>(descriptor.signature)?,
    })
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) fn online_node_descriptors_from_infos(
    descriptors: impl IntoIterator<Item = OnlineNodeDescriptorInfo>,
) -> Vec<OnlineNodeDescriptor> {
    descriptors
        .into_iter()
        .filter_map(|descriptor| online_node_descriptor_from_info(descriptor).ok())
        .filter(OnlineNodeDescriptor::verify_signature)
        .collect()
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) fn onion_exit_descriptors_from_info(
    descriptor: OnionExitDescriptorInfo,
) -> Result<Vec<OnionExitDescriptor>> {
    let did = did_from_string(descriptor.did.as_str())?;
    let public_key = from_json_value::<VerificationPublicKey>(descriptor.public_key)?;
    let session_public_key = from_json_value::<PublicKey<33>>(descriptor.session_public_key)?;
    let node_type = online_node_type_from_info(descriptor.node_type);
    let policy = onion_exit_policy_from_info(descriptor.policy)?;
    let signature = from_json_value::<MessageVerification>(descriptor.signature)?;
    let services = descriptor
        .services
        .into_iter()
        .map(onion_exit_service_from_info)
        .collect::<Result<Vec<_>>>()?;
    if services.is_empty() {
        return Err(Error::InvalidData);
    }

    Ok(services
        .into_iter()
        .map(|service| OnionExitDescriptor {
            schema_version: crate::onion::ONION_EXIT_DESCRIPTOR_SCHEMA_VERSION,
            did,
            public_key: public_key.clone(),
            session_public_key,
            node_type: node_type.clone(),
            network_id: descriptor.network_id,
            service,
            policy: policy.clone(),
            started_at_ms: u128::from(descriptor.started_at_ms),
            heartbeat_at_ms: u128::from(descriptor.heartbeat_at_ms),
            expires_at_ms: u128::from(descriptor.expires_at_ms),
            version: descriptor.version.clone(),
            signature: signature.clone(),
        })
        .collect())
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) fn onion_exit_descriptors_from_infos(
    descriptors: impl IntoIterator<Item = OnionExitDescriptorInfo>,
) -> Vec<OnionExitDescriptor> {
    descriptors
        .into_iter()
        .filter_map(|descriptor| onion_exit_descriptors_from_info(descriptor).ok())
        .flatten()
        .filter(OnionExitDescriptor::verify_signature)
        .collect()
}

pub(crate) fn onion_route_response(route: OnionRoute) -> Result<BuildOnionRouteResponse> {
    Ok(BuildOnionRouteResponse {
        hops: route.hops().iter().map(|did| did.to_string()).collect(),
        service: route.service().to_string(),
        exit: onion_exit_descriptor_info(route.exit().clone())?,
    })
}

fn peer_measurement_counters_info(evidence: PeerQualityEvidence) -> PeerMeasurementCountersInfo {
    PeerMeasurementCountersInfo {
        connected: evidence.connected,
        disconnected: evidence.disconnected,
        sent: evidence.sent,
        failed_to_send: evidence.failed_to_send,
        received: evidence.received,
        failed_to_receive: evidence.failed_to_receive,
    }
}

pub(crate) fn peer_measurement_info(measurement: PeerMeasurement) -> Result<PeerMeasurementInfo> {
    Ok(PeerMeasurementInfo {
        did: measurement.did.to_string(),
        counters: peer_measurement_counters_info(measurement.evidence),
    })
}

pub(crate) fn optional_peer_measurement_info(
    measurement: Option<PeerMeasurement>,
) -> Result<Option<PeerMeasurementInfo>> {
    measurement.map(peer_measurement_info).transpose()
}

pub(crate) fn peer_measurement_infos(
    measurements: impl IntoIterator<Item = PeerMeasurement>,
) -> Result<Vec<PeerMeasurementInfo>> {
    measurements
        .into_iter()
        .map(peer_measurement_info)
        .collect()
}
