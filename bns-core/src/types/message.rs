use crate::types::channel::Channel;
use crate::types::ice_transport::IceTransport;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use web3::types::Address;

pub type SendMessage<M> = (Address, M);

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TransportManager<E: Send, EvCh: Channel<E>> {
    type Transport: IceTransport<E, EvCh>;

    async fn new_transport(&self) -> Result<Arc<Self::Transport>>;
    async fn register(&self, address: &Address, trans: Arc<Self::Transport>) -> Result<()>;
    fn get_transport(&self, address: &Address) -> Option<Arc<Self::Transport>>;
    async fn get_or_register(
        &self,
        address: &Address,
        default: Arc<Self::Transport>,
    ) -> Result<Arc<Self::Transport>>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageHandler<
    T: Sync,
    E: Send,
    M: Send,
    EvCh: Channel<E>,
    MsgCh: Channel<SendMessage<M>>,
>
{
    fn new(
        dht: T,
        transport_manager: &dyn TransportManager<E, EvCh, Transport = impl IceTransport<E, EvCh>>,
        message_sender: MsgCh::Sender,
    ) -> Self
    where
        Self: Sized;

    async fn handle(&self, message: M);
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageHub<E: Send, M: Send, EvCh: Channel<E>, MsgCh: Channel<SendMessage<M>>> {
    fn new(
        transport_manager: &dyn TransportManager<E, EvCh, Transport = impl IceTransport<E, EvCh>>,
        event_receiver: EvCh::Receiver,
        message_receiver: MsgCh::Receiver,
    ) -> Self;

    async fn handle_message_io<T: Sync>(
        &self,
        message_handler: &dyn MessageHandler<T, E, M, EvCh, MsgCh>,
    );
}
