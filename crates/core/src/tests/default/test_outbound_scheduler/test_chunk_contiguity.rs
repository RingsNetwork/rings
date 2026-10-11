//! Chunked transfers of one class stay contiguous on the wire.

use rings_transport::connections::dummy_controlled;
use tokio::time::timeout;
use tokio::time::Duration;

use super::await_send;
use super::connected_nodes;
use super::invalid_test_state;
use super::message_chunk;
use super::wait_until;
use crate::chunk::Chunk;
use crate::error::Result;
use crate::message::Message;
use crate::swarm::transport::outbound_submit_count_for_test;
use crate::swarm::transport::reset_outbound_submit_count_for_test;
use crate::tests::default::dummy_hooks::MaxMessageSizeGuard;
use crate::tests::default::dummy_hooks::PausedDeliveryGuard;
use crate::tests::default::Node;

async fn collect_two_chunk_transfers(node: &Node, first: Chunk) -> Result<Vec<Chunk>> {
    timeout(Duration::from_secs(5), async move {
        let first_id = first.meta.id;
        let first_total = first.chunk[1];
        let mut second = None;
        let mut trace = vec![first];

        loop {
            if let Some((_, second_total)) = second {
                if trace.len() == first_total.saturating_add(second_total) {
                    return Ok(trace);
                }
            }

            let payload = node
                .listen_once()
                .await
                .ok_or_else(|| invalid_test_state("message stream closed while tracing chunks"))?;
            let Some(chunk) = message_chunk(payload.transaction.data::<Message>()?) else {
                continue;
            };
            if chunk.meta.id != first_id {
                match second {
                    None => second = Some((chunk.meta.id, chunk.chunk[1])),
                    Some((second_id, _)) if second_id == chunk.meta.id => {}
                    Some(_) => {
                        return Err(invalid_test_state(
                            "a third chunk transfer appeared in the FIFO trace",
                        ));
                    }
                }
            }
            trace.push(chunk);
        }
    })
    .await
    .map_err(|_| invalid_test_state("timed out collecting complete chunk transfers"))?
}

fn assert_transfer_positions(chunks: &[&Chunk]) {
    let total = chunks.len();
    assert!(chunks
        .iter()
        .enumerate()
        .all(|(position, chunk)| chunk.chunk == [position, total]));
}

#[tokio::test]
async fn test_same_class_chunked_transfers_are_contiguous_on_the_wire() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _max_size = MaxMessageSizeGuard::new(8192);
    let _paused_delivery = PausedDeliveryGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_direct_message(Message::custom(&vec![0x11; 50_000])?, peer)
        .await?;
    wait_until("first chunk delivery gate", || {
        dummy_controlled::delivery_future_waiting()
    })
    .await?;
    let first_payload = node2
        .listen_once()
        .await
        .ok_or_else(|| invalid_test_state("expected the first chunk"))?;
    let first = message_chunk(first_payload.transaction.data::<Message>()?)
        .ok_or_else(|| invalid_test_state("the first wire message was not a chunk"))?;
    assert_eq!(first.chunk[0], 0);

    reset_outbound_submit_count_for_test();
    let second_swarm = node1.swarm.clone();
    let second_send = tokio::spawn(async move {
        second_swarm
            .send_direct_message(Message::custom(&vec![0x22; 40_000])?, peer)
            .await
    });
    wait_until("second application transfer submission", || {
        outbound_submit_count_for_test() >= 1
    })
    .await?;
    dummy_controlled::release_delivery_future_gate();

    let trace = collect_two_chunk_transfers(&node2, first).await?;
    await_send(second_send, "second application transfer").await?;
    let first_id = trace
        .first()
        .map(|chunk| chunk.meta.id)
        .ok_or_else(|| invalid_test_state("chunk trace was empty"))?;
    let second_id = trace
        .iter()
        .find(|chunk| chunk.meta.id != first_id)
        .map(|chunk| chunk.meta.id)
        .ok_or_else(|| invalid_test_state("second chunk transfer was not observed"))?;
    let first_chunks: Vec<_> = trace
        .iter()
        .filter(|chunk| chunk.meta.id == first_id)
        .collect();
    let second_chunks: Vec<_> = trace
        .iter()
        .filter(|chunk| chunk.meta.id == second_id)
        .collect();

    assert_transfer_positions(&first_chunks);
    assert_transfer_positions(&second_chunks);
    assert!(trace
        .iter()
        .take(first_chunks.len())
        .all(|chunk| chunk.meta.id == first_id));
    assert!(trace
        .iter()
        .skip(first_chunks.len())
        .all(|chunk| chunk.meta.id == second_id));
    Ok(())
}
