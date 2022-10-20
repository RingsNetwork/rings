#![warn(missing_docs)]
//! A Framing and Message chucking implementation
//! defined in RFC4917(https://www.rfc-editor.org/rfc/rfc4975#page-9)
//! This chunking mechanism allows a sender to interrupt a chunk part of
//! the way through sending it.  The ability to interrupt messages allows
//! multiple sessions to share a TCP connection, and for large messages
//! to be sent efficiently while not blocking other messages that share
//! the same connection, or even the same MSRP session.

use itertools::Itertools;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// A data structure to presenting Chunks
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunk<const MTU: usize> {
    /// chunk info, [position, total chunks]
    pub chunk: [usize; 2],
    /// bytes
    pub data: Vec<u8>,
    /// meta data of chunk
    pub meta: ChunkMeta,
}

impl<const MTU: usize> Chunk<MTU> {
    /// check two chunks is belongs to same tx
    pub fn tx_eq(a: &Self, b: &Self) -> bool {
        a.meta.id == b.meta.id && a.chunk[1] == b.chunk[1]
    }
}

/// Meta data of a chunk
#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub struct ChunkMeta {
    /// uuid of msg
    pub id: uuid::Uuid,
    /// Time to live
    pub ttl: Option<usize>,
    /// ts
    pub ts: Option<u128>,
}

impl Default for ChunkMeta {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            ts: None,
            ttl: None,
        }
    }
}

/// A helper for manage chunks and chunk pool
pub trait ChunkManager {
    /// list completed Chunks;
    fn list_completed(&self) -> Vec<Uuid>;
    /// list pending Chunks;
    fn list_pending(&self) -> Vec<Uuid>;
    /// get sepc msg via uuid
    /// if a msg is not completed, it will returns None
    fn get(&self, id: Uuid) -> Option<Vec<u8>>;
    /// delete
    fn remove(&mut self, id: Uuid);
}

/// List of Chunk, simply wrapped `Vec<Chunk>`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChunkList<const MTU: usize>(Vec<Chunk<MTU>>);

impl<const MTU: usize> ChunkList<MTU> {
    /// ChunkList to Vec
    pub fn to_vec(&self) -> Vec<Chunk<MTU>> {
        self.0.clone()
    }

    /// ChunkList to &Vec
    pub fn as_vec(&self) -> &Vec<Chunk<MTU>> {
        &self.0
    }

    /// ChunkList to &mut Vec
    pub fn as_vec_mut(&mut self) -> &mut Vec<Chunk<MTU>> {
        &mut self.0
    }

    /// dedup and sort elements in list
    pub fn formalize(&self) -> Self {
        let mut chunks = self.to_vec();
        // dedup same chunk id
        chunks.dedup_by_key(|c| c.chunk[0]);
        chunks.sort_by_key(|a| a.chunk[0]);
        Self::from(chunks)
    }

    /// search and formalize
    pub fn search(&self, id: Uuid) -> Self {
        let chunks: Vec<Chunk<MTU>> = self
            .to_vec()
            .iter()
            .filter(|e| e.meta.id == id)
            .cloned()
            .collect();
        Self::from(chunks).formalize()
    }

    /// check list that is completed
    pub fn is_completed(&self) -> bool {
        let chunks = self.formalize().to_vec();
        // sample first ele, and chunk size is equal to length of grouped vec
        // we can call `unwrap` here because pre-condition is `lens() > 0 `
        !chunks.is_empty() && chunks.len() == chunks.first().unwrap().chunk[1]
    }

    /// if list is completed, withdraw data, or return None
    pub fn try_withdraw(&self) -> Option<Vec<u8>> {
        if !self.is_completed() {
            None
        } else {
            let data = self.formalize().to_vec();
            let ret: Vec<u8> = data.iter().fold(vec![], |a, b| {
                let mut rhs = b.data.clone();
                let mut lhs = a;
                lhs.append(&mut rhs);
                lhs
            });
            Some(ret)
        }
    }
}

impl<const MTU: usize> Default for ChunkList<MTU> {
    fn default() -> Self {
        Self(vec![])
    }
}

impl<const MTU: usize> IntoIterator for ChunkList<MTU> {
    type Item = Chunk<MTU>;
    type IntoIter = std::vec::IntoIter<Chunk<MTU>>;

    fn into_iter(self) -> Self::IntoIter {
        self.to_vec().into_iter()
    }
}

impl<const MTU: usize, T> From<&T> for ChunkList<MTU>
where T: IntoIterator<Item = u8> + Clone
{
    fn from(bytes: &T) -> Self {
        let chunks: Vec<Vec<u8>> = bytes
            .clone()
            .into_iter()
            .chunks(MTU)
            .into_iter()
            .map(|x| x.into_iter().collect::<Vec<_>>())
            .collect();
        let chunks_len: usize = chunks.len();
        let meta = ChunkMeta::default();
        Self(
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, d)| Chunk {
                    meta,
                    chunk: [i, chunks_len],
                    data: d.to_vec(),
                })
                .collect::<Vec<Chunk<MTU>>>(),
        )
    }
}

impl<const MTU: usize> From<ChunkList<MTU>> for Vec<Chunk<MTU>> {
    fn from(l: ChunkList<MTU>) -> Self {
        l.to_vec()
    }
}

impl<const MTU: usize> From<Vec<Chunk<MTU>>> for ChunkList<MTU> {
    fn from(data: Vec<Chunk<MTU>>) -> Self {
        Self(data)
    }
}

impl<const MTU: usize> ChunkManager for ChunkList<MTU> {
    fn list_completed(&self) -> Vec<Uuid> {
        // group by msg uuid and chunk size
        self.to_vec()
            .group_by(|a, b| Chunk::tx_eq(a, b))
            .filter(|e| ChunkList::<MTU>::from(e.to_vec()).is_completed())
            .map(|c| c.first().unwrap().meta.id)
            .collect()
    }

    fn list_pending(&self) -> Vec<Uuid> {
        self.to_vec()
            .group_by(|a, b| Chunk::tx_eq(a, b))
            .filter(|e| !ChunkList::<MTU>::from(e.to_vec()).is_completed())
            .map(|c| c.first().unwrap().meta.id)
            .collect()
    }

    fn get(&self, id: Uuid) -> Option<Vec<u8>> {
        self.search(id).try_withdraw()
    }

    fn remove(&mut self, id: Uuid) {
        //  remove all elements e for where chunk.meta.id == id.
        self.as_vec_mut().retain(|e| e.meta.id != id)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_data_chunks() {
        let data = "helloworld".repeat(2);
        let ret: Vec<Chunk<32>> = ChunkList::<32>::from(&data.bytes()).into();
        assert_eq!(ret.len(), 1);
        assert_eq!(ret[ret.len() - 1].chunk, [0, 1]);

        let data = "helloworld".repeat(1024);
        let ret: Vec<Chunk<32>> = ChunkList::<32>::from(&data.bytes()).into();
        assert_eq!(ret.len(), 10 * 1024 / 32);
        assert_eq!(ret[ret.len() - 1].chunk, [319, 320]);
    }

    #[test]
    fn test_withdraw() {
        let data = "helloworld".repeat(1024);
        let ret: Vec<Chunk<32>> = ChunkList::<32>::from(&data.bytes()).into();
        let incomp = ret[0..30].to_vec();
        let cl = ChunkList::from(incomp);
        assert!(!cl.is_completed());
        let wd = ChunkList::from(ret).try_withdraw().unwrap();
        assert_eq!(wd, data.into_bytes())
    }

    #[test]
    fn test_query_complete() {
        let data1 = "hello".repeat(1024);
        let data2 = "world".repeat(256);
        let chunks1: Vec<Chunk<32>> = ChunkList::<32>::from(&data1.bytes()).into();
        let chunks2: Vec<Chunk<32>> = ChunkList::<32>::from(&data2.bytes()).into();

        let mut part = chunks1[2..5].to_vec();
        let mut fin = chunks2;
        fin.append(&mut part);

        let cl = ChunkList::from(fin);
        let comp = cl.list_completed();
        assert_eq!(comp.len(), 1);
        let id = comp[0];
        assert_eq!(cl.get(id).unwrap(), data2.into_bytes());
        let pend = cl.list_pending();
        assert_eq!(pend.len(), 1);
        assert_eq!(cl.get(pend[0]), None)
    }
}
