use std::ops::Deref;

use base58_monero as b58m;
use serde::Deserialize;
use serde::Serialize;

use crate::err::Error;
use crate::err::Result;

pub trait Encoder {
    fn encode(&self) -> Result<Encoded>;
}

pub trait Decoder: Sized {
    fn from_encoded(encoded: &Encoded) -> Result<Self>;
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Encoded(String);

impl Encoded {
    pub fn value(&self) -> &String {
        &self.0
    }
}

impl Deref for Encoded {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

impl Encoder for String {
    fn encode(&self) -> Result<Encoded> {
        Ok(Encoded(
            b58m::encode_check(self.as_bytes()).map_err(|_| Error::Encode)?,
        ))
    }
}

impl Decoder for String {
    fn from_encoded(encoded: &Encoded) -> Result<String> {
        let d = Vec::from_encoded(encoded)?;
        String::from_utf8(d).map_err(|_| Error::Decode)
    }
}

impl Encoder for &str {
    fn encode(&self) -> Result<Encoded> {
        self.as_bytes().encode()
    }
}

impl Encoder for &[u8] {
    fn encode(&self) -> Result<Encoded> {
        Ok(Encoded(
            b58m::encode_check(self).map_err(|_| Error::Encode)?,
        ))
    }
}

impl Encoder for Vec<u8> {
    fn encode(&self) -> Result<Encoded> {
        Ok(Encoded(
            b58m::encode_check(self).map_err(|_| Error::Encode)?,
        ))
    }
}

impl Decoder for Vec<u8> {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        b58m::decode_check(encoded.deref()).map_err(|_| Error::Decode)
    }
}

impl ToString for Encoded {
    fn to_string(&self) -> String {
        self.deref().to_owned()
    }
}

impl From<String> for Encoded {
    fn from(v: String) -> Self {
        Self(v)
    }
}

impl From<&str> for Encoded {
    fn from(v: &str) -> Self {
        Self(v.to_owned())
    }
}

impl From<Encoded> for Vec<u8> {
    fn from(a: Encoded) -> Self {
        a.to_string().as_bytes().to_vec()
    }
}

impl TryFrom<Vec<u8>> for Encoded {
    type Error = Error;
    fn try_from(a: Vec<u8>) -> Result<Self> {
        let s: String = String::from_utf8(a)?;
        Ok(s.into())
    }
}

impl Encoded {
    pub fn from_encoded_str(str: &str) -> Self {
        Self(str.to_owned())
    }

    pub fn decode<T>(&self) -> Result<T>
    where T: Decoder {
        T::from_encoded(self)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let test1 = vec![1, 2, 3, 4];

        let encoded1 = test1.encode().unwrap();
        let resut1: Vec<u8> = encoded1.decode().unwrap();
        assert_eq!(test1, resut1);

        let test1 = test1.as_slice();
        let encoded1 = test1.encode().unwrap();
        let resut1: Vec<u8> = encoded1.decode().unwrap();
        assert_eq!(test1, resut1);

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
}
