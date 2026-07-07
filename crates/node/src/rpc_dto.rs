//! Conversions from node/core domain values to RPC wire DTOs.

use rings_core::measure::PeerMeasurement;
use rings_core::measure::PeerQualityEvidence;
use rings_rpc::protos::rings_node::BuildOnionRouteResponse;
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use rings_rpc::protos::rings_node::OnionExitPolicyInfo;
use rings_rpc::protos::rings_node::OnionExitServiceInfo;
use rings_rpc::protos::rings_node::OnionExitTransportInfo;
use rings_rpc::protos::rings_node::OnlineNodeDescriptorInfo;
use rings_rpc::protos::rings_node::OnlineNodeTypeInfo;
use rings_rpc::protos::rings_node::PeerMeasurementCountersInfo;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;
use serde::Serialize;
use serde_json::Value;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTransport;
use crate::onion::OnionRoute;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeType;

fn json_value(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(Error::SerdeJsonError)
}

fn online_node_type_info(node_type: OnlineNodeType) -> OnlineNodeTypeInfo {
    match node_type {
        OnlineNodeType::Browser => OnlineNodeTypeInfo::Browser,
        OnlineNodeType::Native => OnlineNodeTypeInfo::Native,
        OnlineNodeType::Ffi => OnlineNodeTypeInfo::Ffi,
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

fn onion_exit_service_info(service: OnionExitService) -> OnionExitServiceInfo {
    OnionExitServiceInfo {
        name: service.name.into(),
        transport: onion_exit_transport_info(service.transport),
    }
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
