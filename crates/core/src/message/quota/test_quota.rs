//! Laws of the origin quota: token-bucket refill and burst bounds, lane separation, delegated
//! charges, and bounded record tables.

use super::OriginQuota;
use super::OriginQuotaAdmissionError;
use super::OriginQuotaArithmeticError;
use super::OriginQuotaConfig;
use super::OriginQuotaConfigError;
use super::OriginQuotaCounterState;
use super::OriginQuotaInstant;
use super::OriginQuotaKey;
use super::OriginQuotaLaneConfig;
use super::OriginQuotaLaneCounters;
use super::OriginQuotaRejection;
use super::OriginQuotaTable;
use super::OriginQuotaVerdict;
use super::TokenBucket;
use super::DEFAULT_ORIGIN_QUOTA_BYTES_PER_SECOND;
use super::DEFAULT_ORIGIN_QUOTA_BYTE_BURST;
use super::DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE;
use super::DELEGATED_MIN_CHARGE;
use super::NANOS_PER_SECOND;
use super::ORIGIN_QUOTA_SHARES;
use crate::dht::Did;
use crate::fair_admission::Principal;
use crate::fair_admission::SlotFloorUse;
use crate::fair_admission::SlotRoster;
use crate::message::admission_delegation::MessageLimit;
use crate::message::types::MessageCategory;

fn config(
    message_rate: u64,
    message_burst: u64,
    byte_rate: u64,
    byte_burst: u64,
    max_records: usize,
) -> OriginQuotaLaneConfig {
    OriginQuotaLaneConfig::new(
        message_rate,
        message_burst,
        byte_rate,
        byte_burst,
        max_records,
    )
    .expect("test quota configuration is valid")
}

fn key(origin: u32, lane: MessageCategory) -> OriginQuotaKey {
    OriginQuotaKey::new(7, Did::from(origin), Did::from(99_u32), lane)
}

#[test]
fn message_and_byte_refill_preserve_fractional_tokens() {
    let config = config(2, 2, 10, 10, 4);
    let start = OriginQuota::full(config, OriginQuotaInstant::ZERO);
    let (depleted, admitted) = start
        .admit(config, MessageLimit::Enforced, 10, OriginQuotaInstant::ZERO)
        .expect("first transition is valid");
    assert_eq!(admitted, OriginQuotaVerdict::Admitted);

    let half_second = OriginQuotaInstant::from_nanos(NANOS_PER_SECOND / 2);
    let (refilled, admitted) = depleted
        .admit(config, MessageLimit::Enforced, 5, half_second)
        .expect("refill transition is valid");
    assert_eq!(admitted, OriginQuotaVerdict::Admitted);
    assert_eq!(refilled.whole_message_tokens(), 1);
    assert_eq!(refilled.whole_byte_tokens(), 0);
}

#[test]
fn burst_edges_and_zero_elapsed_time_are_exact() {
    let config = config(1, 2, 4, 8, 4);
    let quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
    let (quota, first) = quota
        .admit(config, MessageLimit::Enforced, 4, OriginQuotaInstant::ZERO)
        .expect("first transition is valid");
    let (quota, second) = quota
        .admit(config, MessageLimit::Enforced, 4, OriginQuotaInstant::ZERO)
        .expect("second transition is valid");
    let (quota, exhausted) = quota
        .admit(config, MessageLimit::Enforced, 0, OriginQuotaInstant::ZERO)
        .expect("zero elapsed transition is valid");

    assert_eq!(first, OriginQuotaVerdict::Admitted);
    assert_eq!(second, OriginQuotaVerdict::Admitted);
    assert_eq!(exhausted, OriginQuotaVerdict::MessageRateExhausted);
    assert_eq!(quota.whole_message_tokens(), 0);
    assert_eq!(quota.whole_byte_tokens(), 0);
}

#[test]
fn byte_exhaustion_is_distinct_and_does_not_consume_message_tokens() {
    let config = config(1, 3, 1, 8, 4);
    let quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
    let (quota, first) = quota
        .admit(config, MessageLimit::Enforced, 8, OriginQuotaInstant::ZERO)
        .expect("first transition is valid");
    let (quota, exhausted) = quota
        .admit(config, MessageLimit::Enforced, 1, OriginQuotaInstant::ZERO)
        .expect("byte rejection is valid");

    assert_eq!(first, OriginQuotaVerdict::Admitted);
    assert_eq!(exhausted, OriginQuotaVerdict::ByteRateExhausted);
    assert_eq!(quota.whole_message_tokens(), 2);
    assert_eq!(quota.whole_byte_tokens(), 0);
}

#[test]
fn transition_is_deterministic_and_rejects_regressed_time() {
    let config = config(3, 3, 7, 7, 4);
    let quota = OriginQuota::full(config, OriginQuotaInstant::from_nanos(10));
    let input = OriginQuotaInstant::from_nanos(20);
    assert_eq!(
        quota.admit(config, MessageLimit::Enforced, 2, input),
        quota.admit(config, MessageLimit::Enforced, 2, input)
    );
    assert_eq!(
        quota.admit(
            config,
            MessageLimit::Enforced,
            2,
            OriginQuotaInstant::from_nanos(9)
        ),
        Err(OriginQuotaArithmeticError::MonotonicTimeRegressed)
    );
}

#[test]
fn long_idle_refill_saturates_without_wrapping() {
    let config = config(u64::MAX, 3, u64::MAX, 7, 4);
    let quota = OriginQuota {
        messages: TokenBucket { scaled_tokens: 0 },
        bytes: TokenBucket { scaled_tokens: 0 },
        last_refill: OriginQuotaInstant::ZERO,
    };
    let (refilled, verdict) = quota
        .admit(
            config,
            MessageLimit::Enforced,
            7,
            OriginQuotaInstant::from_nanos(u128::MAX),
        )
        .expect("saturating refill remains defined");

    assert_eq!(verdict, OriginQuotaVerdict::Admitted);
    assert_eq!(refilled.whole_message_tokens(), 2);
    assert_eq!(refilled.whole_byte_tokens(), 0);
}

#[test]
fn two_origins_and_two_lanes_have_independent_allowances() {
    let lane = config(1, 1, 1, 1, 4);
    let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
    let now = OriginQuotaInstant::ZERO;

    table
        .reserve(
            key(1, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            now,
        )
        .expect("origin A uses its application allowance");
    assert!(matches!(
        table.reserve(
            key(1, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            now
        ),
        Err(OriginQuotaAdmissionError::Verdict(
            OriginQuotaRejection::MessageRate
        ))
    ));
    table
        .reserve(
            key(2, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            now,
        )
        .expect("origin B keeps its application allowance");
    table
        .reserve(
            key(1, MessageCategory::Storage),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            now,
        )
        .expect("origin A keeps its storage allowance");
}

#[test]
fn table_reuses_only_a_fully_replenished_oldest_record() {
    let lane = config(1, 1, 1, 1, 2);
    let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
    table
        .reserve(
            key(2, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::ZERO,
        )
        .expect("first record is admitted");
    table
        .reserve(
            key(1, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::from_nanos(1),
        )
        .expect("second record is admitted");
    assert!(matches!(
        table.reserve(
            key(3, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::from_nanos(2)
        ),
        Err(OriginQuotaAdmissionError::Verdict(
            OriginQuotaRejection::Capacity { capacity: 2 }
        ))
    ));

    table
        .reserve(
            key(3, MessageCategory::Application),
            Principal::Local,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::from_nanos(NANOS_PER_SECOND + 1),
        )
        .expect("oldest fully replenished record is reusable");
    assert_eq!(table.len(), 2);
    assert!(table.get(key(1, MessageCategory::Application)).is_some());
    assert!(table.get(key(2, MessageCategory::Application)).is_none());
    assert!(table.get(key(3, MessageCategory::Application)).is_some());
}

/// Acceptance of #888. One neighbour's Application record under the default limits carries
/// a delegated namespace at 98 messages/s of 16 KiB, the floor itself, and another namespace
/// at 80 messages/s for half an hour, interleaved on one simulated clock. The delegated
/// traffic is never refused, and the other namespace still is, beyond `burst + rate·T`.
#[test]
fn delegated_namespace_is_never_refused_while_another_namespace_still_is() {
    let config = OriginQuotaLaneConfig::default();
    let seconds: u128 = 1_800;
    let enforced_rate_per_second: u128 = 80;
    let message_bytes = DELEGATED_MIN_CHARGE;
    let mut schedule: Vec<(u128, MessageLimit)> = (0..98 * seconds)
        .map(|index| (index * NANOS_PER_SECOND / 98, MessageLimit::Delegated))
        .chain((0..enforced_rate_per_second * seconds).map(|index| {
            (
                index * NANOS_PER_SECOND / enforced_rate_per_second,
                MessageLimit::Enforced,
            )
        }))
        .collect();
    schedule.sort_by_key(|(at, _)| *at);

    let mut quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
    let (mut delegated_refused, mut enforced_admitted) = (0_u128, 0_u128);
    for (at, message_limit) in schedule {
        let now = OriginQuotaInstant::from_nanos(at);
        let (next, verdict) = quota
            .admit(config, message_limit, message_bytes, now)
            .expect("monotonic schedule");
        quota = next;
        let admitted = verdict == OriginQuotaVerdict::Admitted;
        match message_limit {
            MessageLimit::Delegated => delegated_refused += u128::from(!admitted),
            MessageLimit::Enforced => enforced_admitted += u128::from(admitted),
        }
    }

    assert_eq!(delegated_refused, 0);
    assert!(
        enforced_admitted
            <= u128::from(config.message_burst())
                + u128::from(config.message_rate_per_second()) * seconds
                + 1
    );
    assert!(enforced_admitted < enforced_rate_per_second * seconds);
}

/// A delegated admission neither needs nor consumes a message token, and still pays bytes:
/// the floor for a small message, the exact length for a message above it.
#[test]
fn delegated_admission_skips_the_message_bucket_but_not_the_byte_bucket() {
    let floor = u64::try_from(DELEGATED_MIN_CHARGE).expect("floor fits u64");
    let config = config(1, 1, 1, 3 * floor, 4);
    let quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
    let (quota, first) = quota
        .admit(config, MessageLimit::Enforced, 1, OriginQuotaInstant::ZERO)
        .expect("first transition is valid");
    let (quota, small) = quota
        .admit(config, MessageLimit::Delegated, 4, OriginQuotaInstant::ZERO)
        .expect("delegated transition is valid");
    assert_eq!(quota.whole_byte_tokens(), u128::from(3 * floor - 1 - floor));
    let (quota, large) = quota
        .admit(
            config,
            MessageLimit::Delegated,
            DELEGATED_MIN_CHARGE + 7,
            OriginQuotaInstant::ZERO,
        )
        .expect("delegated transition is valid");
    let (quota, byte_refused) = quota
        .admit(config, MessageLimit::Delegated, 1, OriginQuotaInstant::ZERO)
        .expect("byte rejection is valid");

    assert_eq!(first, OriginQuotaVerdict::Admitted);
    assert_eq!(small, OriginQuotaVerdict::Admitted);
    assert_eq!(large, OriginQuotaVerdict::Admitted);
    assert_eq!(byte_refused, OriginQuotaVerdict::ByteRateExhausted);
    assert_eq!(quota.whole_message_tokens(), 0);
    assert_eq!(quota.whole_byte_tokens(), u128::from(floor - 1 - 7));
}

/// The floor bounds how many delegated messages core admits: a neighbour flooding 50-byte
/// delegated messages at 1000/s for a minute is admitted exactly as often as floors of bytes
/// become available, `byte_burst / floor + t · byte_rate / floor` (4096 + 256·t by default).
#[test]
fn small_delegated_messages_are_bounded_by_the_floor_rate() {
    let config = OriginQuotaLaneConfig::default();
    let floor = u128::try_from(DELEGATED_MIN_CHARGE).expect("floor fits u128");
    let seconds: u128 = 60;
    let mut quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
    let mut admitted = 0_u128;
    for index in 0..1_000 * seconds {
        let now = OriginQuotaInstant::from_nanos(index * NANOS_PER_SECOND / 1_000);
        let (next, verdict) = quota
            .admit(config, MessageLimit::Delegated, 50, now)
            .expect("monotonic schedule");
        quota = next;
        admitted += u128::from(verdict == OriginQuotaVerdict::Admitted);
    }
    // Attempts every millisecond outpace the refill of one floor (about 4 ms), and after the
    // first admission the bucket never refills to its cap. So every floor's worth of bytes
    // available by the last attempt, at `t_last = T − 1 ms`, is admitted, and no more:
    // `⌊(byte_burst + byte_rate · t_last) / floor⌋`. A per-message charge above the floor
    // would admit fewer.
    let last_attempt_millis = 1_000 * seconds - 1;
    let available = u128::from(DEFAULT_ORIGIN_QUOTA_BYTE_BURST) * 1_000
        + u128::from(DEFAULT_ORIGIN_QUOTA_BYTES_PER_SECOND) * last_attempt_millis;
    assert_eq!(admitted, available / (floor * 1_000));
    // 4096 in the burst plus 256 per second for 59.999 s.
    assert_eq!(admitted, 4_096 + 256 * last_attempt_millis / 1_000);
}

#[test]
fn invalid_configuration_is_typed_and_deserialization_revalidates() {
    assert_eq!(
        OriginQuotaLaneConfig::new(0, 1, 1, 1, 1),
        Err(OriginQuotaConfigError::ZeroMessageRate)
    );
    let invalid = r#"{
        "message_rate_per_second": 1,
        "message_burst": 1,
        "byte_rate_per_second": 1,
        "byte_burst": 1,
        "max_records": 0
    }"#;
    assert!(serde_json::from_str::<OriginQuotaLaneConfig>(invalid).is_err());
}

#[test]
fn counters_are_bounded_by_lane_and_reason() {
    let counters = OriginQuotaCounterState::default();
    let lane = MessageCategory::Application;
    counters.record(
        lane,
        &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::MessageRate),
    );
    counters.record(
        lane,
        &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::ByteRate),
    );
    counters.record(
        lane,
        &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity { capacity: 1 }),
    );
    counters.record(
        MessageCategory::Storage,
        &OriginQuotaAdmissionError::Arithmetic(OriginQuotaArithmeticError::MonotonicTimeRegressed),
    );

    assert_eq!(counters.snapshot().lane(lane), OriginQuotaLaneCounters {
        message_rate_exhausted: 1,
        byte_rate_exhausted: 1,
        capacity_exhausted: 1,
        arithmetic_failure: 0,
    });
    assert_eq!(
        counters
            .snapshot()
            .lane(MessageCategory::Storage)
            .arithmetic_failure,
        1
    );
}

/// Multiple `origin_account`s arriving on the same connection slot in [`SlotRoster`] have
/// isolated per-origin token buckets within [`SlotAccounting`] (`floor + pool`), while a single
/// slot rotating unreplenished `origin_account`s cannot exceed `floor + pool` records (#1178).
#[test]
fn multiple_origins_on_same_slot_are_isolated_within_slot_accounting_bound() {
    let roster = SlotRoster::new(4);
    let clean = SlotFloorUse::default();
    let (attacker_edge, honest_edge) = (Did::from(10_u32), Did::from(20_u32));
    roster.arrive(attacker_edge, &clean);
    roster.arrive(honest_edge, &clean);
    let attacker_slot = roster.principal_of(Some(attacker_edge));
    let honest_slot = roster.principal_of(Some(honest_edge));
    assert_eq!(attacker_slot, Principal::Slot(0));
    assert_eq!(honest_slot, Principal::Slot(1));

    let lane = config(1, 1, 100, 100, DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE);
    let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
    let now = OriginQuotaInstant::ZERO;
    let max_slot_records = ORIGIN_QUOTA_SHARES.floor + ORIGIN_QUOTA_SHARES.pool;

    for index in 0..max_slot_records {
        let origin = u32::try_from(100 + index).expect("origin fits u32");
        table
            .reserve(
                key(origin, MessageCategory::DhtControl),
                attacker_slot,
                MessageLimit::Enforced,
                10,
                now,
            )
            .expect("distinct origin within slot's floor + pool share is admitted");
    }
    let overflow_origin = u32::try_from(100 + max_slot_records).expect("origin fits u32");
    assert!(matches!(
        table.reserve(
            key(overflow_origin, MessageCategory::DhtControl),
            attacker_slot,
            MessageLimit::Enforced,
            10,
            now,
        ),
        Err(OriginQuotaAdmissionError::Verdict(
            OriginQuotaRejection::Capacity {
                capacity: DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE
            }
        ))
    ));
    assert!(table
        .get(key(overflow_origin, MessageCategory::DhtControl))
        .is_none());

    table
        .reserve(
            key(50_000, MessageCategory::DhtControl),
            honest_slot,
            MessageLimit::Enforced,
            10,
            now,
        )
        .expect("honest peer on slot 1 still admits its reserved floor record");
}

/// Rotating >1024 `origin_account`s on a single connection slot occupies only 1 record as its
/// bucket refills and never evicts an honest peer's record on another slot (#1178).
#[test]
fn rotating_over_1024_origins_on_one_slot_occupies_one_record_and_never_evicts_honest_slot() {
    let roster = SlotRoster::new(4);
    let clean = SlotFloorUse::default();
    let (attacker_edge, honest_edge) = (Did::from(10_u32), Did::from(20_u32));
    roster.arrive(attacker_edge, &clean);
    roster.arrive(honest_edge, &clean);
    let attacker_slot = roster.principal_of(Some(attacker_edge));
    let honest_slot = roster.principal_of(Some(honest_edge));

    let lane = config(1, 1, 1, 1, DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE);
    let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
    let honest_key = key(9_999, MessageCategory::DhtControl);

    table
        .reserve(
            honest_key,
            honest_slot,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::ZERO,
        )
        .expect("honest slot admits its record at t = 0");

    for step in 1..=1_050_u32 {
        let now = OriginQuotaInstant::from_nanos(u128::from(step) * NANOS_PER_SECOND);
        table
            .reserve(
                key(step, MessageCategory::DhtControl),
                attacker_slot,
                MessageLimit::Enforced,
                1,
                now,
            )
            .expect("each refilled step on slot 0 reuses slot 0's own replenished record");
    }

    assert_eq!(table.held(MessageCategory::DhtControl, attacker_slot), 1);
    assert_eq!(table.held(MessageCategory::DhtControl, honest_slot), 1);
    assert_eq!(table.len(), 2);
    assert!(
        table.get(honest_key).is_some(),
        "honest peer's idle record on slot 1 must never be evicted by slot 0"
    );
}

/// Simultaneous unreplenished rotation of >1024 `origin_account`s on a single connection slot
/// across all four lanes (`DhtControl`, `Storage`, `E2e`, `Application`) is bounded by
/// [`ORIGIN_QUOTA_SHARES`] and cannot cause `Capacity` exhaustion for any other connection slot
/// within its floor, [`Principal::Local`], or [`Principal::Inbox`] (#1178).
#[test]
fn unreplenished_sybil_rotation_on_one_slot_preserves_other_slots_local_and_inbox() {
    let lane = config(1, 8_192, 1, 8_192, DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE);
    let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
    let now = OriginQuotaInstant::ZERO;
    let attacker_slot = Principal::Slot(0);
    let honest_slot = Principal::Slot(1);
    let max_attacker_records = ORIGIN_QUOTA_SHARES.floor + ORIGIN_QUOTA_SHARES.pool;
    let sybil_attempts =
        u32::try_from(DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE + 50).expect("record bound fits u32");

    for category in [
        MessageCategory::DhtControl,
        MessageCategory::Storage,
        MessageCategory::E2e,
        MessageCategory::Application,
    ] {
        let honest_existing = key(50_000, category);
        table
            .reserve(honest_existing, honest_slot, MessageLimit::Enforced, 1, now)
            .expect("honest slot admits its first floor record");

        let mut admitted_by_attacker = 0_usize;
        for origin in 1..=sybil_attempts {
            match table.reserve(
                key(origin, category),
                attacker_slot,
                MessageLimit::Enforced,
                1,
                now,
            ) {
                Ok(_) => admitted_by_attacker += 1,
                Err(OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity {
                    capacity: DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE,
                })) => {}
                Err(other) => panic!("unexpected error on {category:?}: {other:?}"),
            }
        }
        assert_eq!(admitted_by_attacker, max_attacker_records);
        assert_eq!(table.held(category, attacker_slot), max_attacker_records);
        assert!(
            table.get(honest_existing).is_some(),
            "existing honest record on {category:?} must remain intact"
        );

        table
            .reserve(
                key(50_002, category),
                Principal::Slot(2),
                MessageLimit::Enforced,
                1,
                now,
            )
            .expect("fresh honest slot 2 admits within its floor");
        table
            .reserve(
                key(50_003, category),
                Principal::Local,
                MessageLimit::Enforced,
                1,
                now,
            )
            .expect("Principal::Local admits within its reserved share");
        table
            .reserve(
                key(50_004, category),
                Principal::Inbox,
                MessageLimit::Enforced,
                1,
                now,
            )
            .expect("Principal::Inbox admits within its reserved share");
    }
}

#[test]
fn reservation_rollback_restores_record_and_principal_accounting() {
    let lane = config(1, 1, 10, 10, DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE);
    let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
    let slot = Principal::Slot(0);
    let first_key = key(1, MessageCategory::Application);

    let reservation = table
        .reserve(
            first_key,
            slot,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::ZERO,
        )
        .expect("initial slot reservation succeeds");
    assert_eq!(table.len(), 1);
    assert_eq!(table.held(MessageCategory::Application, slot), 1);

    reservation.rollback(&mut table);
    assert_eq!(table.len(), 0);
    assert_eq!(table.held(MessageCategory::Application, slot), 0);

    table
        .reserve(
            first_key,
            slot,
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::ZERO,
        )
        .expect("after rollback, origin 1 has its full burst allowance again");
}
