//! Wire fixtures and laws of the message model.
use bytes::Bytes;

use super::*;
use crate::chunk::ChunkMeta;
use crate::delegation::DelegateeKey;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::ecc::SecretKey;
use crate::message::MessageSigner;
use crate::message::ProbeCompletion;
use crate::message::ProvisionalEpoch;
use crate::message::ProvisionalServiceClaim;
use crate::message::ProvisionalServiceReceipt;
use crate::message::Transaction;

pub(super) struct MessageFixture {
    did: Did,
    public_key: crate::ecc::PublicKey<33>,
    entry: Entry,
    provider: DelegateeKey,
    beneficiary: DelegateeKey,
}

/// The instant the fixture's sessions were delegated.
const FIXTURE_DELEGATED_AT_MS: u128 = 1_700_000_000_000;
/// The lifetime of the fixture's sessions.
const FIXTURE_SESSION_TTL_MS: u64 = 86_400_000;

/// The fixture session of the account key `account` over the delegatee key `delegatee`, both
/// the scalar of their 64-digit hex form.
fn fixture_session(account: &str, delegatee: &str) -> Result<DelegateeKey> {
    DelegateeKey::from_test_keys(
        &SecretKey::try_from(account)?,
        SecretKey::try_from(delegatee)?,
        FIXTURE_DELEGATED_AT_MS,
        FIXTURE_SESSION_TTL_MS,
    )
}

impl MessageFixture {
    /// The fixture: fixed keys, so every body but the parts a signature stamps with the clock
    /// is the same in every run; under a simulation runtime's clock, all of it is (the wire
    /// golden vectors, #1107).
    pub(super) fn new() -> Result<Self> {
        let provider = fixture_session(
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000002",
        )?;
        let beneficiary = fixture_session(
            "0000000000000000000000000000000000000000000000000000000000000003",
            "0000000000000000000000000000000000000000000000000000000000000004",
        )?;
        let did = provider.delegator_did();
        Ok(Self {
            did,
            public_key: provider.delegatee_public_key(),
            entry: Entry::new(did, Vec::new(), EntryKind::Data),
            provider,
            beneficiary,
        })
    }
}

pub(super) trait SampleMessageBody: Sized {
    fn sample(fixture: &MessageFixture) -> Result<Self>;
}

pub(super) fn sample_body<T: SampleMessageBody>(fixture: &MessageFixture) -> Result<T> {
    T::sample(fixture)
}

impl<T> SampleMessageBody for Box<T>
where T: SampleMessageBody
{
    fn sample(fixture: &MessageFixture) -> Result<Self> {
        T::sample(fixture).map(Box::new)
    }
}

macro_rules! sample_message_body {
    ($body:ty, |$fixture:ident| $sample:expr) => {
        impl SampleMessageBody for $body {
            fn sample($fixture: &MessageFixture) -> Result<Self> {
                Ok($sample)
            }
        }
    };
}

sample_message_body!(ConnectNodeSend, |_fixture| ConnectNodeSend {
    sdp: String::new(),
    dht_protocol_mode: DhtProtocolMode::new(1, 1, 1),
    relay_custody: RelayCustody::Holds,
});
sample_message_body!(ConnectNodeReport, |_fixture| ConnectNodeReport {
    sdp: String::new(),
    dht_protocol_mode: DhtProtocolMode::new(1, 1, 1),
    relay_custody: RelayCustody::Holds,
});
sample_message_body!(FindSuccessorSend, |fixture| FindSuccessorSend {
    did: fixture.did,
    strict: false,
    then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
});
sample_message_body!(FindSuccessorReport, |fixture| FindSuccessorReport {
    did: fixture.did,
    handler: FindSuccessorReportHandler::None,
});
sample_message_body!(NotifyPredecessorSend, |fixture| NotifyPredecessorSend {
    did: fixture.did,
});
sample_message_body!(ProbeRequest, |fixture| ProbeRequest {
    epoch: ProvisionalEpoch { slot: 1 },
    nonce: [u8::from(fixture.did != Did::from(0_u32)); 32],
});
impl SampleMessageBody for ProbeOffer {
    fn sample(fixture: &MessageFixture) -> Result<Self> {
        sample_probe_offer(fixture).map(|(offer, _)| offer)
    }
}
impl SampleMessageBody for ProbeAcknowledgement {
    fn sample(fixture: &MessageFixture) -> Result<Self> {
        sample_probe_offer(fixture).map(|(_, receipt)| ProbeAcknowledgement { receipt })
    }
}
sample_message_body!(SearchEntry, |fixture| SearchEntry {
    resource: fixture.did,
    placement: fixture.did,
    redundancy: 1,
});
sample_message_body!(FoundEntry, |fixture| FoundEntry {
    data: vec![fixture.entry.clone()],
    misses: vec![PlacementMiss::new(fixture.did, fixture.did)],
    resource: fixture.did,
    redundancy: 1,
});
sample_message_body!(PlacedEntryOperation, |fixture| PlacedEntryOperation {
    placement: fixture.did,
    op: EntryOperation::Overwrite(fixture.entry.clone()),
    report: true,
});
sample_message_body!(SyncEntriesWithSuccessor, |fixture| {
    SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::physical_owner(fixture.did),
        data: vec![PlacedEntry::new(fixture.did, fixture.entry.clone())],
    }
});
sample_message_body!(SyncEntriesWithSuccessorReport, |fixture| {
    SyncEntriesWithSuccessorReport::new(
        StorageSyncPurpose::AdditiveRepair,
        StorageSyncDestination::physical_owner(fixture.did),
        fixture.did,
        vec![SyncedEntryAck::new(fixture.did, fixture.entry.clone())],
    )
});
sample_message_body!(CustomMessage, |fixture| CustomMessage(
    fixture.did.to_string().into_bytes()
));
sample_message_body!(E2eHandshakeRequest, |fixture| E2eHandshakeRequest {
    requester_public_key: fixture.public_key,
});
sample_message_body!(E2eHandshakeResponse, |fixture| E2eHandshakeResponse {
    responder_public_key: fixture.public_key,
});
sample_message_body!(E2eStreamFrame, |fixture| E2eStreamFrame {
    stream_id: uuid::Uuid::nil(),
    sender_public_key: fixture.public_key,
    sequence: 0,
    is_final: true,
    ciphertext: vec![(fixture.public_key, fixture.public_key)],
});
sample_message_body!(QueryForTopoInfoSend, |fixture| QueryForTopoInfoSend {
    did: fixture.did,
    then: QueryFor::Stabilization,
    request_id: uuid::Uuid::nil(),
});
sample_message_body!(QueryForTopoInfoReport, |fixture| QueryForTopoInfoReport {
    info: TopoInfo {
        successors: vec![fixture.did],
        predecessor: Some(fixture.did),
    },
    then: QueryFor::Stabilization,
    request_id: uuid::Uuid::nil(),
});
sample_message_body!(AnnounceRelayHolder, |fixture| AnnounceRelayHolder {
    previous: vec![PreviousRelayHolder {
        holder: fixture.did,
        run_start: fixture.did,
    }],
});
sample_message_body!(FetchRelayInbox, |_fixture| FetchRelayInbox);
sample_message_body!(RelayInboxReport, |_fixture| RelayInboxReport {
    inbox: None
});
sample_message_body!(RelayCarrierRefused, |fixture| RelayCarrierRefused {
    hold: RelayHoldDigest::of(&EntryOperation::Extend(fixture.entry.clone()))?,
});
sample_message_body!(ReplicateRelayHold, |fixture| ReplicateRelayHold {
    digest: RelayHoldDigest::of(&EntryOperation::Extend(fixture.entry.clone()))?,
    hold: Some(EntryOperation::Extend(fixture.entry.clone())),
});
sample_message_body!(RelayCarrierDigests, |fixture| RelayCarrierDigests {
    carriers: vec![RelayCarrierDigest::of(fixture.did, &fixture.entry)?],
});
sample_message_body!(RelayCarriersWanted, |fixture| RelayCarriersWanted {
    keys: vec![fixture.did],
});
sample_message_body!(DeclareRelayCustody, |_fixture| DeclareRelayCustody {
    custody: crate::dht::RelayCustody::Holds,
});
sample_message_body!(RelayCarrierHeld, |fixture| RelayCarrierHeld {
    hold: RelayHoldDigest::of(&EntryOperation::Extend(fixture.entry.clone()))?,
});
sample_message_body!(EntryStoreReport, |fixture| EntryStoreReport {
    operation: OperationDigest::of(&EntryOperation::Extend(fixture.entry.clone()))?,
    placement: fixture.did,
    outcome: StoreOutcome::Stored,
});
sample_message_body!(FindRelayHolder, |fixture| FindRelayHolder {
    key: fixture.did
});
sample_message_body!(RelayHolderFound, |fixture| RelayHolderFound {
    key: fixture.did,
    holder: Some(fixture.did),
    horizon: None,
});
sample_message_body!(Chunk, |fixture| Chunk {
    chunk: [0, 1],
    data: Bytes::from(fixture.did.to_string()),
    meta: ChunkMeta::default(),
});

fn sample_probe_offer(fixture: &MessageFixture) -> Result<(ProbeOffer, ProvisionalServiceReceipt)> {
    let network_id = 1;
    let tx_id = uuid::Uuid::nil();
    let request_body = ProbeRequest {
        epoch: ProvisionalEpoch { slot: 1 },
        nonce: [7; 32],
    };
    let request = Transaction::new(
        fixture.provider.delegator_did(),
        tx_id,
        0,
        None,
        Message::ProbeRequest(request_body),
        MessageSigner::new(&fixture.beneficiary, network_id),
    )?;
    let request_digest = request.digest()?.into_bytes();
    let completion = Transaction::new(
        fixture.beneficiary.delegator_did(),
        tx_id,
        0,
        None,
        ProbeCompletion {
            request_digest,
            nonce: request_body.nonce,
        },
        MessageSigner::new(&fixture.provider, network_id),
    )?;
    let claim = ProvisionalServiceClaim::probe(
        network_id,
        fixture.provider.delegator_did(),
        fixture.beneficiary.delegator_did(),
        request_body.epoch,
        request_body.nonce,
        request_digest,
        completion.digest()?.into_bytes(),
    );
    let provider_attestation =
        claim.sign_provider(MessageSigner::new(&fixture.provider, network_id))?;
    let beneficiary_attestation =
        claim.sign_beneficiary(MessageSigner::new(&fixture.beneficiary, network_id))?;
    let receipt = ProvisionalServiceReceipt::new(
        claim.clone(),
        provider_attestation.clone(),
        beneficiary_attestation,
    )?;
    Ok((
        ProbeOffer {
            request,
            completion,
            claim,
            provider_attestation,
        },
        receipt,
    ))
}

fn random_did() -> Did {
    SecretKey::random().address().into()
}

#[test]
fn test_find_successor_send_predicate_names_local_report_rule() {
    let local = random_did();
    let remote = random_did();
    let then = FindSuccessorThen::Report(FindSuccessorReportHandler::None);

    let strict_local = FindSuccessorSend {
        did: local,
        strict: true,
        then: then.clone(),
    };
    assert!(strict_local.accepts_local_successor(local));

    let strict_remote = FindSuccessorSend {
        did: remote,
        strict: true,
        then: then.clone(),
    };
    assert!(!strict_remote.accepts_local_successor(local));

    let relaxed_remote = FindSuccessorSend {
        did: remote,
        strict: false,
        then,
    };
    assert!(relaxed_remote.accepts_local_successor(local));
}

#[test]
fn test_find_successor_report_predicate_names_remote_successor() {
    let local = random_did();
    let remote = random_did();

    let local_report = FindSuccessorReport {
        did: local,
        handler: FindSuccessorReportHandler::Connect,
    };
    assert!(!local_report.reports_remote_successor(local));

    let remote_report = FindSuccessorReport {
        did: remote,
        handler: FindSuccessorReportHandler::Connect,
    };
    assert!(remote_report.reports_remote_successor(local));
}

/// Proves that the finger report handler preserves both components of its
/// correlation token across wire serialization.
///
/// The decoded variant must remain `FixFingerTable`, and its lower slot and
/// UUID must equal the original request exactly; otherwise a report could
/// escape the convergence state machine's ownership checks.
#[test]
fn test_finger_fix_report_handler_round_trips_its_correlation_token() -> Result<()> {
    let request = crate::dht::FingerFixRequest::new(17, uuid::Uuid::from_u128(42))
        .ok_or_else(|| crate::error::Error::InvalidMessage("invalid test request".to_owned()))?;
    let handler = FindSuccessorReportHandler::FixFingerTable { request };
    let wire = rings_codec::serialize(&handler).map_err(crate::error::Error::CodecSerialize)?;
    let decoded: FindSuccessorReportHandler =
        rings_codec::deserialize(&wire).map_err(crate::error::Error::CodecDeserialize)?;

    match decoded {
        FindSuccessorReportHandler::FixFingerTable { request: decoded } => {
            assert_eq!(decoded, request);
        }
        _ => {
            return Err(crate::error::Error::InvalidMessage(
                "finger correlation token changed variant on wire round trip".to_owned(),
            ));
        }
    }
    Ok(())
}

#[test]
fn test_topo_info_query_predicate_names_target_node() {
    let local = random_did();
    let remote = random_did();

    assert!(QueryForTopoInfoSend::new_for_sync(local).targets(local));
    assert!(!QueryForTopoInfoSend::new_for_sync(remote).targets(local));
}

#[test]
fn test_message_metadata_wire_indices_follow_enum_declaration_order() -> Result<()> {
    let messages = Message::test_variants()?;
    assert_eq!(messages.len(), MessageKind::WIRE_ORDER.len());
    for (position, ((wire_index, kind), message)) in
        MessageKind::WIRE_ORDER.iter().zip(messages).enumerate()
    {
        assert_eq!(usize::try_from(*wire_index), Ok(position));
        assert_eq!(message.kind(), *kind);
        assert_eq!(
            kind.requires_storage_route(),
            message.storage_sync_destination().is_some()
        );
    }
    Ok(())
}

/// The embedded [`DhtProtocolMode`] encodes exactly as the three fields it replaced did:
/// the codec writes a struct as its fields in declaration order with no framing, so
/// nesting changes nothing on the wire.
#[test]
fn test_connect_node_advertisement_encodes_as_its_flattened_fields() -> Result<()> {
    #[derive(Serialize)]
    struct Flattened {
        sdp: String,
        network_id: u32,
        storage_redundancy: u16,
        dht_virtual_nodes: u16,
        relay_custody: RelayCustody,
    }

    let flattened = Flattened {
        sdp: "v=0".to_string(),
        network_id: 7,
        storage_redundancy: 3,
        dht_virtual_nodes: 160,
        relay_custody: RelayCustody::HoldsNone,
    };
    let mode = DhtProtocolMode::new(7, 3, 160);
    let offer = ConnectNodeSend {
        sdp: "v=0".to_string(),
        dht_protocol_mode: mode,
        relay_custody: RelayCustody::HoldsNone,
    };
    let answer = ConnectNodeReport {
        sdp: "v=0".to_string(),
        dht_protocol_mode: mode,
        relay_custody: RelayCustody::HoldsNone,
    };

    let expected = rings_codec::serialize(&flattened).map_err(Error::CodecSerialize)?;
    assert_eq!(
        rings_codec::serialize(&offer).map_err(Error::CodecSerialize)?,
        expected
    );
    assert_eq!(
        rings_codec::serialize(&answer).map_err(Error::CodecSerialize)?,
        expected
    );
    Ok(())
}
