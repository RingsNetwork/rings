use std::sync::Arc;

use rings_transport::connection_ref::ConnectionRef;
use rings_transport::core::callback::BoxedTransportCallback;
use rings_transport::core::transport::BoxedTransport;
use rings_transport::core::transport::ConnectionInterface;

pub struct Swarm<C, B, E> {
    transport: BoxedTransport<C, E>,
    backend: B,
    callback: Arc<BoxedTransportCallback>,
}

pub trait Backend {
    fn callback(&self) -> BoxedTransportCallback;
}

impl<C, B, E> Swarm<C, B, E>
where
    C: ConnectionInterface<Error = E>,
    B: Backend,
    E: std::error::Error,
{
    pub fn new(transport: BoxedTransport<C, E>, backend: B) -> Self {
        let callback = Arc::new(backend.callback());
        Self {
            transport,
            backend,
            callback,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub async fn new_connection(&self, cid: &str) -> Result<(), C::Error> {
        self.transport
            .new_connection(cid, self.callback.clone())
            .await
    }

    pub fn connection(&self, cid: &str) -> Result<ConnectionRef<C>, C::Error> {
        self.transport.connection(cid)
    }
}
