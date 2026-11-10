//! End-to-end Kleisli pipelines over real linked nodes (#834 Phase 2b):
//!
//! - 2-stage composition `f >=> g` across an 8-hop guard-closed Sphinx loop (`n = 2`, `H = 8`),
//!   with non-interference witness that neither stage learns the other's arguments or values;
//! - 3-stage associativity `(f >=> g) >=> k == f >=> (g >=> k)` across an 11-hop loop (`n = 3`,
//!   `H = 11`) routed end-to-end via signed DHT `OnlineNodeDescriptor` and `OnionInvokeDescriptor`
//!   directory entries;
//! - rejection of unregistered backend names `ω`, `quota(n, f, ω)` exhaustion, and `L(f)` evaluation
//!   overruns without leaking error frames or breaking subsequent loops.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use rings_core::dht::Did;
use rings_core::message::MessageSigner;
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;
use rings_test_support::with_hang_guard;
use sha2::Digest;
use sha2::Sha256;

use super::loop_network::EVENT_BOUND;
use crate::descriptor::SignedDescriptor;
use crate::error::Error;
use crate::error::Result;
use crate::onion::native::NativeOnionCircuitHandle;
use crate::onion::runtime::OnionRuntime;
use crate::onion::OnionBackendName;
use crate::onion::OnionExitOffer;
use crate::onion::OnionInputType;
use crate::onion::OnionInvokeCall;
use crate::onion::OnionInvokeDescriptor;
use crate::onion::OnionInvokeDescriptorBody;
use crate::onion::OnionInvokeFn;
use crate::onion::OnionInvokeOffer;
use crate::onion::OnionInvokePipeline;
use crate::onion::OnionInvokePolicy;
use crate::onion::OnionInvokeShape;
use crate::onion::OnionInvokeStage;
use crate::onion::OnionLatencyClass;
use crate::onion::OnionLoop;
use crate::onion::OnionLoopRelay;
use crate::onion::OnionPipelineRoute;
use crate::onion::OnionRole;
use crate::onion::OnionRouteError;
use crate::onion::OnionRouteHop;
use crate::onion::OnionWidthClass;
use crate::onion::ONION_INVOKES_TOPIC;
use crate::online::OnlineNodeCapabilities;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
use crate::processor::Processor;
use crate::provider::Provider;
#[cfg(not(feature = "dummy"))]
use crate::tests::native::network_test_guard;
use crate::tests::native::prepare_processor_with_onion_role;
use crate::tests::native::signal_edge;

struct PipelineNode {
    processor: Arc<Processor>,
    runtime: OnionRuntime,
    handle: NativeOnionCircuitHandle,
}

impl PipelineNode {
    async fn new(role: OnionRole<OnionExitOffer>) -> Result<Self> {
        let processor = Arc::new(prepare_processor_with_onion_role(role).await);
        let provider = Arc::new(Provider::from_processor(Arc::clone(&processor)));
        let runtime = provider.onion_runtime()?;
        let handle = NativeOnionCircuitHandle::new(runtime.clone());
        provider.set_backend()?;
        Ok(Self {
            processor,
            runtime,
            handle,
        })
    }

    fn did(&self) -> Did {
        self.processor.did()
    }

    fn route_hop(&self) -> OnionRouteHop {
        OnionRouteHop::new(
            self.did(),
            self.processor.delegatee_key().delegatee_public_key(),
            self.processor.onion_process_epoch().get(),
        )
    }

    fn signed_online_descriptor(&self, now_ms: u128) -> Result<OnlineNodeDescriptor> {
        let key = self.processor.delegatee_key();
        let network_id = self.processor.swarm.network_id();
        let mode = self.processor.swarm.dht_protocol_mode();
        let capabilities = if self.processor.onion_role().registers_relay() {
            OnlineNodeCapabilities::onion_relay(self.processor.onion_process_epoch().get())
        } else {
            OnlineNodeCapabilities::default()
        };
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did: self.did(),
                public_key: key
                    .delegation()
                    .delegator_verification_pubkey()
                    .map_err(Error::CoreError)?,
                delegatee_public_key: key.delegatee_public_key(),
                node_type: OnlineNodeType::Native,
                network_id,
                storage_redundancy: mode.storage_redundancy,
                dht_virtual_nodes: mode.dht_virtual_nodes,
                capabilities,
                endpoint_hint: None,
                started_at_ms: now_ms,
                fresh_for_ms: 90_000,
                expires_at_ms: now_ms + 90_000,
                version: "test".to_string(),
            },
            MessageSigner::new(&key, network_id),
        )
        .map(|desc| desc.heard_at(now_ms))
        .map_err(Error::CoreError)
    }

    fn signed_invoke_descriptor(
        &self,
        shape: OnionInvokeShape,
        backend: OnionBackendName,
        policy: OnionInvokePolicy,
        now_ms: u128,
    ) -> Result<OnionInvokeDescriptor> {
        let key = self.processor.delegatee_key();
        let network_id = self.processor.swarm.network_id();
        OnionInvokeDescriptor::new_signed(
            OnionInvokeDescriptorBody {
                did: self.did(),
                public_key: key
                    .delegation()
                    .delegator_verification_pubkey()
                    .map_err(Error::CoreError)?,
                delegatee_public_key: key.delegatee_public_key(),
                process_epoch: self.processor.onion_process_epoch().get(),
                node_type: OnlineNodeType::Native,
                network_id,
                shape,
                backend,
                policy,
                started_at_ms: now_ms,
                fresh_for_ms: 90_000,
                expires_at_ms: now_ms + 90_000,
                version: "test".to_string(),
            },
            MessageSigner::new(&key, network_id),
        )
        .map(|desc| desc.heard_at(now_ms))
        .map_err(Error::CoreError)
    }
}

async fn link_pair(a: &PipelineNode, b: &PipelineNode) -> Result<()> {
    signal_edge(&a.processor, &b.processor).await?;
    with_hang_guard("kleisli pipeline link live", EVENT_BOUND, async {
        a.runtime.links().live(b.did()).await;
        b.runtime.links().live(a.did()).await;
    })
    .await;
    Ok(())
}

/// Pure function `f(ā₁, v₀)` : `32B → 64B` = `(v₀ ⊕ pad32(ā₁)) ‖ SHA-256(v₀ ‖ ā₁)`.
fn expand_xor(constants: &[u8], input: &[u8]) -> Vec<u8> {
    assert_eq!(input.len(), 32);
    let mut pad_const = [0_u8; 32];
    let copy_len = constants.len().min(32);
    pad_const[..copy_len].copy_from_slice(&constants[..copy_len]);
    let mut left = [0_u8; 32];
    for i in 0..32 {
        left[i] = input[i] ^ pad_const[i];
    }
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.update(constants);
    let right: [u8; 32] = hasher.finalize().into();
    [left.as_slice(), right.as_slice()].concat()
}

/// Pure function `g(ā₂, v₁)` : `64B → 32B` = `SHA-256(v₁ ‖ ā₂)`.
fn fold_mac(constants: &[u8], input: &[u8]) -> Vec<u8> {
    assert_eq!(input.len(), 64);
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.update(constants);
    hasher.finalize().to_vec()
}

/// Build an explicit `OnionPipelineRoute` over `[g, r02, h1, r11, r12, h2, r21]` (`n = 2`, `H = 8`).
fn two_stage_route(
    nodes: &[PipelineNode; 7],
    pipeline: &OnionInvokePipeline,
    now_ms: u128,
) -> Result<OnionPipelineRoute> {
    let [g, r02, h1, r11, r12, h2, r21] = nodes;
    let mut relays = [
        (g.route_hop(), true),
        (r02.route_hop(), false),
        (r11.route_hop(), false),
        (r12.route_hop(), false),
        (r21.route_hop(), false),
    ]
    .into_iter();
    let hops = OnionLoop::try_unfold(vec![h1.route_hop()], h2.route_hop(), |kind| {
        let (hop, is_guard) = relays.next().ok_or(Error::InvalidData)?;
        assert_eq!(kind == OnionLoopRelay::Guard, is_guard);
        Ok(hop)
    })?;
    let required = pipeline.required_symbols();
    let descriptors = vec![
        h1.signed_invoke_descriptor(
            required[0].0,
            required[0].1.clone(),
            OnionInvokePolicy::default(),
            now_ms,
        )?,
        h2.signed_invoke_descriptor(
            required[1].0,
            required[1].1.clone(),
            OnionInvokePolicy::default(),
            now_ms,
        )?,
    ];
    Ok(OnionPipelineRoute::new(required, hops, descriptors))
}

/// 2-stage Kleisli composition `f >=> g` over a sealed 8-hop loop (`client → g → r₀₂ → h₁ →
/// r₁₁ → r₁₂ → h₂ → r₂₁ → g → client`), plus rejection of unregistered backends, quota exhaustion,
/// and `L(f)` overruns without leaking errors or corrupting subsequent loops.
#[tokio::test]
async fn test_two_stage_kleisli_pipeline_and_silent_rejections_over_sealed_loop() -> Result<()> {
    #[cfg(not(feature = "dummy"))]
    let _serial = network_test_guard().await;

    let shape_expand = OnionInvokeShape::new(
        OnionInputType::Fixed(OnionWidthClass::B32),
        OnionWidthClass::B64,
        OnionLatencyClass::Ms10,
    );
    let shape_fold = OnionInvokeShape::new(
        OnionInputType::Fixed(OnionWidthClass::B64),
        OnionWidthClass::B32,
        OnionLatencyClass::Ms10,
    );
    let expand_name = OnionBackendName::parse("expand.xor")?;
    let slow_name = OnionBackendName::parse("expand.slow")?;
    let capped_name = OnionBackendName::parse("expand.capped")?;
    let fold_name = OnionBackendName::parse("fold.mac")?;

    let capped_policy = OnionInvokePolicy {
        max_concurrent: 1,
        max_invocations_per_minute: 1,
    };
    let offer_h1 = OnionInvokeOffer::new([
        (
            shape_expand,
            expand_name.clone(),
            OnionInvokePolicy::default(),
        ),
        (
            shape_expand,
            slow_name.clone(),
            OnionInvokePolicy::default(),
        ),
        (shape_expand, capped_name.clone(), capped_policy),
    ])?;
    let offer_h2 =
        OnionInvokeOffer::new([(shape_fold, fold_name.clone(), OnionInvokePolicy::default())])?;

    let client = PipelineNode::new(OnionRole::Client).await?;
    let g = PipelineNode::new(OnionRole::Relay).await?;
    let r02 = PipelineNode::new(OnionRole::Relay).await?;
    let h1 = PipelineNode::new(OnionRole::Invoke(offer_h1)).await?;
    let r11 = PipelineNode::new(OnionRole::Relay).await?;
    let r12 = PipelineNode::new(OnionRole::Relay).await?;
    let h2 = PipelineNode::new(OnionRole::Invoke(offer_h2)).await?;
    let r21 = PipelineNode::new(OnionRole::Relay).await?;

    // Wire the 8-edge loop cycle: client-g-r02-h1-r11-r12-h2-r21-g.
    link_pair(&client, &g).await?;
    link_pair(&g, &r02).await?;
    link_pair(&r02, &h1).await?;
    link_pair(&h1, &r11).await?;
    link_pair(&r11, &r12).await?;
    link_pair(&r12, &h2).await?;
    link_pair(&h2, &r21).await?;
    link_pair(&r21, &g).await?;

    let seen_h1: Arc<Mutex<Vec<OnionInvokeCall>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_h2: Arc<Mutex<Vec<OnionInvokeCall>>> = Arc::new(Mutex::new(Vec::new()));

    {
        let log = Arc::clone(&seen_h1);
        h1.handle.register_invoke(
            shape_expand,
            expand_name.clone(),
            OnionInvokePolicy::default(),
            OnionInvokeFn(move |call: OnionInvokeCall| {
                let log = Arc::clone(&log);
                async move {
                    let out = expand_xor(&call.constants, &call.input);
                    log.lock().expect("lock").push(call);
                    Ok(out)
                }
            }),
        )?;
    }
    h1.handle.register_invoke(
        shape_expand,
        slow_name.clone(),
        OnionInvokePolicy::default(),
        OnionInvokeFn(|call: OnionInvokeCall| async move {
            // Exceeds L(f) = 10ms -> must be dropped on overrun (L4).
            let _ = sleep(Duration::from_millis(120)).await;
            Ok(expand_xor(&call.constants, &call.input))
        }),
    )?;
    h1.handle.register_invoke(
        shape_expand,
        capped_name.clone(),
        capped_policy,
        OnionInvokeFn(|call: OnionInvokeCall| async move {
            Ok(expand_xor(&call.constants, &call.input))
        }),
    )?;
    {
        let log = Arc::clone(&seen_h2);
        h2.handle.register_invoke(
            shape_fold,
            fold_name.clone(),
            OnionInvokePolicy::default(),
            OnionInvokeFn(move |call: OnionInvokeCall| {
                let log = Arc::clone(&log);
                async move {
                    let out = fold_mac(&call.constants, &call.input);
                    log.lock().expect("lock").push(call);
                    Ok(out)
                }
            }),
        )?;
    }

    let nodes = [g, r02, h1, r11, r12, h2, r21];
    let now_ms = get_epoch_ms();

    // 1. Happy path: 2-stage Kleisli composition `expand.xor >=> fold.mac`.
    let stage1 = OnionInvokeStage::new(shape_expand, expand_name.clone(), b"alpha-secret")?;
    let stage2 = OnionInvokeStage::new(shape_fold, fold_name.clone(), b"beta-secret")?;
    let pipeline = stage1.then(stage2)?;
    let route = two_stage_route(&nodes, &pipeline, now_ms)?;
    let v0 = [0x5a_u8; 32];
    let expected_v1 = expand_xor(b"alpha-secret", &v0);
    let expected_v2 = fold_mac(b"beta-secret", &expected_v1);

    let result = with_hang_guard(
        "two-stage Kleisli invoke",
        EVENT_BOUND,
        client
            .handle
            .invoke_pipeline_over_route(&route, &pipeline, &v0),
    )
    .await?;
    assert_eq!(result, expected_v2);

    // Non-interference check on observed hop inputs: h₁ saw (alpha-secret, v₀) only, h₂ saw
    // (beta-secret, v₁) only.
    {
        let h1_calls = seen_h1.lock().expect("lock");
        let h2_calls = seen_h2.lock().expect("lock");
        assert_eq!(h1_calls.len(), 1);
        assert_eq!(h1_calls[0].constants, b"alpha-secret");
        assert_eq!(h1_calls[0].input, v0);
        assert_eq!(h2_calls.len(), 1);
        assert_eq!(h2_calls[0].constants, b"beta-secret");
        assert_eq!(h2_calls[0].input, expected_v1);
    }

    // 2. Unregistered backend `ω` at intermediate hop h₁ drops silently (h₂ never sees it).
    let unregistered_pipeline = OnionInvokeStage::new(
        shape_expand,
        OnionBackendName::parse("not.registered")?,
        b"x",
    )?
    .then(OnionInvokeStage::new(shape_fold, fold_name.clone(), b"y")?)?;
    let unregistered_route = two_stage_route(&nodes, &unregistered_pipeline, now_ms)?;
    let unregistered_res = client
        .handle
        .invoke_pipeline_over_route_with_options(
            &unregistered_route,
            &unregistered_pipeline,
            unregistered_pipeline.loop_class(),
            &v0,
            Some(Duration::from_millis(350)),
        )
        .await;
    assert!(matches!(
        unregistered_res,
        Err(Error::OnionRouteError(OnionRouteError::PipelineTimedOut))
    ));
    assert_eq!(seen_h2.lock().expect("lock").len(), 1, "h₂ never reached");

    // 3. Overrunning evaluation at h₁ (`120 ms > L(f) = 10 ms`) is dropped on overrun (L4).
    let slow_pipeline = OnionInvokeStage::new(shape_expand, slow_name, b"s")?
        .then(OnionInvokeStage::new(shape_fold, fold_name.clone(), b"t")?)?;
    let slow_route = two_stage_route(&nodes, &slow_pipeline, now_ms)?;
    let slow_res = client
        .handle
        .invoke_pipeline_over_route_with_options(
            &slow_route,
            &slow_pipeline,
            slow_pipeline.loop_class(),
            &v0,
            Some(Duration::from_millis(400)),
        )
        .await;
    assert!(matches!(
        slow_res,
        Err(Error::OnionRouteError(OnionRouteError::PipelineTimedOut))
    ));
    assert_eq!(
        seen_h2.lock().expect("lock").len(),
        1,
        "late output never released"
    );

    // 4. Per-(shape, backend) quota (`max_invocations_per_minute: 1`) admits 1st, drops 2nd.
    let capped_pipeline = OnionInvokeStage::new(shape_expand, capped_name, b"q")?
        .then(OnionInvokeStage::new(shape_fold, fold_name, b"r")?)?;
    let capped_route = two_stage_route(&nodes, &capped_pipeline, now_ms)?;
    let first_capped = with_hang_guard(
        "first capped invoke",
        EVENT_BOUND,
        client
            .handle
            .invoke_pipeline_over_route(&capped_route, &capped_pipeline, &v0),
    )
    .await?;
    assert_eq!(first_capped, fold_mac(b"r", &expand_xor(b"q", &v0)));
    let second_capped = client
        .handle
        .invoke_pipeline_over_route_with_options(
            &capped_route,
            &capped_pipeline,
            capped_pipeline.loop_class(),
            &v0,
            Some(Duration::from_millis(350)),
        )
        .await;
    assert!(matches!(
        second_capped,
        Err(Error::OnionRouteError(OnionRouteError::PipelineTimedOut))
    ));

    // 5. Subsequent valid loops on `expand.xor >=> fold.mac` continue to succeed cleanly.
    let again = with_hang_guard(
        "post-rejection valid invoke",
        EVENT_BOUND,
        client
            .handle
            .invoke_pipeline_over_route(&route, &pipeline, &v0),
    )
    .await?;
    assert_eq!(again, expected_v2);
    Ok(())
}

/// 3-stage Kleisli associativity `(f >=> g) >=> k == f >=> (g >=> k)` routed via signed DHT
/// `OnlineNodeDescriptor` and `OnionInvokeDescriptor` directory entries across a full 11-hop
/// (`n = 3`, `H = 11`, `10` distinct overlay nodes) mesh!
#[tokio::test]
async fn test_three_stage_kleisli_associativity_via_directory_discovery() -> Result<()> {
    #[cfg(not(feature = "dummy"))]
    let _serial = network_test_guard().await;

    let shape_f = OnionInvokeShape::new(
        OnionInputType::Unit,
        OnionWidthClass::B32,
        OnionLatencyClass::Ms10,
    );
    let shape_g = OnionInvokeShape::new(
        OnionInputType::Fixed(OnionWidthClass::B32),
        OnionWidthClass::B64,
        OnionLatencyClass::Ms10,
    );
    let shape_k = OnionInvokeShape::new(
        OnionInputType::Fixed(OnionWidthClass::B64),
        OnionWidthClass::B32,
        OnionLatencyClass::Ms10,
    );
    let name_f = OnionBackendName::parse("seed.const")?;
    let name_g = OnionBackendName::parse("expand.xor")?;
    let name_k = OnionBackendName::parse("fold.mac")?;

    let client = PipelineNode::new(OnionRole::Client).await?;
    let host_f = PipelineNode::new(OnionRole::Invoke(OnionInvokeOffer::new([(
        shape_f,
        name_f.clone(),
        OnionInvokePolicy::default(),
    )])?))
    .await?;
    let host_g = PipelineNode::new(OnionRole::Invoke(OnionInvokeOffer::new([(
        shape_g,
        name_g.clone(),
        OnionInvokePolicy::default(),
    )])?))
    .await?;
    let host_k = PipelineNode::new(OnionRole::Invoke(OnionInvokeOffer::new([(
        shape_k,
        name_k.clone(),
        OnionInvokePolicy::default(),
    )])?))
    .await?;

    let mut relays = Vec::with_capacity(7);
    for _ in 0..7 {
        relays.push(PipelineNode::new(OnionRole::Relay).await?);
    }

    host_f.handle.register_invoke(
        shape_f,
        name_f.clone(),
        OnionInvokePolicy::default(),
        OnionInvokeFn(|call: OnionInvokeCall| async move {
            Ok(Sha256::digest(&call.constants).to_vec())
        }),
    )?;
    host_g.handle.register_invoke(
        shape_g,
        name_g.clone(),
        OnionInvokePolicy::default(),
        OnionInvokeFn(|call: OnionInvokeCall| async move {
            Ok(expand_xor(&call.constants, &call.input))
        }),
    )?;
    host_k.handle.register_invoke(
        shape_k,
        name_k.clone(),
        OnionInvokePolicy::default(),
        OnionInvokeFn(
            |call: OnionInvokeCall| async move { Ok(fold_mac(&call.constants, &call.input)) },
        ),
    )?;

    let mut overlay = vec![host_f, host_g, host_k];
    overlay.extend(relays);
    assert_eq!(overlay.len(), 10);

    // Populate the client's directory entries (`ONLINE_NODES_TOPIC` and `ONION_INVOKES_TOPIC`).
    let now_ms = get_epoch_ms();
    let online_descriptors = overlay
        .iter()
        .map(|node| node.signed_online_descriptor(now_ms))
        .collect::<Result<Vec<_>>>()?;
    let invoke_descriptors = vec![
        overlay[0].signed_invoke_descriptor(
            shape_f,
            name_f.clone(),
            OnionInvokePolicy::default(),
            now_ms,
        )?,
        overlay[1].signed_invoke_descriptor(
            shape_g,
            name_g.clone(),
            OnionInvokePolicy::default(),
            now_ms,
        )?,
        overlay[2].signed_invoke_descriptor(
            shape_k,
            name_k.clone(),
            OnionInvokePolicy::default(),
            now_ms,
        )?,
    ];
    client
        .processor
        .storage_store(Processor::registry_entry(
            ONLINE_NODES_TOPIC,
            online_descriptors,
        )?)
        .await?;
    client
        .processor
        .storage_store(Processor::registry_entry(
            ONION_INVOKES_TOPIC,
            invoke_descriptors,
        )?)
        .await?;

    let f = OnionInvokeStage::new(shape_f, name_f, b"seed-k0")?;
    let g = OnionInvokeStage::new(shape_g, name_g, b"salt-k1")?;
    let k = OnionInvokeStage::new(shape_k, name_k, b"key-k2")?;

    let left_assoc = f.clone().then(g.clone())?.then_stage(k.clone())?;
    let right_assoc = f.into_pipeline().then(g.then(k)?)?;
    assert_eq!(left_assoc, right_assoc);

    // Without a direct peer link, `build_onion_pipeline_route` rejects route selection because no
    // first-hop guard is directly connected.
    assert!(client
        .processor
        .build_onion_pipeline_route(&left_assoc)
        .await
        .is_err());

    // Link the client to a directly connected relay guard, select the 11-hop guard-closed route
    // from the signed DHT directory entries, then establish the remaining loop edges.
    link_pair(&client, &overlay[3]).await?;
    let route = client
        .processor
        .build_onion_pipeline_route(&left_assoc)
        .await?;
    assert_eq!(route.hops().positions().count(), 11);
    let find_node = |did: Did| -> &PipelineNode {
        overlay
            .iter()
            .find(|node| node.did() == did)
            .expect("selected hop belongs to overlay")
    };
    let positions = route
        .hops()
        .positions()
        .map(|hop| hop.did)
        .collect::<Vec<_>>();
    assert_eq!(positions[0], overlay[3].did());
    let mut linked_edges = std::collections::BTreeSet::new();
    let mut ensure_edge = |a: Did, b: Did| {
        let key = if a < b { (a, b) } else { (b, a) };
        linked_edges.insert(key)
    };
    for pair in positions.windows(2) {
        if ensure_edge(pair[0], pair[1]) {
            link_pair(find_node(pair[0]), find_node(pair[1])).await?;
        }
    }

    let out_left = with_hang_guard(
        "left-associated 3-stage Kleisli pipeline",
        EVENT_BOUND,
        client
            .handle
            .invoke_pipeline_over_route(&route, &left_assoc, b""),
    )
    .await?;
    let out_right = with_hang_guard(
        "right-associated 3-stage Kleisli pipeline",
        EVENT_BOUND,
        client
            .handle
            .invoke_pipeline_over_route(&route, &right_assoc, b""),
    )
    .await?;

    let expected = fold_mac(
        b"key-k2",
        &expand_xor(b"salt-k1", Sha256::digest(b"seed-k0").as_slice()),
    );
    assert_eq!(out_left, expected);
    assert_eq!(out_right, expected);
    Ok(())
}
