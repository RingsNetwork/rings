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
use super::DELEGATED_MIN_CHARGE;
use super::NANOS_PER_SECOND;
use crate::dht::Did;
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
            MessageLimit::Enforced,
            1,
            now,
        )
        .expect("origin A uses its application allowance");
    assert!(matches!(
        table.reserve(
            key(1, MessageCategory::Application),
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
            MessageLimit::Enforced,
            1,
            now,
        )
        .expect("origin B keeps its application allowance");
    table
        .reserve(
            key(1, MessageCategory::Storage),
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
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::ZERO,
        )
        .expect("first record is admitted");
    table
        .reserve(
            key(1, MessageCategory::Application),
            MessageLimit::Enforced,
            1,
            OriginQuotaInstant::from_nanos(1),
        )
        .expect("second record is admitted");
    assert!(matches!(
        table.reserve(
            key(3, MessageCategory::Application),
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
/// at 20 messages/s for half an hour, interleaved on one simulated clock. The delegated
/// traffic is never refused, and the other namespace still is, beyond `burst + 8·T`.
#[test]
fn delegated_namespace_is_never_refused_while_another_namespace_still_is() {
    let config = OriginQuotaLaneConfig::default();
    let seconds: u128 = 1_800;
    let message_bytes = DELEGATED_MIN_CHARGE;
    let mut schedule: Vec<(u128, MessageLimit)> = (0..98 * seconds)
        .map(|index| (index * NANOS_PER_SECOND / 98, MessageLimit::Delegated))
        .chain(
            (0..20 * seconds).map(|index| (index * NANOS_PER_SECOND / 20, MessageLimit::Enforced)),
        )
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
    assert!(enforced_admitted <= 32 + 8 * seconds + 1);
    assert!(enforced_admitted < 20 * seconds);
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
