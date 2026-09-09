use super::*;

/// The hop budget the fixtures below start with; small enough for a cycle to exhaust quickly.
const FIXTURE_BUDGET: HopBudget = HopBudget::MAX;

fn did(n: u32) -> Did {
    Did::from(n)
}

/// Law: `spend` walks the chain down one step at a time and is undefined at the bottom.
#[test]
fn test_spend_is_the_predecessor_on_the_chain() {
    let mut budget = HopBudget::MAX;
    let mut steps = 0u8;
    while let Some(next) = budget.spend() {
        assert_eq!(next.remaining() + 1, budget.remaining());
        budget = next;
        steps += 1;
    }
    assert_eq!(budget, HopBudget::EXHAUSTED);
    assert_eq!(steps, MAX_RELAY_HOPS);
}

/// Law: `for_ring(f, s) = min(f + s, MAX)`, monotone in both arguments.
#[test]
fn test_for_ring_is_finger_slots_plus_successors_under_the_cap() {
    assert_eq!(HopBudget::for_ring(8, 3).remaining(), 11);
    assert_eq!(HopBudget::for_ring(16, 3).remaining(), 19);
    assert_eq!(HopBudget::for_ring(160, 3), HopBudget::MAX);
    assert_eq!(HopBudget::for_ring(0, 3).remaining(), 3);
    assert_eq!(HopBudget::for_ring(usize::MAX, usize::MAX), HopBudget::MAX);
    assert!(HopBudget::for_ring(8, 3) < HopBudget::for_ring(9, 3));
    assert!(HopBudget::for_ring(8, 3) < HopBudget::for_ring(8, 4));
}

/// A forwarding cycle spends the budget one forward per hop and then drops with the typed error:
/// the witness that replaces history-based loop detection.
#[test]
fn test_forwarding_cycle_exhausts_the_budget_and_drops() -> Result<()> {
    let cycle = [did(1), did(2), did(3)];
    let destination = did(9);
    let mut relay = MessageRelay::new(cycle[0], destination, FIXTURE_BUDGET);
    let mut forwards = 0u8;

    let dropped = loop {
        let index = usize::from(forwards) % cycle.len();
        let current = cycle[index];
        let next = cycle[(index + 1) % cycle.len()];
        match relay.forward(current, next) {
            Ok(forwarded) => {
                assert_eq!(forwarded.destination, destination);
                assert_eq!(forwarded.next_hop, next);
                assert_eq!(
                    forwarded.hop_budget.remaining() + 1,
                    relay.hop_budget.remaining()
                );
                relay = forwarded;
                forwards += 1;
            }
            Err(error) => break error,
        }
    };

    assert!(matches!(dropped, Error::RelayHopBudgetExhausted));
    assert_eq!(forwards, FIXTURE_BUDGET.remaining());
    assert_eq!(relay.hop_budget, HopBudget::EXHAUSTED);
    Ok(())
}

/// A carrier is only forwarded by the node it was addressed to, whatever its budget.
#[test]
fn test_forward_rejects_a_node_the_carrier_was_not_addressed_to() {
    let relay = MessageRelay::new(did(1), did(9), FIXTURE_BUDGET);

    assert!(matches!(
        relay.forward(did(2), did(3)),
        Err(Error::InvalidNextHop)
    ));
}

/// Re-aiming keeps the budget: only `forward` spends it.
#[test]
fn test_reset_destination_keeps_the_budget() -> Result<()> {
    let relay = MessageRelay::new(did(1), did(1), FIXTURE_BUDGET);

    let aimed = relay.reset_destination(did(5));
    assert_eq!(aimed.hop_budget, FIXTURE_BUDGET);
    assert_eq!(aimed.destination, did(5));

    let forwarded = aimed.forward(did(1), did(5))?;
    assert_eq!(
        forwarded.hop_budget.remaining() + 1,
        FIXTURE_BUDGET.remaining()
    );
    Ok(())
}

/// A report is a fresh carrier: it starts from the reporter's own budget, not the request's.
#[test]
fn test_report_is_a_fresh_carrier() -> Result<()> {
    let current = did(2);
    let origin = did(1);
    let next_hop = did(4);
    let request = MessageRelay::new(current, current, HopBudget::EXHAUSTED);

    let report = request.report(current, origin, next_hop, FIXTURE_BUDGET)?;

    assert_eq!(report.next_hop, next_hop);
    assert_eq!(report.destination, origin);
    assert_eq!(report.hop_budget, FIXTURE_BUDGET);
    assert!(matches!(
        request.report(did(3), origin, next_hop, FIXTURE_BUDGET),
        Err(Error::InvalidNextHop)
    ));
    Ok(())
}

/// Decoding admits a budget only inside the invariant, so a peer cannot mint forwards.
#[test]
fn test_decoding_rejects_a_budget_above_the_cap() -> Result<()> {
    let relay = MessageRelay::new(did(1), did(9), FIXTURE_BUDGET);
    let mut wire = rings_codec::serialize(&relay).map_err(Error::CodecSerialize)?;
    let budget_byte = wire
        .iter()
        .rposition(|byte| *byte == MAX_RELAY_HOPS)
        .ok_or_else(|| Error::InvalidMessage("budget byte not found".to_string()))?;
    wire[budget_byte] = MAX_RELAY_HOPS + 1;

    let decoded: std::result::Result<MessageRelay, _> = rings_codec::deserialize(&wire);
    assert!(decoded.is_err());
    assert!(matches!(
        HopBudget::try_from(MAX_RELAY_HOPS + 1),
        Err(Error::RelayHopBudgetAboveMax(claimed)) if claimed == MAX_RELAY_HOPS + 1
    ));
    assert_eq!(HopBudget::try_from(MAX_RELAY_HOPS)?, HopBudget::MAX);
    Ok(())
}
