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
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use rings_rpc::protos::rings_node::OnionExitPolicyInfo;
use rings_rpc::protos::rings_node::OnlineNodeDescriptorInfo;
use rings_rpc::protos::rings_node::OnlineNodeTypeInfo;
use rings_rpc::protos::rings_node::PeerCreditInfo;
use rings_rpc::protos::rings_node::PeerMeasurementCountersInfo;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;
use rings_rpc::protos::rings_node::PeerReliabilityInfo;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitPolicy;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use crate::onion::OnionExitTarget;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use crate::onion::OnionServiceName;
#[cfg(all(feature = "browser", target_family = "wasm"))]
use crate::online::OnlineNodeCapabilities;
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
        delegatee_public_key: json_value(descriptor.delegatee_public_key)?,
        node_type: online_node_type_info(descriptor.node_type),
        network_id: descriptor.network_id,
        storage_redundancy: descriptor.storage_redundancy,
        dht_virtual_nodes: descriptor.dht_virtual_nodes,
        capabilities: json_value(descriptor.capabilities)?,
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
        delegatee_public_key: json_value(descriptor.delegatee_public_key)?,
        process_epoch: json_value(descriptor.process_epoch)?,
        node_type: online_node_type_info(descriptor.node_type),
        network_id: descriptor.network_id,
        service: descriptor.service.into(),
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
        delegatee_public_key: from_json_value::<PublicKey<33>>(descriptor.delegatee_public_key)?,
        node_type: online_node_type_from_info(descriptor.node_type),
        network_id: descriptor.network_id,
        storage_redundancy: descriptor.storage_redundancy,
        dht_virtual_nodes: descriptor.dht_virtual_nodes,
        capabilities: from_json_value::<OnlineNodeCapabilities>(descriptor.capabilities)?,
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
    network_id: u32,
) -> Vec<OnlineNodeDescriptor> {
    descriptors
        .into_iter()
        .filter_map(|descriptor| online_node_descriptor_from_info(descriptor).ok())
        .filter(|descriptor| descriptor.verify_signature(network_id))
        .collect()
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
/// Decode the singular service descriptor returned by a remote directory RPC.
pub(crate) fn onion_exit_descriptor_from_info(
    descriptor: OnionExitDescriptorInfo,
) -> Result<OnionExitDescriptor> {
    let did = did_from_string(descriptor.did.as_str())?;
    let public_key = from_json_value::<VerificationPublicKey>(descriptor.public_key)?;
    let delegatee_public_key = from_json_value::<PublicKey<33>>(descriptor.delegatee_public_key)?;
    let process_epoch =
        from_json_value::<crate::onion::OnionProcessEpoch>(descriptor.process_epoch)?;
    let node_type = online_node_type_from_info(descriptor.node_type);
    let policy = onion_exit_policy_from_info(descriptor.policy)?;
    let signature = from_json_value::<MessageVerification>(descriptor.signature)?;
    let service = OnionServiceName::parse(descriptor.service)?;

    Ok(OnionExitDescriptor {
        did,
        public_key,
        delegatee_public_key,
        process_epoch,
        node_type,
        network_id: descriptor.network_id,
        service,
        policy,
        started_at_ms: u128::from(descriptor.started_at_ms),
        heartbeat_at_ms: u128::from(descriptor.heartbeat_at_ms),
        expires_at_ms: u128::from(descriptor.expires_at_ms),
        version: descriptor.version,
        signature,
    })
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) fn onion_exit_descriptors_from_infos(
    descriptors: impl IntoIterator<Item = OnionExitDescriptorInfo>,
    network_id: u32,
) -> Vec<OnionExitDescriptor> {
    descriptors
        .into_iter()
        .filter_map(|descriptor| onion_exit_descriptor_from_info(descriptor).ok())
        .filter(|descriptor| descriptor.verify_signature(network_id))
        .collect()
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
    // RPC message fields retain protobuf presence; each retained peer has credit.
    let credit = measurement.credit;
    let credit = Some(PeerCreditInfo {
        bytes_sent_to_peer: credit.bytes_sent_to_peer(),
        bytes_received_from_peer: credit.bytes_received_from_peer(),
        last_seen_seconds: credit.last_seen().as_secs(),
        score: measurement.credit_score.as_f64(),
    });
    let reliability = match measurement.quality {
        rings_core::measure::PeerQuality::Healthy => PeerReliabilityInfo::Healthy,
        rings_core::measure::PeerQuality::Unknown => PeerReliabilityInfo::Unknown,
        rings_core::measure::PeerQuality::Degraded => PeerReliabilityInfo::Degraded,
    };
    Ok(PeerMeasurementInfo {
        did: measurement.did.to_string(),
        counters: peer_measurement_counters_info(measurement.evidence),
        credit,
        reliability,
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
