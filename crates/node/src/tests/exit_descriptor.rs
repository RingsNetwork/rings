//! The signed onion-exit registration fixture of the onion and processor tests.
//!
//! A fixture borrows the key that signs it; every other body field is plain data, so a test
//! varies one field with struct-update syntax over [`ExitDescriptorFixture::offline`] or
//! [`ExitDescriptorFixture::live`] and then [signs](ExitDescriptorFixture::sign) it.

use rings_core::delegation::DelegateeKey;
use rings_core::message::MessageSigner;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
#[cfg(feature = "node")]
use crate::processor::Processor;
#[cfg(feature = "node")]
use crate::registration::default_online_node_type;
use crate::tests::TEST_NETWORK_ID;

/// How long a [`ExitDescriptorFixture::live`] registration stays live after its heartbeat.
#[cfg(feature = "node")]
const LIVE_REGISTRATION_TTL_MS: u128 = 90_000;

/// An unsigned exit registration and the key that signs it.
pub(crate) struct ExitDescriptorFixture<'k> {
    /// The signing key; it fixes the DID and both public keys of the body.
    pub(crate) key: &'k DelegateeKey,
    /// The overlay the descriptor is published for and signed on.
    pub(crate) network_id: u32,
    /// The process epoch the exit registers.
    pub(crate) process_epoch: OnionProcessEpoch,
    /// The node type the exit advertises.
    pub(crate) node_type: OnlineNodeType,
    /// The service the exit offers.
    pub(crate) service: OnionServiceName,
    /// The exit's target policy.
    pub(crate) policy: OnionExitPolicy,
    /// The registration's start, in epoch milliseconds.
    pub(crate) started_at_ms: u128,
    /// The registration's latest heartbeat, in epoch milliseconds.
    pub(crate) heartbeat_at_ms: u128,
    /// The registration's expiry, in epoch milliseconds.
    pub(crate) expires_at_ms: u128,
    /// The build version the exit advertises.
    pub(crate) version: String,
}

impl<'k> ExitDescriptorFixture<'k> {
    /// A `tcp` registration of `key` on [`TEST_NETWORK_ID`] at `process_epoch`, started at
    /// 1 ms and live from `heartbeat_at_ms` until `expires_at_ms`: a native node allowing only
    /// `example.com:443`, for 16 sessions and 1 KiB a minute.
    pub(crate) fn offline(
        key: &'k DelegateeKey,
        process_epoch: OnionProcessEpoch,
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
    ) -> Result<Self> {
        Ok(Self {
            key,
            network_id: TEST_NETWORK_ID,
            process_epoch,
            node_type: OnlineNodeType::Native,
            service: OnionServiceName::tcp(),
            policy: OnionExitPolicy {
                allowed_targets: vec![OnionExitTarget::parse("example.com:443")?],
                denied_targets: vec![],
                max_sessions: 16,
                max_bytes_per_minute: 1024,
            },
            started_at_ms: 1,
            heartbeat_at_ms,
            expires_at_ms,
            version: "test".to_string(),
        })
    }

    /// `processor`'s registration of `service` under `policy` as the running node publishes
    /// it at `now_ms`: its network, current process epoch, default node type and build
    /// version, live for [`LIVE_REGISTRATION_TTL_MS`].
    #[cfg(feature = "node")]
    pub(crate) fn live(
        processor: &'k Processor,
        service: OnionServiceName,
        policy: OnionExitPolicy,
        now_ms: u128,
    ) -> Self {
        Self {
            key: processor.delegatee_key(),
            network_id: processor.swarm.network_id(),
            process_epoch: processor.onion_process_epoch().get(),
            node_type: default_online_node_type(),
            service,
            policy,
            started_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            expires_at_ms: now_ms + LIVE_REGISTRATION_TTL_MS,
            version: crate::util::build_version(),
        }
    }

    /// The descriptor of this registration, signed by its key on its network.
    pub(crate) fn sign(self) -> Result<OnionExitDescriptor> {
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: self.key.delegator_did(),
                public_key: self
                    .key
                    .delegation()
                    .delegator_verification_pubkey()
                    .map_err(Error::CoreError)?,
                delegatee_public_key: self.key.delegatee_public_key(),
                process_epoch: self.process_epoch,
                node_type: self.node_type,
                network_id: self.network_id,
                service: self.service,
                policy: self.policy,
                started_at_ms: self.started_at_ms,
                heartbeat_at_ms: self.heartbeat_at_ms,
                expires_at_ms: self.expires_at_ms,
                version: self.version,
            },
            MessageSigner::new(self.key, self.network_id),
        )
        .map_err(Error::CoreError)
    }
}
