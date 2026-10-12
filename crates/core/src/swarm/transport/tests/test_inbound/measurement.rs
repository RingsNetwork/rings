use super::*;

/// Reassembly limits small enough to reach in a test: one pending message of at most four
/// chunks of 1 KiB, a 4 KiB shared budget, and four remembered completions.
fn small_reassembly_limits() -> ReassemblyLimits {
    ReassemblyLimits {
        max_pending_messages: 1,
        max_chunk_data_len: 1_024,
        max_message_bytes: 1_024,
        max_chunks_per_message: 4,
        max_total_buffered_cost: 4_096,
        slot_overhead: 8,
        max_completed_ids: 4,
    }
}

/// A transport under [`small_reassembly_limits`], reporting to `measure`.
fn small_reassembly_transport(measure: &Arc<RecordingMeasure>) -> Result<Arc<SwarmTransport>> {
    TransportSpec {
        measure: measure.clone(),
        reassembly_limits: small_reassembly_limits(),
        ..TransportSpec::default()
    }
    .build()
    .map(Arc::new)
}

/// One authenticated partial reassembly: on a [`small_reassembly_transport`], an active peer
/// whose callback reads the injected `clock` has buffered the first of two chunks.
struct BufferedPartial {
    measure: Arc<RecordingMeasure>,
    transport: Arc<SwarmTransport>,
    pending: PendingPeer,
    /// The clock the callback's admission and cleanup read, in epoch milliseconds.
    clock: Arc<Mutex<u128>>,
}

impl BufferedPartial {
    /// Buffer the first of two chunks of `data`; returns the fixture, and the chunk's frame and
    /// expiry as [`Self::deliver_partial`] does.
    async fn new(data: &'static [u8]) -> Result<(Self, Bytes, u128)> {
        let measure = Arc::new(RecordingMeasure::default());
        let transport = small_reassembly_transport(&measure)?;
        let clock = Arc::new(Mutex::new(0));
        let pending =
            pending_peer_with(&transport, SecretKey::random(), Some(Arc::clone(&clock))).await?;
        pending.activate(&transport)?;
        let fixture = Self {
            measure,
            transport,
            pending,
            clock,
        };
        let (frame, expiry) = fixture.deliver_partial(data).await?;
        Ok((fixture, frame, expiry))
    }

    /// Set the clock to `now_ms`: advancing it past an expiry is the whole expiry event, so no
    /// wall time needs to pass.
    fn set_clock(&self, now_ms: u128) {
        *self
            .clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now_ms;
    }

    /// Restart the clock at a fresh chunk's stamp, deliver that chunk as the first of two
    /// chunks of `data`, with a time-to-live of 100 ms, and check it is buffered. Returns its
    /// frame and its expiry on the clock.
    async fn deliver_partial(&self, data: &'static [u8]) -> Result<(Bytes, u128)> {
        let meta = ChunkMeta {
            ttl_ms: 100,
            ..Default::default()
        };
        self.set_clock(meta.ts_ms);
        let expiry = meta.ts_ms.saturating_add(u128::from(meta.ttl_ms));
        let frame = local_wire(
            Message::Chunk(Chunk {
                chunk: [0, 2],
                data: Bytes::from_static(data),
                meta,
            }),
            &self.pending.session,
            self.transport.dht.did,
        )?;
        self.pending.receive(&frame).await?;
        assert!(self.transport.reassembly_budget().buffered_cost_for_test() > 0);
        Ok((frame, expiry))
    }

    /// Resolve once the shared reassembly budget is empty and `failures` receive failures are
    /// charged to the peer.
    async fn await_released_with_failures(&self, failures: usize) {
        let peer = self.pending.peer;
        self.transport
            .reassembly_budget()
            .await_buffered_cost_for_test(|cost| cost == 0)
            .await;
        self.measure
            .await_recorded(|measure| {
                failed_receive_count(measure, peer).is_ok_and(|count| count == failures)
            })
            .await;
    }
}

#[tokio::test]
async fn test_malformed_unbound_transport_does_not_attribute_peer_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer: Did = SecretKey::random().address().into();
    let callback = SwarmTransportCallback::new(transport);

    callback
        .on_admitted_message_for_test(&peer.to_string(), &[0xff])
        .await
        .expect_err("truncated wire metadata must fail");

    assert_eq!(failed_receive_count(&measure, peer)?, 0);
    Ok(())
}

#[tokio::test]
async fn test_invalid_chunk_shape_records_peer_receive_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let pending = pending_peer(&transport).await?;
    pending.activate(&transport)?;
    let frame = local_wire(
        Message::Chunk(Chunk {
            chunk: [0, 0],
            data: Bytes::from_static(b"invalid"),
            meta: ChunkMeta::default(),
        }),
        &pending.session,
        transport.dht.did,
    )?;

    let error = pending
        .callback
        .on_admitted_message_for_test(&pending.peer.to_string(), &frame)
        .await
        .expect_err("invalid chunk shape must fail");

    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InvalidChunkMessage)
    ));
    assert_eq!(failed_receive_count(&measure, pending.peer)?, 1);
    Ok(())
}

#[tokio::test]
async fn test_reassembly_rejections_score_only_invalid_input_and_release_resources() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = small_reassembly_transport(&measure)?;
    let budget = transport.reassembly_budget();
    let pending = pending_peer(&transport).await?;
    pending.activate(&transport)?;
    let peer = pending.peer;
    let first = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"first"),
        meta: ChunkMeta::default(),
    };
    let deliver =
        |chunk: Chunk| local_wire(Message::Chunk(chunk), &pending.session, transport.dht.did);

    pending.receive(&deliver(first.clone())?).await?;
    assert!(budget.buffered_cost_for_test() > 0);

    pending.receive(&deliver(first.clone())?).await?;

    pending
        .receive(&deliver(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"second"),
            meta: ChunkMeta::default(),
        })?)
        .await?;
    assert_eq!(failed_receive_count(&measure, peer)?, 0);

    let conflict_frame = deliver(Chunk {
        data: Bytes::from_static(b"conflict"),
        ..first
    })?;
    let error = pending
        .callback
        .on_admitted_message_for_test(&peer.to_string(), &conflict_frame)
        .await
        .expect_err("conflicting bytes at one position must fail");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InvalidChunkMessage)
    ));
    assert_eq!(failed_receive_count(&measure, peer)?, 1);
    assert_eq!(successful_receive_count(&measure, peer)?, 0);
    assert_eq!(
        pending
            .callback
            .inbound_for_test()
            .admitted_count_for_test(),
        0
    );

    pending.receive(&conflict_frame).await?;
    assert_eq!(
        failed_receive_count(&measure, peer)?,
        1,
        "repeated invalid evidence for one pending message must not double charge"
    );

    pending.callback.inbound_for_test().close_for_test();
    drop(pending);
    budget.await_buffered_cost_for_test(|cost| cost == 0).await;
    Ok(())
}

#[tokio::test]
async fn test_expired_partial_reassembly_releases_shared_budget_without_more_peer_traffic(
) -> Result<()> {
    let (partial, frame, expiry) = BufferedPartial::new(b"partial").await?;
    let peer = partial.pending.peer;
    partial.set_clock(expiry);
    partial.await_released_with_failures(1).await;

    partial.pending.receive(&frame).await?;
    assert_eq!(
        failed_receive_count(&partial.measure, peer)?,
        1,
        "a late chunk for an expired logical message must not charge it again"
    );

    let (_closing_frame, closing_expiry) = partial.deliver_partial(b"closing-partial").await?;
    let callback = &partial.pending.callback;
    let passes_before_close = callback.inbound_for_test().cleanup_passes_for_test();
    callback.inbound_for_test().close_for_test();
    // The close path runs one cleanup pass immediately under the pre-expiry
    // clock. Awaiting that pass, rather than a wall-clock window, proves the
    // retention decision was taken and not merely not yet reached.
    callback
        .inbound_for_test()
        .await_cleanup_passes_for_test(|passes| passes > passes_before_close)
        .await;
    assert!(
        partial
            .transport
            .reassembly_budget()
            .buffered_cost_for_test()
            > 0,
        "closing before TTL must retain only bounded reassembly cleanup state"
    );
    assert_eq!(
        failed_receive_count(&partial.measure, peer)?,
        1,
        "local close before TTL must not immediately penalize the peer"
    );
    partial.set_clock(closing_expiry);
    partial.await_released_with_failures(2).await;
    Ok(())
}

#[tokio::test]
async fn test_authenticated_partial_expiry_remains_attributable_after_disconnect() -> Result<()> {
    let (partial, _frame, expiry) = BufferedPartial::new(b"authenticated-partial").await?;
    partial.transport.disconnect(partial.pending.peer).await?;
    partial.set_clock(expiry);
    partial.await_released_with_failures(1).await;
    Ok(())
}

#[tokio::test]
async fn test_invalid_unbound_transport_envelope_does_not_attribute_peer_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer: Did = SecretKey::random().address().into();
    let callback = SwarmTransportCallback::new(transport);

    callback
        .on_invalid_inbound_frame(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(failed_receive_count(&measure, peer)?, 0);
    Ok(())
}

#[test]
fn test_inbound_mailbox_without_runtime_returns_typed_error() -> Result<()> {
    let transport = default_transport()?;
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let wire = peer.custom_message_wire(&transport, b"valid-message")?;
    let is_runtime_error = futures::executor::block_on(async {
        match peer
            .callback
            .on_admitted_message_for_test("not-a-did", &wire)
            .await
        {
            Err(error) => matches!(
                error.downcast_ref::<Error>(),
                Some(Error::InboundMailboxRuntimeUnavailable)
            ),
            Ok(()) => false,
        }
    });

    assert!(is_runtime_error);
    Ok(())
}
