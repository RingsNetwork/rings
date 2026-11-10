use bytes::Bytes;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use rings_core::dht::Did;

use super::declared_variants;
use super::to_hex;
use super::ALICE_ACCOUNT_SEED;
use super::BOB_ACCOUNT_SEED;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::OnionLinkFact;
use crate::onion::circuit::OnionReplayNonce;
use crate::onion::https::decode_outcome;
use crate::onion::https::encode_outcome;
use crate::onion::https::encode_request;
use crate::onion::https::OnionHttpsOutcome;
use crate::onion::https::OnionHttpsRequest;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::pipeline::OnionInvokeArguments;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::session::OnionSessionArguments;
use crate::onion::session::OnionSessionId;
use crate::onion::session::OnionTargetDigest;
use crate::onion::signature::OnionBackendName;
use crate::onion::signature::OnionInputType;
use crate::onion::signature::OnionInvokeShape;
use crate::onion::signature::OnionLatencyClass;
use crate::onion::signature::OnionServiceName;
use crate::onion::signature::OnionShape;
use crate::onion::signature::OnionSymbol;
use crate::onion::signature::OnionWidthClass;
use crate::onion::sphinx::builder::build_loop;
use crate::onion::sphinx::builder::build_surb;
use crate::onion::sphinx::carry;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeaderMac;
use crate::onion::sphinx::layer::OnionApplication;
use crate::onion::sphinx::layer::OnionLayer;
use crate::onion::sphinx::layer::OnionLayerApplication;
use crate::onion::sphinx::layer::OnionLayerHead;
use crate::onion::sphinx::seed::OnionCarrySeed;
use crate::onion::sphinx::seed::OnionSegmentSeed;
use crate::onion::tests::fixture::fixture_secret;
use crate::onion::tests::fixture::grid_expiry;
use crate::onion::tests::fixture::session_loop;
use crate::onion::OnionExitFailure;
use crate::onion::OnionLoop;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteHop;

fn fixture_hop(seed: u8, epoch: OnionProcessEpoch) -> OnionRouteHop {
    OnionRouteHop::new(
        Did::from(u32::from(seed)),
        fixture_secret(seed).pubkey(),
        epoch,
    )
}

fn fixture_loop_for_golden(epoch: OnionProcessEpoch) -> OnionLoop<OnionRouteHop> {
    session_loop([1, 2, 3, 4].map(|seed| fixture_hop(seed, epoch)))
}

/// Onion wire formats: KDF salts and info labels, derived carry seeds and AEZ carry slot, loop
/// classes, signature symbols, uniform layers, session arguments and frames, reply blocks
/// (`OnionSurb`), Sphinx header, seeded loop cell, `OnionLinkFact`, and HTTPS fetch codec.
pub(super) fn onion_goldens() -> Vec<(String, Vec<u8>)> {
    let alice_did = Did::from(fixture_secret(ALICE_ACCOUNT_SEED).address());
    let bob_did = Did::from(fixture_secret(BOB_ACCOUNT_SEED).address());
    let fixture_epoch = OnionProcessEpoch::new([0x31; 16]);
    let fixture_expiry: OnionExpiry = grid_expiry(5);

    let mut out = vec![
        (
            "onion/kdf/header_salt".to_string(),
            b"rings-node:onion-sphinx-header".to_vec(),
        ),
        ("onion/kdf/header_stream_info".to_string(), b"prg".to_vec()),
        ("onion/kdf/header_mac_info".to_string(), b"mac".to_vec()),
        ("onion/kdf/header_blind_info".to_string(), b"blind".to_vec()),
        (
            "onion/kdf/seed_salt".to_string(),
            b"rings-node:onion-carry-seed".to_vec(),
        ),
        (
            "onion/kdf/key_salt".to_string(),
            b"rings-node:onion-carry-key".to_vec(),
        ),
        ("onion/kdf/relay_info".to_string(), b"relay".to_vec()),
        ("onion/kdf/relay_indices".to_string(), vec![1_u8, 2_u8]),
        ("onion/kdf/consumer_info".to_string(), b"consumer".to_vec()),
        ("onion/kdf/key_info".to_string(), b"aez".to_vec()),
    ];

    // Carry seed schedule and AEZ carry slot.
    let segment_seed = OnionSegmentSeed::new([0x42; 32]);
    let derived_seeds = segment_seed.seeds();
    out.push((
        "onion/carry/seed/relay_1".to_string(),
        derived_seeds.relays[0].as_bytes().to_vec(),
    ));
    out.push((
        "onion/carry/seed/relay_2".to_string(),
        derived_seeds.relays[1].as_bytes().to_vec(),
    ));
    out.push((
        "onion/carry/seed/consumer".to_string(),
        derived_seeds.consumer.as_bytes().to_vec(),
    ));
    let segment_keys = segment_seed.keys().unwrap();
    let mut sealed_carry = vec![0_u8; OnionLoopClass::DEFAULT.carry_bytes()];
    carry::seal(
        OnionLoopClass::DEFAULT,
        &segment_keys,
        b"golden carry",
        &mut sealed_carry,
    )
    .unwrap();
    out.push(("onion/carry/slot/KiB16".to_string(), sealed_carry));

    // Loop classes and signature table.
    for class in OnionLoopClass::ALL {
        let spec = format!(
            "cell_bytes={} carry_bytes={} mac_label={}",
            class.cell_bytes(),
            class.carry_bytes(),
            to_hex(class.mac_label())
        );
        out.push((format!("onion/class/{class:?}"), spec.into_bytes()));
    }
    let symbols = std::iter::once((OnionSymbol::Relay, "relay".to_string()))
        .chain(
            OnionShape::all().map(|shape| (OnionSymbol::Shape(shape.clone()), shape.to_string())),
        )
        .collect::<Vec<_>>();
    for (symbol, name) in symbols {
        let spec = format!("code={} name={name}", symbol.code());
        out.push((format!("onion/symbol/{name}"), spec.into_bytes()));
    }

    // Session arguments (64 bytes) and uniform layers (205 bytes).
    let args_authority = OnionSessionArguments {
        session: OnionSessionId::new([0x51; 16]),
        digest: OnionTargetDigest::of(b"example.com:443"),
    };
    let args_ipv6 = OnionSessionArguments {
        session: OnionSessionId::new([0x52; 16]),
        digest: OnionTargetDigest::of(b"[2001:db8::1]:8443"),
    };
    let encoded_args_authority = args_authority.encode();
    let encoded_args_ipv6 = args_ipv6.encode();
    assert_eq!(
        OnionSessionArguments::decode(&encoded_args_authority),
        Some(args_authority)
    );
    assert_eq!(
        OnionSessionArguments::decode(&encoded_args_ipv6),
        Some(args_ipv6)
    );
    out.push((
        "onion/session_arguments/Authority".to_string(),
        encoded_args_authority.as_bytes().to_vec(),
    ));
    out.push((
        "onion/session_arguments/Ipv6".to_string(),
        encoded_args_ipv6.as_bytes().to_vec(),
    ));

    let mac = OnionHeaderMac::new([0x26; 16]);
    let make_layer = |application: OnionLayerApplication| OnionLayer {
        head: OnionLayerHead {
            application,
            next: Did::from(0x0102_0304_u32),
            epoch: fixture_epoch,
            expiry: fixture_expiry,
            nonce: OnionReplayNonce::new([0x23; 16]),
        },
        inbound: OnionCarrySeed::new([0x24; 32]),
        outbound: OnionSegmentSeed::new([0x25; 32]),
    };
    let relay_layer_bytes = make_layer(OnionLayerApplication::Relay).encode(&mac);
    let tcp_layer_bytes = make_layer(OnionLayerApplication::Apply(OnionApplication {
        symbol: OnionServiceName::tcp().into(),
        arguments: encoded_args_authority,
    }))
    .encode(&mac);
    let https_layer_bytes = make_layer(OnionLayerApplication::Apply(OnionApplication {
        symbol: OnionServiceName::https().into(),
        arguments: encoded_args_ipv6,
    }))
    .encode(&mac);
    let invoke_shape = OnionInvokeShape::new(
        OnionInputType::Fixed(OnionWidthClass::B32),
        OnionWidthClass::B64,
        OnionLatencyClass::Ms500,
    );
    let invoke_args = OnionInvokeArguments::new(
        OnionBackendName::parse("jev.typesafe").unwrap(),
        b"const-bytes".to_vec(),
    )
    .unwrap();
    let encoded_invoke_args = invoke_args.encode();
    assert_eq!(
        OnionInvokeArguments::decode(&encoded_invoke_args),
        Some(invoke_args)
    );
    let invoke_layer_bytes = make_layer(OnionLayerApplication::Apply(OnionApplication {
        symbol: OnionShape::Invoke(invoke_shape),
        arguments: encoded_invoke_args,
    }))
    .encode(&mac);
    let (decoded_layer, decoded_mac) = OnionLayer::decode(tcp_layer_bytes.as_slice()).unwrap();
    assert_eq!(
        decoded_layer.encode(&decoded_mac).as_slice(),
        tcp_layer_bytes.as_slice()
    );
    out.push((
        "onion/invoke_arguments/jev.typesafe".to_string(),
        encoded_invoke_args.as_bytes().to_vec(),
    ));
    out.push(("onion/layer/Relay".to_string(), relay_layer_bytes.to_vec()));
    out.push((
        "onion/layer/Apply_invoke".to_string(),
        invoke_layer_bytes.to_vec(),
    ));
    out.push((
        "onion/layer/Apply_tcp".to_string(),
        tcp_layer_bytes.to_vec(),
    ));
    out.push((
        "onion/layer/Apply_https".to_string(),
        https_layer_bytes.to_vec(),
    ));

    // Reply block (`OnionSurb`): pure reply cell production and seeded `build_surb` + `Credit`.
    let raw_surb_block = [
        [0x11_u8; 20].as_slice(),
        vec![0x22_u8; 2919].as_slice(),
        [0x33_u8; 32].as_slice(),
        fixture_expiry.to_wire_ms().to_be_bytes().as_slice(),
    ]
    .concat();
    let decoded_surb = OnionSurb::decode(OnionLoopClass::DEFAULT, &raw_surb_block).unwrap();
    let (_, produced_reply_cell) = decoded_surb.produce(b"golden reply").unwrap();
    out.push((
        "onion/surb/produced_cell".to_string(),
        produced_reply_cell.into_bytes(),
    ));

    let loop_hops = fixture_loop_for_golden(fixture_epoch);
    let (built_surb, built_surb_tag) = build_surb(
        loop_hops.return_path(),
        Did::from(99_u32),
        OnionLoopClass::DEFAULT,
        fixture_expiry,
        &mut ChaCha20Rng::from_seed([0x43; 32]),
    )
    .unwrap();
    out.push((
        "onion/surb/built_reply_tag".to_string(),
        built_surb_tag.tag.as_bytes().to_vec(),
    ));

    // Session frames (Data, Data with target, Fin, Fin with target, Credit, Abort).
    let frame_cases = [
        ("Data", OnionFrame::Data {
            sequence: OnionSequence::new(6),
            target: None,
            payload: Bytes::from_static(b"xyz"),
        }),
        ("Data_with_target", OnionFrame::Data {
            sequence: OnionSequence::new(5),
            target: Some(Bytes::from_static(b"example.com:443")),
            payload: Bytes::from_static(b"ab"),
        }),
        ("Fin", OnionFrame::Fin {
            sequence: OnionSequence::new(7),
            target: None,
        }),
        ("Fin_with_target", OnionFrame::Fin {
            sequence: OnionSequence::new(8),
            target: Some(Bytes::from_static(b"example.com:443")),
        }),
        ("Credit", OnionFrame::Credit {
            mark: OnionSequence::new(0x0102_0304),
            surbs: vec![built_surb],
        }),
        ("Abort", OnionFrame::Abort {
            sequence: OnionSequence::new(9),
        }),
    ];
    for (name, frame) in frame_cases {
        let encoded = frame.encode(OnionLoopClass::DEFAULT).unwrap();
        let decoded = OnionFrame::decode(OnionLoopClass::DEFAULT, encoded.as_slice()).unwrap();
        assert_eq!(
            decoded.encode(OnionLoopClass::DEFAULT).unwrap().as_slice(),
            encoded.as_slice()
        );
        out.push((format!("onion/frame/{name}"), encoded.to_vec()));
    }

    // Seeded full loop build (first cell χ₁ ‖ y₀ + reply tag t_⋄).
    let built_loop = build_loop(
        &loop_hops,
        &[OnionApplication {
            symbol: OnionServiceName::tcp().into(),
            arguments: encoded_args_authority,
        }],
        Did::from(99_u32),
        OnionLoopClass::DEFAULT,
        fixture_expiry,
        b"golden value",
        &mut ChaCha20Rng::from_seed([0x43; 32]),
    )
    .unwrap();
    out.push((
        "onion/loop/KiB16/first_cell".to_string(),
        built_loop.cell.into_bytes(),
    ));
    out.push((
        "onion/loop/KiB16/reply_tag".to_string(),
        built_loop.reply.tag.as_bytes().to_vec(),
    ));

    // Link facts (`OnionLinkFact`), checked against declared variants.
    let link_facts = vec![
        ("Opened", OnionLinkFact::Opened(alice_did)),
        ("Closed", OnionLinkFact::Closed(alice_did)),
        ("Dropped", OnionLinkFact::Dropped(bob_did)),
        ("Reconcile", OnionLinkFact::Reconcile),
    ];
    let declared_link_facts = declared_variants::<OnionLinkFact>();
    assert_eq!(
        link_facts.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_link_facts,
        "OnionLinkFact goldens must cover every declared variant"
    );
    for (name, fact) in link_facts {
        let encoded = fact.encode().unwrap();
        let decoded: OnionLinkFact = rings_codec::deserialize(&encoded).unwrap();
        assert_eq!(decoded, fact);
        out.push((format!("onion/link_fact/{name}"), encoded.to_vec()));
    }

    // HTTPS exit request and outcomes, checked against declared variants.
    let https_request = OnionHttpsRequest {
        method: "POST".to_string(),
        path: "/v1/submit?dry_run=false".to_string(),
        headers: vec![(
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body: b"request-body".to_vec(),
    };
    let encoded_request = encode_request(&https_request).unwrap();
    let decoded_request: OnionHttpsRequest = rings_codec::deserialize(&encoded_request).unwrap();
    assert_eq!(decoded_request, https_request);
    out.push(("onion/https/Request".to_string(), encoded_request));

    let failures = vec![
        ("PermissionDenied", OnionExitFailure::PermissionDenied),
        ("MalformedRequest", OnionExitFailure::MalformedRequest),
        ("Internal", OnionExitFailure::Internal),
    ];
    let declared_failures = declared_variants::<OnionExitFailure>();
    assert_eq!(
        failures.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_failures,
        "OnionExitFailure goldens must cover every declared variant"
    );

    let outcomes = vec![
        (
            "Response",
            OnionHttpsOutcome::Response(OnionHttpsResponse {
                status: 200,
                headers: vec![("x-rings".to_string(), "ok".to_string())],
                body: b"response-body".to_vec(),
            }),
        ),
        (
            "Error",
            OnionHttpsOutcome::Error(OnionExitFailure::PermissionDenied),
        ),
    ];
    let declared_outcomes = declared_variants::<OnionHttpsOutcome>();
    assert_eq!(
        outcomes.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_outcomes,
        "OnionHttpsOutcome goldens must cover every declared variant"
    );
    for (name, outcome) in outcomes {
        let encoded = encode_outcome(&outcome).unwrap();
        let decoded = decode_outcome(&encoded).unwrap();
        assert_eq!(decoded, outcome);
        out.push((format!("onion/https/Outcome/{name}"), encoded));
    }
    for (name, failure) in failures {
        let outcome = OnionHttpsOutcome::Error(failure);
        let encoded = encode_outcome(&outcome).unwrap();
        assert_eq!(decode_outcome(&encoded).unwrap(), outcome);
        out.push((format!("onion/https/Failure/{name}"), encoded));
    }

    out
}
