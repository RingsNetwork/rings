//! This module contains the [Pool] struct.

use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::connection_ref::ConnectionRef;
use crate::core::transport::ConnectionInterface;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::core::transport::ConnectionStateSnapshot;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::PlatformSendSync;

/// [Pool] manages all the connections for each peer.
pub struct Pool<C> {
    connections: DashMap<String, Arc<C>>,
}

enum PoolInsertion<C> {
    Inserted {
        connection: ConnectionRef<C>,
        retired: Option<Arc<C>>,
    },
    Rejected {
        candidate: Arc<C>,
        error: Error,
    },
}

impl<C> Default for Pool<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Pool<C> {
    /// Create a new [Pool] instance.
    pub fn new() -> Self {
        Self {
            connections: DashMap::new(),
        }
    }

    /// Get a reference of the connection by its id.
    pub fn connection(&self, cid: &str) -> Result<ConnectionRef<C>> {
        self.connections
            .get(cid)
            .map(|c| ConnectionRef::new(cid, c.value()))
            .ok_or(Error::ConnectionNotFound(cid.to_string()))
    }

    /// Get all the connections in the pool.
    pub fn connections(&self) -> Vec<(String, ConnectionRef<C>)> {
        self.connections
            .iter()
            .map(|kv| (kv.key().clone(), ConnectionRef::new(kv.key(), kv.value())))
            .collect()
    }

    /// Get all the connection ids in the pool.
    pub fn connection_ids(&self) -> Vec<String> {
        self.connections.iter().map(|kv| kv.key().clone()).collect()
    }

    fn insert_if_replaceable(
        &self,
        cid: &str,
        conn: C,
        current_is_live: impl FnOnce(&C) -> bool,
    ) -> PoolInsertion<C> {
        let connection = Arc::new(conn);
        let retired = match self.connections.entry(cid.to_string()) {
            Entry::Occupied(mut entry) => {
                if current_is_live(entry.get()) {
                    return PoolInsertion::Rejected {
                        candidate: connection,
                        error: Error::ConnectionAlreadyExists(cid.to_string()),
                    };
                }
                Some(entry.insert(Arc::clone(&connection)))
            }
            Entry::Vacant(entry) => {
                entry.insert(Arc::clone(&connection));
                None
            }
        };
        PoolInsertion::Inserted {
            connection: ConnectionRef::new(cid, &connection),
            retired,
        }
    }

    fn take_if_current(&self, expected: &ConnectionRef<C>) -> Option<Arc<C>> {
        match self.connections.entry(expected.cid().to_string()) {
            Entry::Occupied(entry) if expected.points_to(entry.get()) => Some(entry.remove()),
            _ => None,
        }
    }
}

impl<C, S> Pool<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S> + PlatformSendSync,
    S: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Reject an early connection setup while the peer already occupies its slot.
    pub(crate) fn ensure_peer_slot_available(&self, cid: &str) -> Result<()> {
        if self
            .connection(cid)
            .is_ok_and(|connection| connection.webrtc_connection_state().occupies_peer_slot())
        {
            Err(Error::ConnectionAlreadyExists(cid.to_string()))
        } else {
            Ok(())
        }
    }

    /// The `safely_insert` method is used to insert a connection into the pool.
    /// It ensures that the connection is not inserted twice in concurrent scenarios.
    ///
    /// The implementation of match statement refers to `Entry::insert` in dashmap.
    /// An extra check is added to see if the connection is already connected.
    /// See also: <https://docs.rs/dashmap/latest/dashmap/mapref/entry/enum.Entry.html#method.insert>
    pub async fn safely_insert(&self, cid: &str, conn: C) -> Result<ConnectionRef<C>> {
        let insertion = self.insert_if_replaceable(cid, conn, |current| {
            current.webrtc_connection_state().occupies_peer_slot()
        });
        match insertion {
            PoolInsertion::Inserted {
                connection,
                retired,
            } => {
                if let Some(retired) = retired {
                    if let Err(error) = retired.close().await {
                        tracing::warn!(
                            connection_id = cid,
                            error = ?error,
                            "failed to close replaced transport connection"
                        );
                    }
                }
                Ok(connection)
            }
            PoolInsertion::Rejected { candidate, error } => {
                if let Err(close_error) = candidate.close().await {
                    tracing::warn!(
                        connection_id = cid,
                        error = ?close_error,
                        "failed to close rejected transport connection"
                    );
                }
                Err(error)
            }
        }
    }

    /// This method closes and releases the connection from pool.
    /// All references to this cid, created by `get_connection`, will be released.
    /// The [ConnectionInterface] methods of them will return [Error::ConnectionReleased].
    pub async fn safely_remove(&self, cid: &str) -> Result<()> {
        let Some((_, conn)) = self.connections.remove(cid) else {
            return Err(Error::ConnectionNotFound(cid.to_string()));
        };
        conn.close().await
    }

    /// Remove and close `expected` only while it still owns its pool slot.
    pub async fn safely_remove_if_current(&self, expected: &ConnectionRef<C>) -> Result<bool> {
        let Some(conn) = self.take_if_current(expected) else {
            return Ok(false);
        };
        conn.close().await?;
        Ok(true)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::core::transport::SendPermit;
    use crate::core::transport::TransportMessage;
    use crate::delivery::DeliveryFuture;

    struct TestConnection {
        state: Mutex<WebrtcConnectionState>,
        closed: Arc<AtomicBool>,
    }

    impl TestConnection {
        fn new(state: WebrtcConnectionState, closed: Arc<AtomicBool>) -> Self {
            Self {
                state: Mutex::new(state),
                closed,
            }
        }

        fn set_state(&self, state: WebrtcConnectionState) -> Result<()> {
            *self
                .state
                .lock()
                .map_err(|_| Error::DataChannelOpen("test state lock poisoned".to_string()))? =
                state;
            Ok(())
        }
    }

    #[async_trait]
    impl ConnectionInterface for TestConnection {
        type Sdp = String;
        type Error = Error;

        async fn send_message(&self, _: TransportMessage) -> Result<DeliveryFuture> {
            Err(Error::DataChannelOpen(
                "test connection cannot send".to_string(),
            ))
        }

        async fn send_message_with_permit(
            &self,
            _: TransportMessage,
            _: SendPermit,
        ) -> Result<DeliveryFuture> {
            Err(Error::DataChannelOpen(
                "test connection cannot send".to_string(),
            ))
        }

        fn webrtc_connection_state(&self) -> WebrtcConnectionState {
            self.state
                .lock()
                .map(|state| *state)
                .unwrap_or(WebrtcConnectionState::Failed)
        }

        fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
            ConnectionStateSnapshot::new(self.webrtc_connection_state(), false)
        }

        fn data_channel_is_open(&self) -> Result<bool> {
            Ok(false)
        }

        fn max_message_size(&self) -> usize {
            1
        }

        async fn get_stats(&self) -> Vec<String> {
            Vec::new()
        }

        async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
            Err(Error::DataChannelOpen(
                "test connection cannot create offers".to_string(),
            ))
        }

        async fn webrtc_answer_offer(&self, _: Self::Sdp) -> Result<Self::Sdp> {
            Err(Error::DataChannelOpen(
                "test connection cannot answer offers".to_string(),
            ))
        }

        async fn webrtc_accept_answer(&self, _: Self::Sdp) -> Result<()> {
            Err(Error::DataChannelOpen(
                "test connection cannot accept answers".to_string(),
            ))
        }

        async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
            Err(Error::DataChannelOpen(
                "test connection has no data channel".to_string(),
            ))
        }

        async fn close(&self) -> Result<()> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn safely_insert_returns_the_exact_inserted_connection() -> Result<()> {
        let pool = Pool::new();
        let old_closed = Arc::new(AtomicBool::new(false));
        let old = pool
            .safely_insert(
                "peer",
                TestConnection::new(WebrtcConnectionState::New, Arc::clone(&old_closed)),
            )
            .await?;
        old.upgrade()?.set_state(WebrtcConnectionState::Failed)?;

        let replacement_closed = Arc::new(AtomicBool::new(false));
        let replacement = pool
            .safely_insert(
                "peer",
                TestConnection::new(WebrtcConnectionState::New, Arc::clone(&replacement_closed)),
            )
            .await?;

        assert!(!pool.safely_remove_if_current(&old).await?);
        assert!(old_closed.load(Ordering::SeqCst));
        assert!(pool.safely_remove_if_current(&replacement).await?);
        assert!(replacement_closed.load(Ordering::SeqCst));
        assert!(matches!(
            pool.connection("peer"),
            Err(Error::ConnectionNotFound(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn safely_insert_closes_rejected_connection() -> Result<()> {
        let pool = Pool::new();
        let current_closed = Arc::new(AtomicBool::new(false));
        let current = pool
            .safely_insert(
                "peer",
                TestConnection::new(
                    WebrtcConnectionState::Connected,
                    Arc::clone(&current_closed),
                ),
            )
            .await?;

        let rejected_closed = Arc::new(AtomicBool::new(false));
        let result = pool
            .safely_insert(
                "peer",
                TestConnection::new(WebrtcConnectionState::New, Arc::clone(&rejected_closed)),
            )
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("live connection must retain its pool slot"),
        };

        assert!(matches!(error, Error::ConnectionAlreadyExists(_)));
        assert!(rejected_closed.load(Ordering::SeqCst));
        assert!(!current_closed.load(Ordering::SeqCst));
        assert!(pool.safely_remove_if_current(&current).await?);
        assert!(current_closed.load(Ordering::SeqCst));
        Ok(())
    }

    #[test]
    fn current_removal_waits_for_pool_shard_contention() -> std::io::Result<()> {
        let pool = Arc::new(Pool::new());
        let expected = futures::executor::block_on(pool.safely_insert(
            "peer",
            TestConnection::new(WebrtcConnectionState::New, Arc::new(AtomicBool::new(false))),
        ))
        .map_err(std::io::Error::other)?;
        let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let lock_pool = Arc::clone(&pool);
        let lock_thread = std::thread::spawn(move || {
            let _entry = lock_pool.connections.get_mut("peer");
            let _ = locked_tx.send(());
            let _ = release_rx.recv();
        });
        locked_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(std::io::Error::other)?;

        let (removed_tx, removed_rx) = std::sync::mpsc::channel();
        let remove_pool = Arc::clone(&pool);
        let remove_thread = std::thread::spawn(move || {
            let result =
                futures::executor::block_on(remove_pool.safely_remove_if_current(&expected));
            let _ = removed_tx.send(result);
        });
        assert!(removed_rx.recv_timeout(Duration::from_millis(25)).is_err());

        release_tx.send(()).map_err(std::io::Error::other)?;
        lock_thread
            .join()
            .map_err(|_| std::io::Error::other("pool lock thread panicked"))?;
        let removed = removed_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)?;
        remove_thread
            .join()
            .map_err(|_| std::io::Error::other("pool remove thread panicked"))?;

        assert!(removed);
        assert!(matches!(
            pool.connection("peer"),
            Err(Error::ConnectionNotFound(_))
        ));
        Ok(())
    }
}
