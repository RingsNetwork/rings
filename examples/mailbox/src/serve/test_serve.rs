use super::*;

#[test]
fn test_the_backoff_doubles_up_to_its_bound() {
    let mut backoff = BACKOFF_FIRST;
    let mut seen = vec![backoff];
    for _ in 0..10 {
        backoff = next_backoff(backoff);
        seen.push(backoff);
    }
    assert_eq!(seen.get(1), Some(&Duration::from_millis(20)));
    assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(seen.last(), Some(&BACKOFF_MAX));
}
