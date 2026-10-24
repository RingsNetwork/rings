use imap_next::imap_types::core::AString;
use imap_next::imap_types::fetch::Part;

use super::*;

/// A multipart/mixed message: a text part, then an encapsulated message/rfc822.
const MIXED: &str = "From: Alice <a@x.test>\r\n\
To: b@y.test\r\n\
Subject: mixed\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
first part\r\n\
--b1\r\n\
Content-Type: message/rfc822\r\n\
\r\n\
From: Carol <c@z.test>\r\n\
Subject: inner\r\n\
\r\n\
inner body\r\n\
--b1--\r\n";

/// A part path.
fn part(path: &[u32]) -> Part {
    Part(
        Vec1::try_from(
            path.iter()
                .map(|n| NonZeroU32::new(*n).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    )
}

/// The section `section` of `MIXED`, as text.
fn section(section: Section<'_>) -> Option<String> {
    parse(MIXED.as_bytes())
        .section(MIXED.as_bytes(), &section)
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

#[test]
fn test_a_single_part_message_has_only_part_one() {
    let text = "Subject: s\r\n\r\nbody\r\n";
    let document = parse(text.as_bytes());
    assert_eq!(
        document.entity(&[NonZeroU32::new(1).unwrap()]),
        Some(&document.top)
    );
    assert_eq!(document.entity(&[NonZeroU32::new(2).unwrap()]), None);
    let body = document.section(text.as_bytes(), &Section::Part(part(&[1])));
    assert_eq!(body.as_deref(), Some(&b"body\r\n"[..]));
}

#[test]
fn test_sections_address_parts_and_encapsulated_messages() {
    assert_eq!(
        section(Section::Part(part(&[1]))).as_deref(),
        Some("first part")
    );
    assert!(section(Section::Mime(part(&[1])))
        .unwrap()
        .starts_with("Content-Type: text/plain"));
    let inner_header = section(Section::Header(Some(part(&[2])))).unwrap();
    assert!(inner_header.starts_with("From: Carol"), "{inner_header}");
    assert!(
        inner_header.ends_with("\r\n\r\n"),
        "a header section ends with its empty line"
    );
    assert_eq!(
        section(Section::Text(Some(part(&[2])))).as_deref(),
        Some("inner body")
    );
    assert_eq!(
        section(Section::Part(part(&[2, 1]))).as_deref(),
        Some("inner body")
    );
    assert_eq!(
        section(Section::Header(Some(part(&[1])))),
        None,
        "part 1 is not a message"
    );
    assert_eq!(section(Section::Part(part(&[3]))), None);
}

#[test]
fn test_header_fields_keep_the_named_fields_only() {
    let names = Vec1::try_from(vec![
        AString::try_from("SUBJECT").unwrap(),
        AString::try_from("to").unwrap(),
    ])
    .unwrap();
    assert_eq!(
        section(Section::HeaderFields(None, names.clone())).as_deref(),
        Some("To: b@y.test\r\nSubject: mixed\r\n\r\n")
    );
    let rest = section(Section::HeaderFieldsNot(None, names)).unwrap();
    assert!(
        rest.starts_with("From: Alice") && !rest.contains("Subject"),
        "{rest}"
    );
}

#[test]
fn test_continuation_lines_follow_their_field() {
    let header = b"Subject: a\r\n b\r\nTo: c\r\n\r\n";
    assert_eq!(
        header_fields(header, |name| name.eq_ignore_ascii_case("subject")),
        b"Subject: a\r\n b\r\n\r\n"
    );
}

#[test]
fn test_body_structure_nests_the_encapsulated_message() {
    let structure = parse(MIXED.as_bytes()).body_structure(MIXED.as_bytes());
    let BodyStructure::Multi {
        bodies, subtype, ..
    } = structure
    else {
        panic!("a multipart");
    };
    assert_eq!(subtype, istring("mixed"));
    let [text, message] = bodies.as_ref() else {
        panic!("two parts");
    };
    assert!(matches!(text, BodyStructure::Single {
        body: Body {
            specific: SpecificFields::Text { .. },
            ..
        },
        ..
    }));
    let BodyStructure::Single {
        body:
            Body {
                specific: SpecificFields::Message { envelope, .. },
                ..
            },
        ..
    } = message
    else {
        panic!("an encapsulated message");
    };
    assert_eq!(envelope.subject, nstring(Some("inner")));
}

#[test]
fn test_envelope_carries_the_header_fields_and_defaults_sender() {
    let envelope = parse(MIXED.as_bytes()).envelope();
    assert_eq!(envelope.subject, nstring(Some("mixed")));
    let [from] = envelope.from.as_slice() else {
        panic!("one sender");
    };
    assert_eq!(from.name, nstring(Some("Alice")));
    assert_eq!(from.mailbox, nstring(Some("a")));
    assert_eq!(from.host, nstring(Some("x.test")));
    assert_eq!(envelope.sender, envelope.from, "Sender defaults to From");
    assert_eq!(
        envelope.reply_to, envelope.from,
        "Reply-To defaults to From"
    );
}

#[test]
fn test_a_non_ascii_display_name_is_an_encoded_word() {
    assert_eq!(encode_word("Alice"), "Alice");
    assert_eq!(encode_word("λ"), "=?utf-8?b?zrs=?=");
}
