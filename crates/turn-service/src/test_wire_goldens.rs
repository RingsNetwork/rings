//! Wire golden vectors for `rings-turn-service` (`TurnServiceDescriptor` and `turn.ticket`
//! fixed-width frames), pinned against [`WIRE_VERSION`] outside `rings-core` and `rings-node`.

use rings_core::consts::DEFAULT_DELEGATION_TTL_MS;
use rings_core::delegation::Account;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::keccak256;
use rings_core::ecc::SecretKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::MessageSigner;
use rings_core::message::WIRE_VERSION;
use rings_node::descriptor::RegistryElement;
use rings_node::descriptor::SignedDescriptor;
use rings_node::descriptor::SignedDescriptorBody;
use rings_node::onion::OnionProcessEpoch;
use rings_turn::SignedTurnSessionTicket;
use serde_reflection::Samples;
use serde_reflection::Tracer;
use serde_reflection::TracerConfig;

use crate::decode_turn_ticket_outcome;
use crate::decode_turn_ticket_request;
use crate::encode_turn_ticket_outcome;
use crate::encode_turn_ticket_request;
use crate::TurnServiceDescriptor;
use crate::TurnServiceDescriptorBody;
use crate::TurnTicketOutcome;
use crate::TurnTicketRejection;
use crate::TurnTicketRequest;
use crate::TURN_SERVICES_TOPIC;
use crate::TURN_TICKET_INVOKE_BACKEND;
use crate::TURN_TICKET_INVOKE_SHAPE;

const PINNED: (u16, &str) = (
    6,
    "6a86b00cd6771cdf4ff2a4f0afc311bb3e82f426aeeae853029d76849d5d6b3f",
);

const GOLDEN_EPOCH_MS: u128 = 1_700_000_000_000;
const GOLDEN_NETWORK_ID: u32 = 7;
const ALICE_ACCOUNT_SEED: u8 = 0x11;
const ALICE_DELEGATEE_SEED: u8 = 0x12;
const BOB_SERVER_SEED: u8 = 0x21;

fn fixture_secret(seed: u8) -> SecretKey {
    SecretKey::try_from(format!("{seed:02x}").repeat(32).as_str()).unwrap()
}

fn to_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct GoldenActor {
    did: Did,
    public_key: VerificationPublicKey,
    delegatee_key: DelegateeKey,
}

impl GoldenActor {
    fn new(account_seed: u8, delegatee_seed: u8) -> Self {
        let account_key = fixture_secret(account_seed);
        let delegatee_secret_key = fixture_secret(delegatee_seed);
        let did = Did::from(account_key.address());
        let public_key = VerificationPublicKey::Secp256k1(account_key.pubkey());
        let delegatee_did = Did::from(delegatee_secret_key.address());
        let delegator = Account::Secp256k1(did);
        let ttl_ms = DEFAULT_DELEGATION_TTL_MS;
        let ts_ms = GOLDEN_EPOCH_MS;
        let proof =
            format!("rings-delegation-v1\nsecp256k1\n{did}\n{delegatee_did}\n{ts_ms}\n{ttl_ms}");
        let delegator_signature = account_key.sign(&proof).unwrap().to_vec();
        let delegatee_key: DelegateeKey = serde_json::from_value(serde_json::json!({
            "delegation": {
                "delegatee_did": delegatee_did,
                "delegator": delegator,
                "ttl_ms": ttl_ms,
                "ts_ms": ts_ms,
                "delegator_signature": delegator_signature,
            },
            "delegatee_secret_key": delegatee_secret_key,
        }))
        .unwrap();
        delegatee_key
            .delegation()
            .verify_delegator_authorization_at(GOLDEN_EPOCH_MS)
            .unwrap();
        Self {
            did,
            public_key,
            delegatee_key,
        }
    }

    fn signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, GOLDEN_NETWORK_ID)
    }
}

#[test]
fn turn_service_wire_goldens_match_pinned_version() {
    let alice = GoldenActor::new(ALICE_ACCOUNT_SEED, ALICE_DELEGATEE_SEED);
    let server_key = fixture_secret(BOB_SERVER_SEED);
    let server_pubkey = server_key.pubkey();
    let ephemeral_pk = alice.delegatee_key.delegatee_public_key();
    let process_epoch = OnionProcessEpoch::new([0x33; 16]);

    let mut tracer = Tracer::new(TracerConfig::default());
    let mut samples = Samples::new();

    let body = TurnServiceDescriptorBody {
        did: alice.did,
        public_key: alice.public_key.clone(),
        delegatee_public_key: ephemeral_pk,
        process_epoch,
        network_id: GOLDEN_NETWORK_ID,
        turn_url: "turn://turn.rings.rs:3478".to_string(),
        server_pubkey,
        fresh_for_ms: 90_000,
        expires_at_ms: GOLDEN_EPOCH_MS + 90_000,
    };
    let signing_data = body.body_signing_data().unwrap();
    let signature = alice
        .signer()
        .sign_at(
            <TurnServiceDescriptorBody as SignedDescriptorBody>::DOMAIN_TAG,
            &signing_data,
            GOLDEN_EPOCH_MS,
        )
        .unwrap();
    let descriptor = body.clone().into_signed_descriptor(signature);
    assert!(descriptor.descriptor_verify_signature_at(GOLDEN_NETWORK_ID, GOLDEN_EPOCH_MS));
    assert!(descriptor.has_valid_endpoint());
    let element = descriptor.to_element().unwrap();
    let decoded_desc = TurnServiceDescriptor::from_element(&element).unwrap();
    assert_eq!(decoded_desc, descriptor);

    tracer.trace_value(&mut samples, &body).unwrap();
    tracer.trace_value(&mut samples, &descriptor).unwrap();

    let ticket_req = TurnTicketRequest {
        ephemeral_client_pubkey: ephemeral_pk,
        ttl_ms: 60_000,
    };
    tracer.trace_value(&mut samples, &ticket_req).unwrap();

    let signed_ticket = SignedTurnSessionTicket::issue(
        [0x77; 16],
        ephemeral_pk,
        GOLDEN_EPOCH_MS + 60_000,
        &server_key,
    )
    .unwrap();
    let issued = TurnTicketOutcome::Issued {
        ticket: signed_ticket,
        server_pubkey,
    };
    tracer.trace_value(&mut samples, &issued).unwrap();
    tracer.trace_type::<TurnTicketOutcome>(&samples).unwrap();
    tracer.trace_type::<TurnTicketRejection>(&samples).unwrap();

    let registry = tracer.registry().unwrap();
    let registry_json = serde_json::to_vec(&registry).unwrap();

    let req_frame = encode_turn_ticket_request(&ticket_req).unwrap();
    assert_eq!(decode_turn_ticket_request(&req_frame).unwrap(), ticket_req);
    let issued_frame = encode_turn_ticket_outcome(&issued).unwrap();
    assert_eq!(decode_turn_ticket_outcome(&issued_frame).unwrap(), issued);
    let rejected = TurnTicketOutcome::Rejected(TurnTicketRejection::CapacityReached);
    let rejected_frame = encode_turn_ticket_outcome(&rejected).unwrap();
    assert_eq!(
        decode_turn_ticket_outcome(&rejected_frame).unwrap(),
        rejected
    );

    let shape_str = TURN_TICKET_INVOKE_SHAPE.to_string();
    let entries: Vec<(&str, &[u8])> = vec![
        ("format/registry", &registry_json),
        ("descriptor/topic", TURN_SERVICES_TOPIC.as_bytes()),
        (
            "descriptor/domain_tag",
            <TurnServiceDescriptorBody as SignedDescriptorBody>::DOMAIN_TAG.as_bytes(),
        ),
        ("descriptor/signing_data", &signing_data),
        ("descriptor/element", &element),
        ("invoke/backend", TURN_TICKET_INVOKE_BACKEND.as_bytes()),
        ("invoke/shape", shape_str.as_bytes()),
        ("invoke/request_frame", &req_frame),
        ("invoke/issued_frame", &issued_frame),
        ("invoke/rejected_frame", &rejected_frame),
    ];

    let mut manifest = Vec::new();
    for (label, payload) in entries {
        manifest.extend_from_slice(label.as_bytes());
        manifest.push(0);
        manifest.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        manifest.extend_from_slice(payload);
    }
    let digest = to_hex(keccak256(&manifest));
    assert_eq!(
        (WIRE_VERSION, digest.as_str()),
        PINNED,
        "turn-service wire goldens changed; update PINNED = ({WIRE_VERSION}, \"{digest}\")"
    );
}
