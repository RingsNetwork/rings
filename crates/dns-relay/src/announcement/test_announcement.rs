use super::*;

/// The name under the owner `name`, which must be valid.
fn sub(name: &str) -> SubName {
    SubName::new(name).unwrap()
}

/// The TXT values `values`, each of which must be valid.
fn txt(values: &[&str]) -> Vec<TxtValue> {
    values
        .iter()
        .map(|value| TxtValue::new(*value).unwrap())
        .collect()
}

#[test]
fn test_txt_value_is_one_character_string() {
    let longest = "v".repeat(MAX_TXT_VALUE_LEN);
    assert!(TxtValue::new(longest).is_ok());
    assert!(TxtValue::new("").is_ok());
    let too_long = "v".repeat(MAX_TXT_VALUE_LEN + 1);
    assert_eq!(
        TxtValue::new(too_long),
        Err(AnnouncementError::TxtValue(MAX_TXT_VALUE_LEN + 1))
    );
}

#[test]
fn test_sub_name_admits_lowercase_labels_only() {
    let longest_label = "l".repeat(MAX_LABEL_LEN);
    for valid in [
        "",
        "_acme-challenge",
        "a.b",
        "a.b.c.d",
        longest_label.as_str(),
    ] {
        assert!(SubName::new(valid).is_ok(), "{valid:?}");
    }
    let too_long_label = "l".repeat(MAX_LABEL_LEN + 1);
    for invalid in [
        "A",
        "a..b",
        ".a",
        "a.",
        "a.b.c.d.e",
        "a b",
        too_long_label.as_str(),
    ] {
        assert_eq!(
            SubName::new(invalid),
            Err(AnnouncementError::SubName(invalid.to_owned())),
            "{invalid:?}"
        );
    }
}

#[test]
fn test_sub_name_lies_at_or_below_its_suffixes() {
    let cases = [
        ("a.b", "b", true),
        ("a.b", "a.b", true),
        ("a.b", "", true),
        ("", "", true),
        ("b", "a.b", false),
        ("ab", "b", false),
        ("a.bc", "c", false),
        ("", "a", false),
    ];
    for (name, other, below) in cases {
        assert_eq!(
            sub(name).is_at_or_below(&sub(other)),
            below,
            "{name} ≤ {other}"
        );
    }
}

#[test]
fn test_announcement_keeps_its_ports_and_records() {
    let announcement = Announcement::new(
        [(SMTP_PORT, ProxyProtocol::V2), (22, ProxyProtocol::Off)],
        [(sub("_acme-challenge"), txt(&["token"]))],
    )
    .unwrap();
    assert_eq!(announcement.serves(22), Some(ProxyProtocol::Off));
    assert_eq!(announcement.serves(SMTP_PORT), Some(ProxyProtocol::V2));
    assert_eq!(announcement.serves(80), None);
    assert_eq!(announcement.serves(0), None);
    assert!(announcement.receives_mail());
    assert_eq!(announcement.served_ports().collect::<Vec<_>>(), vec![
        (22, ProxyProtocol::Off),
        (SMTP_PORT, ProxyProtocol::V2)
    ]);
    assert_eq!(
        announcement.txt_at(&sub("_acme-challenge")),
        txt(&["token"])
    );
    assert!(announcement.txt_at(&SubName::owner()).is_empty());
}

#[test]
fn test_announcement_without_port_25_receives_no_mail() {
    let announcement = Announcement::new([(22, ProxyProtocol::Off)], []).unwrap();
    assert!(!announcement.receives_mail());
    assert!(!announcement.is_empty());
    assert!(Announcement::new([], []).unwrap().is_empty());
}

/// A refused announcement: its ports, its TXT records, and why it is refused.
type RefusedCase = (
    Vec<(u16, ProxyProtocol)>,
    Vec<(SubName, Vec<TxtValue>)>,
    AnnouncementError,
);

#[test]
fn test_announcement_refuses_what_breaks_a_bound() {
    let too_many_ports = (1..=u16::try_from(MAX_SERVED_PORTS + 1).unwrap())
        .map(|port| (port, ProxyProtocol::Off))
        .collect::<Vec<_>>();
    let too_many_names = (0..=MAX_TXT_NAMES)
        .map(|index| (sub(&format!("n{index}")), txt(&["v"])))
        .collect::<Vec<_>>();
    let too_many_values = vec!["v"; MAX_TXT_VALUES + 1];
    let cases: Vec<RefusedCase> = vec![
        (
            vec![(0, ProxyProtocol::Off)],
            vec![],
            AnnouncementError::PortZero,
        ),
        (
            vec![(22, ProxyProtocol::Off), (22, ProxyProtocol::V2)],
            vec![],
            AnnouncementError::DuplicatePort(22),
        ),
        (
            too_many_ports,
            vec![],
            AnnouncementError::TooManyPorts(MAX_SERVED_PORTS + 1),
        ),
        (
            vec![],
            too_many_names,
            AnnouncementError::TooManyTxtNames(MAX_TXT_NAMES + 1),
        ),
        (
            vec![],
            vec![(sub("a"), vec![])],
            AnnouncementError::TxtValueCount {
                name: "a".to_owned(),
                count: 0,
            },
        ),
        (
            vec![],
            vec![(sub("a"), txt(&too_many_values))],
            AnnouncementError::TxtValueCount {
                name: "a".to_owned(),
                count: MAX_TXT_VALUES + 1,
            },
        ),
        (
            vec![],
            vec![(sub("a"), txt(&["1"])), (sub("a"), txt(&["2"]))],
            AnnouncementError::DuplicateTxtName("a".to_owned()),
        ),
    ];
    for (ports, records, error) in cases {
        assert_eq!(
            Announcement::new(ports, records),
            Err(error.clone()),
            "{error}"
        );
    }
}

#[test]
fn test_announcement_names_its_records_and_their_empty_non_terminals() {
    let announcement = Announcement::new([], [(sub("a.b"), txt(&["v"]))]).unwrap();
    for (name, exists) in [
        ("", true),
        ("a.b", true),
        ("b", true),
        ("a", false),
        ("c.a.b", false),
    ] {
        assert_eq!(announcement.names(&sub(name)), exists, "{name:?}");
    }
}
