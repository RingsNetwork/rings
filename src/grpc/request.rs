use super::GrpcRequest;
use arrayref::array_ref;

pub const DEFAULT_VERSION: &str = "2.0";

#[derive(Debug, Clone)]
pub enum RpcMethod {
    ConnectWithUrl,
    ListPeers,
    CreateOffer,
    ConnectWithHandshakeInfo,
    SendTo,
    Disconnect,
    AcceptAnswer,
}

impl RpcMethod {
    pub fn build_request(&self, params: &[&[u8]]) -> GrpcRequest {
        GrpcRequest {
            version: DEFAULT_VERSION.to_owned(),
            id: 1,
            method: self.to_string(),
            params: params.iter().copied().map(|x| x.to_vec()).collect(),
        }
    }
}

impl ToString for RpcMethod {
    fn to_string(&self) -> String {
        match self {
            RpcMethod::ConnectWithUrl => "connectWithUrl",
            RpcMethod::ListPeers => "listPeers",
            RpcMethod::CreateOffer => "createOffer",
            RpcMethod::ConnectWithHandshakeInfo => "connectWithHandshakeInfo",
            RpcMethod::SendTo => "sendTo",
            RpcMethod::Disconnect => "disconnect",
            RpcMethod::AcceptAnswer => "acceptAnswer",
        }
        .to_owned()
    }
}

impl TryFrom<&str> for RpcMethod {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Ok(match value {
            "connectWithUrl" => Self::ConnectWithUrl,
            "listPeers" => Self::ListPeers,
            "createOffer" => Self::CreateOffer,
            "connectWithHandshakeInfo" => Self::ConnectWithHandshakeInfo,
            "sendTo" => Self::SendTo,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            _ => return Err(anyhow::anyhow!("Invalid method: {}", value)),
        })
    }
}

impl TryFrom<String> for RpcMethod {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct ConnectWithUrl {
    pub url: String,
}

impl ConnectWithUrl {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
        }
    }
}

impl TryFrom<&tonic::Request<GrpcRequest>> for ConnectWithUrl {
    type Error = anyhow::Error;

    fn try_from(value: &tonic::Request<GrpcRequest>) -> Result<Self, Self::Error> {
        if value.get_ref().params.len() != 1 {
            return Err(anyhow::anyhow!("InvalidArgument"));
        }
        let [url_arr] = array_ref![value.get_ref().params, 0, 1];
        Ok(ConnectWithUrl {
            url: String::from_utf8(url_arr.to_vec())?,
        })
    }
}

impl tonic::IntoRequest<GrpcRequest> for ConnectWithUrl {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(RpcMethod::ConnectWithUrl.build_request(&[self.url.as_bytes()]))
    }
}

#[derive(Debug, Clone)]
pub struct ConnectWithHandshakeInfo {
    pub handshake_info: String,
}

impl ConnectWithHandshakeInfo {
    pub fn new(handshake_info: &str) -> Self {
        Self {
            handshake_info: handshake_info.to_owned(),
        }
    }
}

impl TryFrom<&tonic::Request<GrpcRequest>> for ConnectWithHandshakeInfo {
    type Error = anyhow::Error;

    fn try_from(value: &tonic::Request<GrpcRequest>) -> Result<Self, Self::Error> {
        if value.get_ref().params.len() != 1 {
            return Err(anyhow::anyhow!("InvalidArgument"));
        }
        let [hs_info_arr] = array_ref![value.get_ref().params, 0, 1];
        Ok(ConnectWithHandshakeInfo {
            handshake_info: String::from_utf8(hs_info_arr.to_owned())?,
        })
    }
}

impl tonic::IntoRequest<GrpcRequest> for ConnectWithHandshakeInfo {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(
            RpcMethod::ConnectWithHandshakeInfo.build_request(&[self.handshake_info.as_bytes()]),
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct CreateOffer {}

impl tonic::IntoRequest<GrpcRequest> for CreateOffer {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(RpcMethod::CreateOffer.build_request(&[]))
    }
}

#[derive(Debug, Clone)]
pub struct ListPeers {
    pub all: bool,
}

impl ListPeers {
    pub fn new(all: bool) -> Self {
        Self { all }
    }
}

impl TryFrom<&tonic::Request<GrpcRequest>> for ListPeers {
    type Error = anyhow::Error;

    fn try_from(value: &tonic::Request<GrpcRequest>) -> Result<Self, Self::Error> {
        if value.get_ref().params.len() != 1 {
            return Err(anyhow::anyhow!("InvalidArgument"));
        }
        let [all_arr] = array_ref![value.get_ref().params, 0, 1];
        Ok(Self {
            all: all_arr[0] == 1,
        })
    }
}

impl tonic::IntoRequest<GrpcRequest> for ListPeers {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(RpcMethod::ListPeers.build_request(&[&[if self.all { 1 } else { 0 }]]))
    }
}

pub struct SendTo {
    pub to_address: String,
    pub text: String,
}

impl SendTo {
    pub fn new(to_address: &str, text: &str) -> Self {
        Self {
            to_address: to_address.to_owned(),
            text: text.to_owned(),
        }
    }
}

impl TryFrom<&tonic::Request<GrpcRequest>> for SendTo {
    type Error = anyhow::Error;

    fn try_from(value: &tonic::Request<GrpcRequest>) -> Result<Self, Self::Error> {
        if value.get_ref().params.len() < 2 {
            return Err(anyhow::anyhow!("InvalidArgument"));
        }
        let [to_address, text] = array_ref![value.get_ref().params, 0, 2];
        Ok(Self {
            to_address: String::from_utf8(to_address.to_vec())?,
            text: String::from_utf8(text.to_vec())?,
        })
    }
}

impl tonic::IntoRequest<GrpcRequest> for SendTo {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(
            RpcMethod::SendTo.build_request(&[self.to_address.as_bytes(), self.text.as_bytes()]),
        )
    }
}

#[derive(Clone, Debug)]
pub struct Disconnect {
    pub address: String,
}

impl TryFrom<&tonic::Request<GrpcRequest>> for Disconnect {
    type Error = anyhow::Error;

    fn try_from(value: &tonic::Request<GrpcRequest>) -> Result<Self, Self::Error> {
        if value.get_ref().params.is_empty() {
            return Err(anyhow::anyhow!("InvalidArgument"));
        }
        let [address] = array_ref![value.get_ref().params, 0, 1];
        Ok(Self {
            address: String::from_utf8(address.to_vec())?,
        })
    }
}

impl tonic::IntoRequest<GrpcRequest> for Disconnect {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(RpcMethod::Disconnect.build_request(&[self.address.as_bytes()]))
    }
}

#[derive(Clone, Debug)]
pub struct AcceptAnswer {
    pub transport_id: String,
    pub handshake_info: String,
}

impl AcceptAnswer {
    pub fn new(transport_id: &str, handshake_info: &str) -> Self {
        Self {
            transport_id: transport_id.to_owned(),
            handshake_info: handshake_info.to_owned(),
        }
    }
}

impl TryFrom<&tonic::Request<GrpcRequest>> for AcceptAnswer {
    type Error = anyhow::Error;

    fn try_from(value: &tonic::Request<GrpcRequest>) -> Result<Self, Self::Error> {
        if value.get_ref().params.len() < 2 {
            return Err(anyhow::anyhow!("InvalidArgument"));
        }
        let [transport_id, handshake_info] = array_ref![value.get_ref().params, 0, 2];
        Ok(Self {
            transport_id: String::from_utf8(transport_id.to_vec())?,
            handshake_info: String::from_utf8(handshake_info.to_vec())?,
        })
    }
}

impl tonic::IntoRequest<GrpcRequest> for AcceptAnswer {
    fn into_request(self) -> tonic::Request<GrpcRequest> {
        tonic::Request::new(
            RpcMethod::AcceptAnswer
                .build_request(&[self.transport_id.as_bytes(), self.handshake_info.as_bytes()]),
        )
    }
}
