use crate::{
    jsonrpc::{
        method::Method,
        response::{Peer, TransportAndIce},
    },
    jsonrpc_client::SimpleClient,
};
use jsonrpc_core::{Params, Value};
//use jsonrpc_core_client::RawClient;
use serde_json::json;

#[derive(Clone)]
pub struct Client {
    client: SimpleClient,
}

pub struct ClientOutput<T> {
    pub result: T,
    display: String,
}
type Output<T> = anyhow::Result<ClientOutput<T>>;

impl Client {
    pub async fn new(endpoint_url: &str) -> anyhow::Result<Self> {
        let client = SimpleClient::new_with_url(endpoint_url);
        Ok(Self { client })
    }

    pub async fn connect_peer_via_http(&mut self, http_url: &str) -> Output<String> {
        let resp = self
            .client
            .call_method(
                Method::ConnectPeerViaHttp.as_str(),
                Params::Array(vec![Value::String(http_url.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        log::debug!("resp: {:?}", resp);
        let transport_id = resp
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Unexpect response"))?;

        ClientOutput::ok(
            format!("Succeed, Your transport_id: {}", transport_id),
            transport_id.to_string(),
        )
    }

    pub async fn connect_peer_via_ice(&mut self, ice_info: &str) -> Output<TransportAndIce> {
        let resp = self
            .client
            .call_method(
                Method::ConnectPeerViaIce.as_str(),
                Params::Array(vec![Value::String(ice_info.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(
            format!(
                "Successful!\ntransport_id: {}\nice: {}",
                info.transport_id, info.ice,
            ),
            info,
        )
    }

    pub async fn create_offer(&mut self) -> Output<TransportAndIce> {
        let resp = self
            .client
            .call_method(Method::CreateOffer.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(
            format!(
                "Successful!\ntransport_id: {}\nice: {}",
                info.transport_id, info.ice
            ),
            info,
        )
    }

    pub async fn accept_answer(&mut self, transport_id: &str, ice: &str) -> Output<Peer> {
        let resp = self
            .client
            .call_method(
                Method::AcceptAnswer.as_str(),
                Params::Array(vec![json!(transport_id), json!(ice)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let peer: Peer = serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(
            format!("Successful, transport_id: {}", peer.transport_id),
            peer,
        )
    }

    pub async fn list_peers(&mut self) -> Output<Vec<Peer>> {
        let resp = self
            .client
            .call_method(Method::ListPeers.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let peers: Vec<Peer> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut display = String::new();
        display.push_str("Successful\n");
        display.push_str("Address, TransportId\n");
        display.push_str(
            peers
                .iter()
                .map(|peer| format!("{}, {}", peer.address, peer.transport_id))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        );

        ClientOutput::ok(display, peers)
    }

    pub async fn disconnect(&mut self, address: &str) -> Output<()> {
        self.client
            .call_method(
                Method::Disconnect.as_str(),
                Params::Array(vec![json!(address)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Done.".into(), ())
    }

    pub async fn list_pendings(&self) -> Output<()> {
        let resp = self
            .client
            .call_method(Method::ListPendings.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let resp: Vec<String> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut display = String::new();
        for item in resp.iter() {
            display.push_str(item)
        }
        ClientOutput::ok(display, ())
    }

    pub async fn close_pending_transport(&self, transport_id: &str) -> Output<()> {
        self.client
            .call_method(
                Method::ClosePendingTransport.as_str(),
                Params::Array(vec![json!(transport_id)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }
}

impl<T> ClientOutput<T> {
    // Put display ahead to avoid moved value error.
    pub fn ok(display: String, result: T) -> anyhow::Result<Self> {
        Ok(Self { result, display })
    }

    pub fn display(&self) {
        println!("{}", self.display);
    }
}
