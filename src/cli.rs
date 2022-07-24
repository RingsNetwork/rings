use std::sync::Arc;

use jsonrpc_core::Params;
use jsonrpc_core::Value;
use serde_json::json;

use crate::jsonrpc::method::Method;
use crate::jsonrpc::response::Peer;
use crate::jsonrpc::response::TransportAndIce;
use crate::jsonrpc_client::SimpleClient;
use crate::prelude::reqwest;

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
    pub async fn new(endpoint_url: &str, signature: &str) -> anyhow::Result<Self> {
        let mut default_headers = reqwest::header::HeaderMap::default();
        default_headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(signature)?,
        );
        let client = SimpleClient::new(
            Arc::new(
                reqwest::Client::builder()
                    .default_headers(default_headers)
                    .build()?,
            ),
            endpoint_url,
        );
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

    pub async fn answer_offer(&mut self, ice_info: &str) -> Output<TransportAndIce> {
        let resp = self
            .client
            .call_method(
                Method::AnswerOffer.as_str(),
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

    pub async fn connect_with_address(&mut self, address: &str) -> Output<()> {
        self.client
            .call_method(
                Method::ConnectWithAddress.as_str(),
                Params::Array(vec![Value::String(address.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Successful!".to_owned(), ())
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
        display.push_str("Address, TransportId, Status\n");
        display.push_str(
            peers
                .iter()
                .map(|peer| format!("{}, {}, {}", peer.address, peer.transport_id, peer.state))
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

    pub async fn send_message(&self, address: &str, text: &str) -> Output<()> {
        let mut params = serde_json::Map::new();
        params.insert("destination".to_owned(), json!(address));
        params.insert("text".to_owned(), json!(text));
        self.client
            .call_method(Method::SendTo.as_str(), Params::Map(params))
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
