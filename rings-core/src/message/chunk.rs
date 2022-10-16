use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunk<const MTU: usize> {
    pub chunk: [usize; 2],
    /// from serde_json::to_vec
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunks<const MTU: usize>(Vec<Chunk<MTU>>);

impl<const MTU: usize> IntoIterator for Chunks<MTU> {
    type Item = Chunk<MTU>;
    type IntoIter = std::vec::IntoIter<Chunk<MTU>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<const MTU: usize, T> From<&T> for Chunks<MTU>
where T: Iterator<Item = u8> + Clone
{
    fn from(bytes: &T) -> Self {
        let chunks: Vec<[u8; MTU]> = bytes.clone().array_chunks::<MTU>().collect();
        let chunks_len: usize = chunks.len();
        Self(
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, d)| Chunk {
                    chunk: [i, chunks_len],
                    data: d.to_vec(),
                })
                .collect::<Vec<Chunk<MTU>>>(),
        )
    }
}

impl<const MTU: usize> Chunk<MTU> {
    pub fn from_bytes(bytes: Vec<u8>) -> Vec<Self> {
        let chunks: Vec<&[u8; MTU]> = bytes.array_chunks::<MTU>().collect();
        let chunks_len: usize = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, d)| Chunk {
                chunk: [i, chunks_len],
                data: d.to_vec(),
            })
            .collect::<Vec<Chunk<MTU>>>()
    }
}

#[cfg(test)]
mod test {
    use core::iter::repeat;
    use std::collections::HashMap;

    use bytes::Bytes;

    use super::*;

    #[test]
    fn test_data_chunks() {
        let data: String = repeat("helloworld").take(1024).collect();
        let ret = Chunk::<32>::from_bytes(data.as_bytes().to_vec());
        assert_eq!(ret.len(), 10 * 1024 * 4 / 32 + 1);
        assert_eq!(ret[ret.len() - 1].chunk, [1280, 1281]);
    }
}
