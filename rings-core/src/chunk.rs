#![warn(missing_docs)]
//! A Framing and Message chucking implementation
//! defined in RFC4917(https://www.rfc-editor.org/rfc/rfc4975#page-9)
//! This chunking mechanism allows a sender to interrupt a chunk part of
//! the way through sending it.  The ability to interrupt messages allows
//! multiple sessions to share a TCP connection, and for large messages
//! to be sent efficiently while not blocking other messages that share
//! the same connection, or even the same MSRP session.

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::consts::DEFAULT_TTL_MS;
use crate::err::Error;
use crate::err::Result;
use crate::utils::get_epoch_ms;

/// A data structure to presenting Chunks
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunk<const MTU: usize> {
    /// chunk info, [position, total chunks]
    pub chunk: [usize; 2],
    /// bytes
    pub data: Bytes,
    /// meta data of chunk
    pub meta: ChunkMeta,
}

impl<const MTU: usize> Chunk<MTU> {
    /// check two chunks is belongs to same tx
    pub fn tx_eq(a: &Self, b: &Self) -> bool {
        a.meta.id == b.meta.id && a.chunk[1] == b.chunk[1]
    }

    /// serelize chunk to bytes
    pub fn to_bincode(&self) -> Result<Bytes> {
        bincode::serialize(self)
            .map(Bytes::from)
            .map_err(Error::BincodeSerialize)
    }

    /// deserialize bytes to chunk
    pub fn from_bincode(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data).map_err(Error::BincodeDeserialize)
    }
}

/// Meta data of a chunk
#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub struct ChunkMeta {
    /// uuid of msg
    pub id: uuid::Uuid,
    /// Created time
    pub ts_ms: u128,
    /// Time to live
    pub ttl_ms: usize,
}

impl Default for ChunkMeta {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            ts_ms: get_epoch_ms(),
            ttl_ms: DEFAULT_TTL_MS,
        }
    }
}

/// A helper for manage chunks and chunk pool
pub trait ChunkManager<const MTU: usize> {
    /// list completed Chunks;
    fn list_completed(&self) -> Vec<Uuid>;
    /// list pending Chunks;
    fn list_pending(&self) -> Vec<Uuid>;
    /// get sepc msg via uuid
    /// if a msg is not completed, it will returns None
    fn get(&self, id: Uuid) -> Option<Bytes>;
    ///  remove all chunks of id
    fn remove(&mut self, id: Uuid);
    /// remove expired chunks by ttl
    fn remove_expired(&mut self);
    /// handle a chunk
    fn handle(&mut self, chunk: Chunk<MTU>) -> Option<Bytes>;
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
    pub fn try_withdraw(&self) -> Option<Bytes> {
        if !self.is_completed() {
            None
        } else {
            let data = self.formalize().to_vec();
            let ret = data.into_iter().flat_map(|c| c.data).collect();
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

impl<const MTU: usize> From<&Bytes> for ChunkList<MTU> {
    fn from(bytes: &Bytes) -> Self {
        let chunks: Vec<Bytes> = bytes.chunks(MTU).map(|c| c.to_vec().into()).collect();
        let chunks_len: usize = chunks.len();
        let meta = ChunkMeta::default();
        Self(
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, data)| Chunk {
                    meta,
                    chunk: [i, chunks_len],
                    data,
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

impl<const MTU: usize> ChunkManager<MTU> for ChunkList<MTU> {
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

    fn get(&self, id: Uuid) -> Option<Bytes> {
        self.search(id).try_withdraw()
    }

    fn remove(&mut self, id: Uuid) {
        self.as_vec_mut().retain(|e| e.meta.id != id)
    }

    fn remove_expired(&mut self) {
        let now = get_epoch_ms();
        self.as_vec_mut()
            .retain(|e| e.meta.ts_ms + e.meta.ttl_ms as u128 > now)
    }

    fn handle(&mut self, chunk: Chunk<MTU>) -> Option<Bytes> {
        self.as_vec_mut().push(chunk.clone());

        let id = chunk.meta.id;
        let data = self.get(id)?;

        self.remove(id);
        self.remove_expired();

        Some(data)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_data_chunks() {
        let data = "helloworld".repeat(2).into();
        let ret: Vec<Chunk<32>> = ChunkList::<32>::from(&data).into();
        assert_eq!(ret.len(), 1);
        assert_eq!(ret[ret.len() - 1].chunk, [0, 1]);

        let data = "helloworld".repeat(1024).into();
        let ret: Vec<Chunk<32>> = ChunkList::<32>::from(&data).into();
        assert_eq!(ret.len(), 10 * 1024 / 32);
        assert_eq!(ret[ret.len() - 1].chunk, [319, 320]);
    }

    #[test]
    fn test_withdraw() {
        let data = "helloworld".repeat(1024).into();
        let ret: Vec<Chunk<32>> = ChunkList::<32>::from(&data).into();
        let incomp = ret[0..30].to_vec();
        let cl = ChunkList::from(incomp);
        assert!(!cl.is_completed());
        let wd = ChunkList::from(ret).try_withdraw().unwrap();
        assert_eq!(wd, data);
    }

    #[test]
    fn test_query_complete() {
        let data1 = "hello".repeat(1024).into();
        let data2 = "world".repeat(256).into();
        let chunks1: Vec<Chunk<32>> = ChunkList::<32>::from(&data1).into();
        let chunks2: Vec<Chunk<32>> = ChunkList::<32>::from(&data2).into();

        let mut part = chunks1[2..5].to_vec();
        let mut fin = chunks2;
        fin.append(&mut part);

        let cl = ChunkList::from(fin);
        let comp = cl.list_completed();
        assert_eq!(comp.len(), 1);
        let id = comp[0];
        assert_eq!(cl.get(id).unwrap(), data2);
        let pend = cl.list_pending();
        assert_eq!(pend.len(), 1);
        assert_eq!(cl.get(pend[0]), None)
    }
}
