use super::*;

#[test]
fn test_encode_decode() {
    let test1 = vec![1, 2, 3, 4];

    let encoded1 = test1.encode().unwrap();
    let result1: Vec<u8> = encoded1.decode().unwrap();
    assert_eq!(test1, result1);

    let test1 = test1.as_slice();
    let encoded1 = test1.encode().unwrap();
    let result1: Vec<u8> = encoded1.decode().unwrap();
    assert_eq!(test1, result1);

    let test2 = "abc";
    let encoded2 = test2.encode().unwrap();
    let result2: String = encoded2.decode().unwrap();
    assert_eq!(test2, result2);

    let test3 = String::from("abc");
    let encoded3 = test3.encode().unwrap();
    let result3: String = encoded3.decode().unwrap();
    assert_eq!(test3, result3);
}

#[test]
fn test_from_encoded() {
    let source = [1u8; 32].to_vec();
    let encoded = source.encode().unwrap();
    let v = encoded.to_string();
    let v2 = Encoded::from_encoded_str(&v);
    assert_eq!(encoded, v2);
}
