use super::*;

#[test]
#[rustfmt::skip]
fn test_has_infinite_loop() {
    assert!(!has_infinite_loop(&Vec::<u8>::new()));

    assert!(!has_infinite_loop(&[
        1, 2, 3,
    ]));

    assert!(!has_infinite_loop(&[
        1, 2, 3,
        1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
        1, 2, 3,
        1, 2, 3,
        1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
        1, 1, 2, 3,
           1, 2, 3,
           1, 2, 3,
    ]));

    assert!(!has_infinite_loop(&[
           1, 2, 3,
        1, 1, 2, 3,
           1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
        1, 2, 1, 2, 3,
              1, 2, 3,
              1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
        4, 5, 1, 2, 3,
              1, 2, 3,
              1, 2, 3,
    ]));

    assert!(!has_infinite_loop(&[
        1, 2, 3,
              3,
        1, 2, 3,
              3,
        1, 2, 3,
    ]));

    assert!(!has_infinite_loop(&[
              1,
        1, 2, 3,
              3,
        1, 2, 3,
              3,
        1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
              3,
        1, 2, 3,
              3,
        1, 2, 3,
              3,
        1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
        1, 2, 3,
        1, 2, 3,
              3,
        1, 2, 3,
              3,
        1, 2, 3,
    ]));

    assert!(has_infinite_loop(&[
              1, 2,
           3, 1, 2,
        3, 3, 1, 2,
        3, 3, 1, 2,
        3, 3, 1, 2,
    ]));

    assert!(!has_infinite_loop(&[
           2, 3,
           4, 3,
        1, 2, 3,
           4, 3,
        1, 2, 3,
           4, 3,
    ]));

    assert!(has_infinite_loop(&[
        1, 2, 3,
           4, 3,
        1, 2, 3,
           4, 3,
        1, 2, 3,
           4, 3,
    ]));

    assert!(has_infinite_loop(&[
           1, 2, 3, 4,
        3, 1, 2, 3, 4,
        3, 1, 2, 3, 4,
        3, 1, 2, 3, 4,
    ]));
}

#[test]
fn empty_path_origin_sender_is_checked() {
    let fallback_destination = Did::from(2);
    let relay = MessageRelay::new(vec![], Did::from(1), fallback_destination);

    assert!(matches!(
        relay.try_origin_sender(),
        Err(Error::CannotInferNextHop)
    ));
    assert_eq!(relay.origin_sender(), fallback_destination);
}

#[test]
fn path_report_preserves_legacy_reverse_path() -> Result<()> {
    let origin = Did::from(1);
    let hop = Did::from(2);
    let current = Did::from(3);
    let relay = MessageRelay::new(vec![origin, hop], current, current);

    let report = relay.report(current, ReportReturnPolicy::Path, None)?;

    assert_eq!(report.path, vec![current]);
    assert_eq!(report.next_hop, hop);
    assert_eq!(report.destination, origin);
    Ok(())
}

#[test]
fn routed_report_uses_declared_destination_and_next_hop() -> Result<()> {
    let origin = Did::from(1);
    let current = Did::from(2);
    let return_destination = Did::from(3);
    let routed_next_hop = Did::from(4);
    let relay = MessageRelay::new(vec![origin], current, current);

    let report = relay.report(
        current,
        ReportReturnPolicy::Routed {
            destination: return_destination,
        },
        Some(routed_next_hop),
    )?;

    assert_eq!(report.path, vec![current]);
    assert_eq!(report.next_hop, routed_next_hop);
    assert_eq!(report.destination, return_destination);
    Ok(())
}

#[test]
fn routed_report_requires_explicit_next_hop() {
    let origin = Did::from(1);
    let current = Did::from(2);
    let return_destination = Did::from(3);
    let relay = MessageRelay::new(vec![origin], current, current);

    assert!(matches!(
        relay.report(
            current,
            ReportReturnPolicy::Routed {
                destination: return_destination,
            },
            None,
        ),
        Err(Error::CannotInferNextHop)
    ));
}

#[test]
fn routed_policy_must_be_authorized_by_destination_signer() -> Result<()> {
    let signer = Did::from(1);
    let other = Did::from(2);

    ReportReturnPolicy::Path.validate_authorized_by(signer)?;
    ReportReturnPolicy::Routed {
        destination: signer,
    }
    .validate_authorized_by(signer)?;

    assert!(matches!(
        ReportReturnPolicy::Routed { destination: other }.validate_authorized_by(signer),
        Err(Error::InvalidMessage(_))
    ));
    Ok(())
}
