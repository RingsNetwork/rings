//! UDP instance of the relay: the listener (client side) and the per-flow datagram relay
//! loops (server side, plus the client-side return path). UDP flows have no half-close.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::net::UdpSocket;

use super::inject_accepted;
use super::send_frame;
use super::Outbound;
use super::RelayTask;
use super::TransportSessions;
use super::UDP_BUF;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::spawn_detached;
use crate::extension::transport::Frame;
use crate::extension::transport::RELAY_IDLE_TIMEOUT;

impl TransportSessions {
    /// Bind a UDP socket; route each datagram. A datagram from a **known** source (in the
    /// effect-populated `udp_flows` cache) is forwarded directly (fast path, no `step`). A
    /// datagram from a **new** source stashes the first bytes and reports the accept to the
    /// pure relay (`Accepted`); the core mints the session id and replies with `OpenAccepted`
    /// → [`bind_accepted`](TransportSessions::bind_accepted), which opens the flow and
    /// forwards that first datagram. The listener picks no identity.
    pub(super) async fn listen_udp(
        self: Arc<Self>,
        scope: Scope,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
    ) {
        let socket = match UdpSocket::bind(local_addr).await {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                tracing::error!("transport udp bind {local_addr} failed: {e:?}");
                return;
            }
        };
        spawn_detached(async move {
            let mut buf = vec![0u8; UDP_BUF];
            loop {
                match socket.recv_from(buf.as_mut_slice()).await {
                    Ok((n, src)) => {
                        let Some(bytes) = received_bytes(&buf, n) else {
                            tracing::error!("UDP listener returned a length beyond its buffer");
                            break;
                        };
                        match self.udp_flow(&src) {
                            Some(key) => {
                                let _ = send_frame(&scope, key.peer, Frame::Data {
                                    session: key.session,
                                    from_opener: super::opened_by_us(&key),
                                    bytes,
                                })
                                .await;
                            }
                            None => {
                                if let Some(token) =
                                    self.reserve_pending_udp(socket.clone(), src, bytes)
                                {
                                    if inject_accepted(&scope, token, peer, service.clone())
                                        .await
                                        .is_err()
                                    {
                                        self.evict_pending(token);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("transport udp recv on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }
}

/// Server-side UDP flow: a per-flow socket connected to the backend.
pub(super) async fn relay_udp_connected(task: RelayTask, socket: UdpSocket) {
    relay_udp_connected_with_idle(task, socket, RELAY_IDLE_TIMEOUT).await;
}

async fn relay_udp_connected_with_idle(
    task: RelayTask,
    socket: UdpSocket,
    idle_timeout: std::time::Duration,
) {
    let RelayTask {
        sessions,
        scope,
        key,
        mut outbound_rx,
        cancel,
        generation,
    } = task;
    let peer = key.peer;
    let session = key.session;
    let from_opener = super::opened_by_us(&key);
    let mut buf = vec![0u8; UDP_BUF];
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            received = socket.recv(buf.as_mut_slice()) => match received {
                Ok(n) => {
                    idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                    let Some(bytes) = received_bytes(&buf, n) else {
                        break;
                    };
                    if send_frame(&scope, peer, Frame::Data {
                        session,
                        from_opener,
                        bytes,
                    })
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            },
            outbound = outbound_rx.recv() => match outbound {
                Some(Outbound::Data(bytes)) => {
                    idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                    let _ = socket.send(bytes.as_ref()).await;
                }
                Some(Outbound::Shutdown) | None => break,
            },
            _ = &mut idle => break,
        }
    }
    // Only tell the peer if we were still the current owner (stale task stays silent).
    if sessions.close_if_current(&scope, &key, generation).await {
        let _ = send_frame(&scope, peer, Frame::Close {
            session,
            from_opener,
        })
        .await;
    }
}

/// Client-side UDP flow: route peer bytes back to the originating local client `dest`.
pub(super) fn spawn_udp_sendto(task: RelayTask, socket: Arc<UdpSocket>, dest: SocketAddr) {
    spawn_detached(relay_udp_sendto(task, socket, dest, RELAY_IDLE_TIMEOUT));
}

async fn relay_udp_sendto(
    task: RelayTask,
    socket: Arc<UdpSocket>,
    dest: SocketAddr,
    idle_timeout: std::time::Duration,
) {
    let RelayTask {
        sessions,
        scope,
        key,
        mut outbound_rx,
        cancel,
        generation,
    } = task;
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            outbound = outbound_rx.recv() => match outbound {
                Some(Outbound::Data(bytes)) => {
                    idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                    let _ = socket.send_to(bytes.as_ref(), dest).await;
                }
                Some(Outbound::Shutdown) | None => break,
            },
            _ = &mut idle => break,
        }
    }
    // The return-path task owns the client-side UDP flow lifetime. An idle timeout therefore
    // retires both the live session and its `udp_flows` projection; a stale generation stays
    // silent and cannot close a replacement (the same ABA rule as every other relay task).
    if sessions.close_if_current(&scope, &key, generation).await {
        let _ = send_frame(&scope, key.peer, Frame::Close {
            session: key.session,
            from_opener: super::opened_by_us(&key),
        })
        .await;
    }
}

/// Bind an ephemeral UDP socket and connect it to `addr`.
pub(super) async fn bind_connected_udp(addr: SocketAddr) -> Option<UdpSocket> {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().ok()?
    } else {
        "[::]:0".parse().ok()?
    };
    let socket = UdpSocket::bind(bind).await.ok()?;
    socket.connect(addr).await.ok()?;
    Some(socket)
}

fn received_bytes(buffer: &[u8], received: usize) -> Option<Bytes> {
    buffer.get(..received).map(Bytes::copy_from_slice)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::error::Error;
    use crate::error::Result;
    use crate::extension::transport::engine::relay_task_for_test;
    use crate::extension::transport::engine::relay_task_for_test_with_src;
    use crate::extension::transport::engine::UdpFlowState;

    #[tokio::test]
    async fn idle_connected_udp_relay_releases_its_session() -> Result<()> {
        let peer = UdpSocket::bind("127.0.0.1:0").await.map_err(io_error)?;
        let socket = UdpSocket::bind("127.0.0.1:0").await.map_err(io_error)?;
        socket
            .connect(peer.local_addr().map_err(io_error)?)
            .await
            .map_err(io_error)?;
        let (task, sessions, key) = relay_task_for_test("udp")?;

        tokio::time::timeout(
            Duration::from_secs(1),
            relay_udp_connected_with_idle(task, socket, Duration::from_millis(20)),
        )
        .await
        .map_err(|_| Error::ExtensionError("idle UDP relay was not reclaimed".to_string()))?;
        assert!(!sessions.is_live(&key));
        Ok(())
    }

    #[tokio::test]
    async fn idle_udp_return_path_reclaims_session_and_flow_projection() -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind UDP"));
        let src = "127.0.0.1:19001".parse().expect("UDP source");
        let (task, sessions, key) = relay_task_for_test_with_src("udp", Some(src))?;
        sessions
            .udp_flows
            .lock()
            .map_err(|_| Error::Lock)?
            .insert(src, UdpFlowState::Active(key.clone()));

        tokio::time::timeout(
            Duration::from_secs(1),
            relay_udp_sendto(task, socket, src, Duration::from_millis(20)),
        )
        .await
        .expect("idle UDP return path must terminate");
        assert!(!sessions.is_live(&key));
        assert!(!sessions
            .udp_flows
            .lock()
            .map_err(|_| Error::Lock)?
            .contains_key(&src));
        Ok(())
    }

    #[test]
    fn received_datagram_rejects_length_beyond_buffer() {
        assert_eq!(
            received_bytes(&[1, 2], 2),
            Some(Bytes::from_static(&[1, 2]))
        );
        assert_eq!(received_bytes(&[1, 2], 3), None);
    }

    fn io_error(error: std::io::Error) -> Error {
        Error::ExtensionError(format!("UDP relay test IO failed: {error}"))
    }
}
