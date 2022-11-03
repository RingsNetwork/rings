//! An efficient command tool of using ring-node.
use std::sync::Arc;

use jsonrpc_core::Params;
use jsonrpc_core::Value;
use serde_json::json;

use crate::backend::ipfs::IpfsRequest;
use crate::backend::types::Timeout;
use crate::jsonrpc;
use crate::jsonrpc::method::Method;
use crate::jsonrpc::response::Peer;
use crate::jsonrpc::response::TransportAndIce;
use crate::jsonrpc_client::SimpleClient;
use crate::prelude::reqwest;
use crate::seed::Seed;
use crate::util::loader::ResourceLoader;

/// Alias about Result<ClientOutput<T>, E>.
type Output<T> = anyhow::Result<ClientOutput<T>>;

/// Wrap json_client send request between nodes or browers.
#[derive(Clone)]
pub struct Client {
    client: SimpleClient,
}

/// Wrap client output contain raw result and humanreadable dispaly.
pub struct ClientOutput<T> {
    pub result: T,
    display: String,
}

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

        let transport_id = resp
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Unexpect response"))?;

        ClientOutput::ok(
            format!("Succeed, Your transport_id: {}", transport_id),
            transport_id.to_string(),
        )
    }

    pub async fn connect_with_seed(&mut self, source: &str) -> Output<()> {
        let seed = Seed::load(source).await?;
        let seed_v = serde_json::to_value(seed).map_err(|_| anyhow::anyhow!("serialize failed"))?;

        self.client
            .call_method(
                Method::ConnectWithSeed.as_str(),
                Params::Array(vec![seed_v]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Successful!".to_string(), ())
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

    pub async fn connect_with_did(&mut self, did: &str) -> Output<()> {
        self.client
            .call_method(
                Method::ConnectWithDid.as_str(),
                Params::Array(vec![Value::String(did.to_owned())]),
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

    pub async fn list_peers(&mut self) -> Output<()> {
        let resp = self
            .client
            .call_method(Method::ListPeers.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let peers: Vec<Peer> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut display = String::new();
        display.push_str("Successful\n");
        display.push_str("Did, TransportId, Status\n");
        display.push_str(
            peers
                .iter()
                .map(|peer| format!("{}, {}, {}", peer.did, peer.transport_id, peer.state))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        );

        ClientOutput::ok(display, ())
    }

    pub async fn disconnect(&mut self, did: &str) -> Output<()> {
        self.client
            .call_method(Method::Disconnect.as_str(), Params::Array(vec![json!(did)]))
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
        let resp: Vec<jsonrpc::response::TransportInfo> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut display = String::new();
        display.push_str("Successful\n");
        display.push_str("TransportId, Status\n");
        for item in resp.iter() {
            display.push_str(format!("{}, {}", item.transport_id, item.state).as_str())
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

    pub async fn send_message(&self, did: &str, text: &str) -> Output<()> {
        let mut params = serde_json::Map::new();
        params.insert("destination".to_owned(), json!(did));
        params.insert("text".to_owned(), json!(text));
        self.client
            .call_method(Method::SendTo.as_str(), Params::Map(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    pub async fn send_ipfs_request(&self, did: &str, url: &str, timeout: Timeout) -> Output<()> {
        let ipfs_request: IpfsRequest = (url.to_owned(), timeout).into();
        let params2 = serde_json::to_value(ipfs_request).map_err(|e| anyhow::anyhow!(e))?;
        self.client
            .call_method(
                Method::SendTo.as_str(),
                Params::Array(vec![json!(did), params2]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    pub async fn send_simple_text_message(&self, did: &str, text: &str) -> Output<()> {
        self.client
            .call_method(
                Method::SendTo.as_str(),
                Params::Array(vec![json!(did), json!(text)]),
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
