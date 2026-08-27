use super::*;

#[test]
fn test_compress_iter() {
    let v = vec!['a', 'a', 'f', 'a', 'b', 'b', 'c', 'c', 'c', 'd', 'e'];
    assert_eq!(
        vec![
            ('a', 0, 1),
            ('f', 2, 2),
            ('a', 3, 3),
            ('b', 4, 5),
            ('c', 6, 8),
            ('d', 9, 9),
            ('e', 10, 10),
        ],
        compress_iter(v.into_iter())
    );
}
