use super::*;

/// The credentials of every case.
fn credentials() -> Credentials {
    Credentials {
        user: "me".to_owned(),
        password: "secret".to_owned(),
    }
}

#[test]
fn test_credentials_accept_the_user_or_their_address_with_the_password() {
    let credentials = credentials();
    assert!(credentials.accept(b"me", b"secret"));
    assert!(credentials.accept(b"me@0xabc.rings.rs", b"secret"));
    for (user, password) in [
        (&b"me"[..], &b"secret!"[..]),
        (b"me", b"secre"),
        (b"you", b"secret"),
        (b"you@me", b"secret"),
    ] {
        assert!(!credentials.accept(user, password), "{user:?}");
    }
}

#[test]
fn test_list_patterns_follow_the_wildcards() {
    let cases = [
        ("*", "INBOX", true),
        ("*", "Work/Projects", true),
        ("%", "Work", true),
        ("%", "Work/Projects", false),
        ("Work/%", "Work/Projects", true),
        ("W*s", "Work/Projects", true),
        ("Sent", "Sent", true),
        ("Sent", "Sent2", false),
        ("", "Sent", false),
    ];
    for (pattern, name, matching) in cases {
        assert_eq!(
            list_matches(pattern.as_bytes(), name.as_bytes()),
            matching,
            "{pattern} {name}"
        );
    }
}

#[tokio::test]
async fn test_plain_authentication_reads_authcid_and_password() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), 1024).await.unwrap());
    let mut session = Session::new(Arc::clone(&store), credentials());
    assert!(!session.authenticate_plain(b"\0me\0wrong"));
    assert!(
        !session.authenticate_plain(b"me\0secret"),
        "two fields are malformed"
    );
    assert!(session.authenticate_plain(b"\0me\0secret"));
    assert!(session.is_authenticated());
    assert!(
        !session.authenticate_plain(b"\0me\0secret"),
        "once is enough"
    );
}
