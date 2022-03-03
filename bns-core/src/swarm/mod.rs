/// Swarm is transport management
///

/// Once a Peer receive a `DHTMessage`, it can cause two situations:

/// 1. Found some did, and return with `DHTResponse`.

/// ```
/// #[derive(Deserialize, Serialize, Debug, Clone)]
/// pub struct DHTResponse {
///     act: RemoteAction,
///     req_sig: Vec<u8>,
///     sdp: Option<Encoded>,
/// }
/// ```

/// DHT response is for handling `ResponseMsg` From DHT Peer. It simply wrapped a `DHTAction` with addition info
/// such as `req_sig` and `sdp` for transactions management and handshake connection.

/// 3. Not Found, continue to send `DHTRelayMessage` to next peer.

/// ```
/// #[derive(Deserialize, Serialize, Debug, Clone)]
/// pub struct DHTRelayMessage {
///     act: RemoteAction,
///     req_sig: Vec<u8>,
///     from: vec<Address>
/// }

/// #[derive(Deserialize, Serialize, Debug, Clone)]
/// pub struct DHTRelayResponse {
///     act: DHTResponse,
///     from: Vec<Address>
/// }
/// ```

/// Consider Peer A send `DHTRelayMessage` `msg_a` with push `self.id` to `msg_a`.from to B,
/// then B send `DHTRelayMessage` `msg_b` to C and C got some value, it's returning
/// `ResponseMsg<RelayMsg>` resp_c to B

/// Once B received resp_c, B should send `ResponseMsg<RelayMsg>` to resp_c.act.from[-1],
/// Once B received resp_c, B should send `ResponseMsg<RelayMsg>` to resp_c.act.from[-2],
/// Once B received resp_c, B should send `ResponseMsg<RelayMsg>` to resp_c.act.from[-3],

/// So `DHTRelayMessage` has two associated method:

/// `record`: record self.id to relay chain.
/// `prev`: pop and get previous hop for msg routing back
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
use crate::dht::chord::Chord;
use crate::dht::chord::ChordAction;
use crate::dht::chord::RemoteAction;
use crate::ecc::SecretKey;
use crate::encoder::Encoded;
use crate::msg::SignedMsg;
use crate::storage::{MemStorage, Storage};
#[cfg(not(feature = "wasm"))]
use crate::transports::default::DefaultTransport as Transport;
#[cfg(feature = "wasm")]
use crate::transports::wasm::WasmTransport as Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use anyhow::Result;
use futures::lock::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use web3::types::Address;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DHTResponse {
    act: RemoteAction,
    req_sig: Vec<u8>,
    sdp: Option<Encoded>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DHTRelayResponse {
    act: DHTResponse,
    from: Vec<Address>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DHTRelayMessage {
    act: RemoteAction,
    req_sig: Vec<u8>,
    from: Vec<Address>,
}

impl DHTRelayMessage {
    pub fn new<F>(msg: SignedMsg<F>, act: RemoteAction) -> Self {
        Self {
            act,
            req_sig: msg.sig,
            from: Vec::new(),
        }
    }

    pub fn record(&mut self, did: Address) {
        self.from.push(did);
    }

    pub fn prev(&mut self) -> Option<Address> {
        self.from.pop()
    }
}

impl DHTResponse {
    pub fn new<F>(msg: SignedMsg<F>, act: RemoteAction) -> Self {
        Self {
            act,
            req_sig: msg.sig,
            sdp: None,
        }
    }

    pub fn with_sdp(&mut self, sdp: Encoded) {
        self.sdp = Some(sdp);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "data")]
pub enum Message {
    CustomMessage(String),
    DHTMessage(RemoteAction),
    DHTRelayMessage(DHTRelayMessage),
    DHTResponse(DHTResponse),
    DHTRelayResponse(DHTRelayResponse),
}

pub struct Swarm {
    pub table: MemStorage<Address, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
    pub dht: Arc<Mutex<Chord>>,
    pub key: SecretKey,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: String, key: SecretKey) -> Self {
        Self {
            table: MemStorage::<Address, Arc<Transport>>::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun,
            dht: Arc::new(Mutex::new(Chord::new(key.address().into()))),
            key,
        }
    }

    pub fn address(&self) -> Address {
        self.key.address()
    }

    pub async fn new_transport(&self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(self.signaler());
        ice_transport
            .start(self.stun_server.clone())
            .await?
            .apply_callback()
            .await?;
        let trans = Arc::new(ice_transport);
        Ok(Arc::clone(&trans))
    }

    pub async fn register(&self, address: Address, trans: Arc<Transport>) {
        let prev_trans = self.table.set(address, trans);
        if let Some(trans) = prev_trans {
            if let Err(e) = trans.close().await {
                log::error!("failed to close previous while registering {:?}", e);
            }
        }
        let mut dht = self.dht.lock().await;
        if let ChordAction::RemoteAction((addr, act)) = (*dht).join(address.into()) {
            if let Ok(msg) = SignedMsg::new(Message::DHTMessage(act), &self.key, None) {
                // should handle return
                if let Err(e) = self.send_message_without_dht(addr.into(), msg).await {
                    log::error!("failed on register: {:?}", e);
                }
            }
        }
    }

    pub fn get_transport(&self, address: Address) -> Option<Arc<Transport>> {
        self.table.get(address)
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
    }

    pub async fn send_message_without_dht(
        &self,
        address: Address,
        msg: SignedMsg<Message>,
    ) -> Result<()> {
        match self.get_transport(address) {
            Some(trans) => Ok(trans.send_message(msg).await?),
            None => Err(anyhow::anyhow!("cannot seek address in swarm table")),
        }
    }

    pub async fn event_handler(&self) {
        loop {
            match self.signaler.recv().await {
                Ok(ev) => match ev {
                    Events::ReceiveMsg(msg) => {
                        match serde_json::from_slice::<SignedMsg<Message>>(&msg) {
                            Ok(m) => {
                                if m.is_expired() || !m.verify() {
                                    log::error!("cannot verify msg or it's expired: {:?}", m);
                                } else {
                                    let _ = self.message_handler(m.to_owned()).await;
                                }
                            }
                            Err(_e) => {
                                log::error!("cant handle Msg {:?}", msg);
                            }
                        }
                    }
                    x => {
                        log::debug!("Receive {:?}", x)
                    }
                },
                Err(e) => {
                    log::error!("failed on handle event {:?}", e)
                }
            }
        }
    }

    pub async fn dht_action_handler(
        &self,
        msg: SignedMsg<Message>,
        action: RemoteAction,
    ) -> Option<(Address, Message)> {
        let msg_addr = msg.addr;
        match action {
            RemoteAction::FindSuccessor(id) => {
                let dht = self.dht.lock().await;
                match dht.find_successor(id) {
                    Ok(ChordAction::Some(p)) => {
                        let resp = DHTResponse::new(msg, RemoteAction::FindSuccessor(p));
                        Some((msg_addr, Message::DHTResponse(resp)))
                    }
                    Ok(ChordAction::RemoteAction((addr, act))) => {
                        let mut relay_msg = DHTRelayMessage::new(msg, act);
                        relay_msg.record(self.key.address());
                        Some((addr.into(), Message::DHTRelayMessage(relay_msg)))
                    }
                    _ => None,
                }
            }
            RemoteAction::Notify(did) => {
                let mut dht = self.dht.lock().await;
                dht.notify(did);
                None
            }
            RemoteAction::FindSuccessorForFix(did) => {
                let dht = self.dht.lock().await;
                match dht.find_successor(did) {
                    Ok(ChordAction::Some(p)) => {
                        let resp = DHTResponse::new(msg, RemoteAction::FindSuccessorForFix(p));
                        Some((msg_addr, Message::DHTResponse(resp)))
                    }
                    Ok(ChordAction::RemoteAction((addr, RemoteAction::FindSuccessor(did)))) => {
                        let mut relay_msg =
                            DHTRelayMessage::new(msg, RemoteAction::FindSuccessorForFix(did));
                        relay_msg.record(self.key.address());
                        Some((addr.into(), Message::DHTRelayMessage(relay_msg)))
                    }
                    _ => None,
                }
            }
            RemoteAction::CheckPredecessor => None,
        }
    }

    pub async fn message_handler(&self, message: SignedMsg<Message>) {
        match message.data.to_owned() {
            Message::CustomMessage(m) => {
                log::info!("got Msg {:?}", m);
            }
            Message::DHTMessage(action) => {
                if let Some((addr, msg)) = self.dht_action_handler(message, action).await {
                    if let Ok(m) = SignedMsg::new(msg, &self.key, None) {
                        if let Err(e) = self.send_message_without_dht(addr, m).await {
                            log::error!("{:?}", e);
                        }
                    }
                }
            }
            Message::DHTResponse(action) => {
                match action.act {
                    RemoteAction::FindSuccessor(did) => {
                        // should create a handshake to that did
                        let mut dht = self.dht.lock().await;
                        dht.successor = did;
                    }
                    RemoteAction::FindSuccessorForFix(did) => {
                        let mut dht = self.dht.lock().await;
                        let index = dht.fix_finger_index;
                        dht.finger[index as usize] = Some(did)
                    }
                    _ => {}
                }
            }
            Message::DHTRelayMessage(action) => {
                if let Some((addr, msg)) = self.dht_action_handler(message, action.act).await {
                    let msg = msg.clone();
                    let mut addr = addr;

                    let modified = match msg {
                        Message::DHTRelayMessage(ref m) => {
                            let mut m = m.clone();
                            m.from = action.from;
                            m.record(self.address());
                            Message::DHTRelayMessage(m)
                        }
                        Message::DHTResponse(ref m) => {
                            // [origin, next]
                            if action.from.len() > 1 {
                                let mut from = action.from.clone();
                                addr = from.pop().unwrap();
                                let m = DHTRelayResponse {
                                    act: m.clone(),
                                    from,
                                };
                                Message::DHTRelayResponse(m)
                            } else {
                                msg.clone()
                            }
                        }
                        _ => {
                            log::error!("should not happen here");
                            msg.clone()
                        }
                    };
                    if let Ok(m) = SignedMsg::new(modified, &self.key, None) {
                        if let Err(e) = self.send_message_without_dht(addr, m).await {
                            log::error!("{:?}", e);
                        }
                    }
                }
            }
            Message::DHTRelayResponse(action) => {
                if action.from.len() > 1 {
                    let mut act = action.clone();
                    let addr = act.from.pop().unwrap();
                    let msg = Message::DHTRelayResponse(act);
                    if let Ok(m) = SignedMsg::new(msg, &self.key, None) {
                        if let Err(e) = self.send_message_without_dht(addr, m).await {
                            log::error!("{:?}", e);
                        }
                    }
                } else {
                    let msg = Message::DHTResponse(action.act.clone());
                    let addr = action.from[0];
                    if let Ok(m) = SignedMsg::new(msg, &self.key, None) {
                        if let Err(e) = self.send_message_without_dht(addr, m).await {
                            log::error!("{:?}", e);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

    fn new_swarm() -> Swarm {
        let ch = Arc::new(Channel::new(1));
        let stun = String::from("stun:stun.l.google.com:19302");
        Swarm::new(ch, stun, SecretKey::random())
    }

    #[tokio::test]
    async fn swarm_new_transport() {
        let swarm = new_swarm();

        let transport = swarm.new_transport().await.unwrap();
        assert_eq!(
            transport.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
    }

    #[tokio::test]
    async fn test_swarm_register_and_get() {
        let swarm1 = new_swarm();
        let swarm2 = new_swarm();

        assert!(swarm1.get_transport(swarm2.address()).is_none());

        let transport0 = swarm1.new_transport().await.unwrap();
        let transport1 = transport0.clone();

        swarm1.register(swarm2.address(), transport1).await;

        let transport2 = swarm1.get_transport(swarm2.address()).unwrap();

        assert!(Arc::ptr_eq(&transport0, &transport2));
    }

    #[tokio::test]
    async fn test_swarm_will_close_previous_transport() {
        let swarm1 = new_swarm();
        let swarm2 = new_swarm();

        assert!(swarm1.get_transport(swarm2.address()).is_none());

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm1.new_transport().await.unwrap();

        swarm1.register(swarm2.address(), transport1.clone()).await;
        swarm1.register(swarm2.address(), transport2.clone()).await;

        assert_eq!(
            transport1.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
        assert_eq!(
            transport1
                .get_peer_connection()
                .await
                .unwrap()
                .connection_state(),
            RTCPeerConnectionState::Closed
        );

        assert_eq!(
            transport2.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
        assert_eq!(
            transport2
                .get_peer_connection()
                .await
                .unwrap()
                .connection_state(),
            RTCPeerConnectionState::New
        );
    }

    #[tokio::test]
    async fn test_swarm_event_handler() {}
}
