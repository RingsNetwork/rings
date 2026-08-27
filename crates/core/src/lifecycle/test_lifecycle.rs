use super::*;

#[test]
fn test_stop_source_propagates_stop_to_existing_and_cloned_tokens() {
    let source = StopSource::new();
    let first = source.token();
    let second = first.clone();

    assert!(!first.should_stop());
    assert!(!second.should_stop());
    assert!(!source.is_stop_requested());

    source.request_stop();

    assert!(first.should_stop());
    assert!(second.should_stop());
    assert!(source.is_stop_requested());
}

#[test]
fn test_cloned_stop_source_controls_the_same_lifecycle_scope() {
    let source = StopSource::new();
    let cloned_source = source.clone();
    let token = source.token();

    cloned_source.request_stop();

    assert!(token.should_stop());
    assert!(source.is_stop_requested());
}

#[test]
fn test_never_token_is_independent_from_other_sources() {
    let source = StopSource::new();
    let token = StopToken::never();

    source.request_stop();

    assert!(!token.should_stop());
}
