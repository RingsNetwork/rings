use super::assert_single_structured_log_event;

const TARGET: &str = "rings_core::tests::structured";
const EVENT: &str = "expected event";

fn assert_event(lines: &[&str]) -> std::result::Result<(), String> {
    assert_single_structured_log_event(
        lines,
        TARGET,
        EVENT,
        ("tx_id", "wanted"),
        &[("value", "1".to_owned())],
        &[],
    )
}

#[test]
fn test_different_event_with_matching_suffix_is_not_selected() {
    let impostor =
        "scope: rings_core::tests::structured: different: expected event tx_id=wanted value=1";

    assert!(assert_event(&[impostor]).is_err());
}

#[test]
fn test_duplicate_event_with_wrong_transaction_is_rejected_before_field_validation() {
    let valid = "scope: rings_core::tests::structured: expected event tx_id=wanted value=1";
    let duplicate = "scope: rings_core::tests::structured: expected event tx_id=other value=1";

    assert!(assert_event(&[valid, duplicate]).is_err());
}
