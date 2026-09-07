//! Rpc methods and their authorization classes.

use jsonrpc_core::Call;
use jsonrpc_core::Request;

use super::error::Error;
use super::error::Result;

/// supported methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Method {
    /// Connect peer with remote jsonrpc server url
    ConnectPeerViaHttp,
    /// Connect peer with remote peer's did
    ConnectWithDid,
    /// Connect peers from a seed file
    ConnectWithSeed,
    /// List all connected peers
    ListPeers,
    /// Create offer for manually handshake
    CreateOffer,
    /// Answer offer for manually handshake
    AnswerOffer,
    /// Accept Answer for manually handshake
    AcceptAnswer,
    /// Disconnect a peer
    Disconnect,
    /// SendBackendMessage
    SendBackendMessage,
    /// Send an E2E public-key handshake request
    SendE2eHandshake,
    /// Send an encrypted E2E message stream
    SendE2eMessage,
    /// Append data to topic
    PublishMessageToTopic,
    /// Fetch data of topic
    FetchTopicMessages,
    /// Register service
    RegisterService,
    /// Lookup service
    LookupService,
    /// Lookup online-node registry descriptors
    LookupOnlineNodes,
    /// Lookup application-layer onion exit descriptors
    LookupOnionExits,
    /// Build an onion route from live presence and exit descriptors
    BuildOnionRoute,
    /// Retrieve Node info
    NodeInfo,
    /// Retrieve local measurement counters for a peer
    PeerMeasurement,
    /// Retrieve local measurement counters for connected peers
    ListPeerMeasurements,
    /// Retrieve Node DID
    NodeDid,
}

impl Method {
    /// Return method's name as `&str`
    pub fn as_str(&self) -> &str {
        match self {
            Method::ConnectPeerViaHttp => "connectPeerViaHttp",
            Method::ConnectWithDid => "connectWithDid",
            Method::ConnectWithSeed => "connectWithSeed",
            Method::ListPeers => "listPeers",
            Method::CreateOffer => "createOffer",
            Method::AnswerOffer => "answerOffer",
            Method::Disconnect => "disconnect",
            Method::AcceptAnswer => "acceptAnswer",
            Method::SendBackendMessage => "sendBackendMessage",
            Method::SendE2eHandshake => "sendE2eHandshake",
            Method::SendE2eMessage => "sendE2eMessage",
            Method::PublishMessageToTopic => "publishMessageToTopic",
            Method::FetchTopicMessages => "fetchTopicMessages",
            Method::RegisterService => "registerService",
            Method::LookupService => "lookupService",
            Method::LookupOnlineNodes => "lookupOnlineNodes",
            Method::LookupOnionExits => "lookupOnionExits",
            Method::BuildOnionRoute => "buildOnionRoute",
            Method::NodeInfo => "nodeInfo",
            Method::PeerMeasurement => "peerMeasurement",
            Method::ListPeerMeasurements => "listPeerMeasurements",
            Method::NodeDid => "nodeDid",
        }
    }

    /// Return the authorization class a listener must demand before executing this method.
    ///
    /// Only the two halves of the HTTP handshake are `Public`: `nodeDid` reveals the DID the
    /// caller is about to address, and `answerOffer` admits a session whose offer already carries
    /// a signature bound to the peer DID, so neither is guarded by the listener credential. Every
    /// other method reads node status, reads a registry, or controls the node, and is `Gated`.
    pub fn authorization_class(&self) -> AuthorizationClass {
        match self {
            Method::NodeDid | Method::AnswerOffer => AuthorizationClass::Public,
            Method::ConnectPeerViaHttp
            | Method::ConnectWithDid
            | Method::ConnectWithSeed
            | Method::ListPeers
            | Method::CreateOffer
            | Method::AcceptAnswer
            | Method::Disconnect
            | Method::SendBackendMessage
            | Method::SendE2eHandshake
            | Method::SendE2eMessage
            | Method::PublishMessageToTopic
            | Method::FetchTopicMessages
            | Method::RegisterService
            | Method::LookupService
            | Method::LookupOnlineNodes
            | Method::LookupOnionExits
            | Method::BuildOnionRoute
            | Method::NodeInfo
            | Method::PeerMeasurement
            | Method::ListPeerMeasurements => AuthorizationClass::Gated,
        }
    }
}

/// The credential a listener must demand before executing a method.
///
/// The classes form the two-element chain `Public < Gated`, hence a join-semilattice with bottom
/// `⊥ = Public` and top `⊤ = Gated`; `Ord` is that order. A request is classified by the join
/// of its calls, so one gated call gates a whole batch, and a listener combines its own floor
/// with the request class by the same join, so a floor of `⊤` absorbs every method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AuthorizationClass {
    /// Executed for any caller that presents a well-formed request; the method's own protocol
    /// checks are its guard.
    Public,
    /// Executed only for a caller that presents the listener's Bearer credential.
    Gated,
}

impl AuthorizationClass {
    /// Return the least upper bound `self ⊔ other`.
    pub fn join(self, other: Self) -> Self {
        self.max(other)
    }

    /// Return whether a caller that did (`true`) or did not (`false`) present the listener
    /// credential satisfies this class: `⊥` is satisfied by every caller, `⊤` only by an
    /// authenticated one.
    pub fn satisfied_by(self, authenticated: bool) -> bool {
        match self {
            Self::Public => true,
            Self::Gated => authenticated,
        }
    }

    /// Return the class of one call.
    ///
    /// A call that names a known method takes that method's class. A call that cannot be
    /// classified, because it is malformed or names an unknown method, is `⊤`: nothing it could
    /// execute is known to be public.
    pub fn of_call(call: &Call) -> Self {
        let method = match call {
            Call::MethodCall(call) => call.method.as_str(),
            Call::Notification(notification) => notification.method.as_str(),
            Call::Invalid { .. } => return Self::Gated,
        };
        Method::try_from(method).map_or(Self::Gated, |method| method.authorization_class())
    }

    /// Return the class of a whole request: `⊔ { of_call(c) | c ∈ calls }`.
    ///
    /// The join over the empty batch is `⊥`, since such a request executes nothing.
    pub fn of_request(request: &Request) -> Self {
        match request {
            Request::Single(call) => Self::of_call(call),
            Request::Batch(calls) => calls
                .iter()
                .map(Self::of_call)
                .fold(Self::Public, Self::join),
        }
    }
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for Method {
    fn to_string(&self) -> String {
        self.as_str().to_owned()
    }
}

impl TryFrom<&str> for Method {
    type Error = crate::error::Error;

    fn try_from(value: &str) -> Result<Self> {
        Ok(match value {
            "connectPeerViaHttp" => Self::ConnectPeerViaHttp,
            "connectWithDid" => Self::ConnectWithDid,
            "connectWithSeed" => Self::ConnectWithSeed,
            "listPeers" => Self::ListPeers,
            "createOffer" => Self::CreateOffer,
            "answerOffer" => Self::AnswerOffer,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            "sendBackendMessage" => Self::SendBackendMessage,
            "sendE2eHandshake" => Self::SendE2eHandshake,
            "sendE2eMessage" => Self::SendE2eMessage,
            "publishMessageToTopic" => Method::PublishMessageToTopic,
            "fetchTopicMessages" => Method::FetchTopicMessages,
            "registerService" => Method::RegisterService,
            "lookupService" => Method::LookupService,
            "lookupOnlineNodes" => Method::LookupOnlineNodes,
            "lookupOnionExits" => Method::LookupOnionExits,
            "buildOnionRoute" => Method::BuildOnionRoute,
            "nodeInfo" => Method::NodeInfo,
            "peerMeasurement" => Method::PeerMeasurement,
            "listPeerMeasurements" => Method::ListPeerMeasurements,
            "nodeDid" => Method::NodeDid,
            _ => return Err(Error::InvalidMethod),
        })
    }
}

#[cfg(test)]
mod tests {
    use jsonrpc_core::Id;
    use jsonrpc_core::MethodCall;
    use jsonrpc_core::Params;
    use jsonrpc_core::Version;

    use super::AuthorizationClass;
    use super::Call;
    use super::Method;
    use super::Request;

    /// Every variant is named without a wildcard, so a new `Method` must be classified here
    /// before the crate compiles again.
    fn expected_class(method: &Method) -> AuthorizationClass {
        match method {
            Method::NodeDid | Method::AnswerOffer => AuthorizationClass::Public,
            Method::ConnectPeerViaHttp
            | Method::ConnectWithDid
            | Method::ConnectWithSeed
            | Method::ListPeers
            | Method::CreateOffer
            | Method::AcceptAnswer
            | Method::Disconnect
            | Method::SendBackendMessage
            | Method::SendE2eHandshake
            | Method::SendE2eMessage
            | Method::PublishMessageToTopic
            | Method::FetchTopicMessages
            | Method::RegisterService
            | Method::LookupService
            | Method::LookupOnlineNodes
            | Method::LookupOnionExits
            | Method::BuildOnionRoute
            | Method::NodeInfo
            | Method::PeerMeasurement
            | Method::ListPeerMeasurements => AuthorizationClass::Gated,
        }
    }

    const EVERY_METHOD: [Method; 22] = [
        Method::ConnectPeerViaHttp,
        Method::ConnectWithDid,
        Method::ConnectWithSeed,
        Method::ListPeers,
        Method::CreateOffer,
        Method::AnswerOffer,
        Method::AcceptAnswer,
        Method::Disconnect,
        Method::SendBackendMessage,
        Method::SendE2eHandshake,
        Method::SendE2eMessage,
        Method::PublishMessageToTopic,
        Method::FetchTopicMessages,
        Method::RegisterService,
        Method::LookupService,
        Method::LookupOnlineNodes,
        Method::LookupOnionExits,
        Method::BuildOnionRoute,
        Method::NodeInfo,
        Method::PeerMeasurement,
        Method::ListPeerMeasurements,
        Method::NodeDid,
    ];

    fn call(method: &str) -> Call {
        Call::MethodCall(MethodCall {
            jsonrpc: Some(Version::V2),
            method: method.to_string(),
            params: Params::None,
            id: Id::Num(1),
        })
    }

    #[test]
    fn only_the_handshake_methods_are_public() {
        for method in EVERY_METHOD {
            assert_eq!(
                method.authorization_class(),
                expected_class(&method),
                "{}",
                method.as_str()
            );
        }
    }

    #[test]
    fn classes_form_a_chain_with_public_at_the_bottom() {
        assert!(AuthorizationClass::Public < AuthorizationClass::Gated);
        assert!(AuthorizationClass::Public.satisfied_by(false));
        assert!(!AuthorizationClass::Gated.satisfied_by(false));
        assert!(AuthorizationClass::Gated.satisfied_by(true));
        assert_eq!(
            AuthorizationClass::Public.join(AuthorizationClass::Gated),
            AuthorizationClass::Gated
        );
        assert_eq!(
            AuthorizationClass::Public.join(AuthorizationClass::Public),
            AuthorizationClass::Public
        );
    }

    #[test]
    fn a_request_takes_the_join_of_its_calls() {
        let public = Request::Batch(vec![call("nodeDid"), call("answerOffer")]);
        let mixed = Request::Batch(vec![call("nodeDid"), call("nodeInfo")]);
        let unknown = Request::Single(call("notAMethod"));
        let invalid = Request::Single(Call::Invalid { id: Id::Null });
        let empty = Request::Batch(Vec::new());
        assert_eq!(
            AuthorizationClass::of_request(&public),
            AuthorizationClass::Public
        );
        assert_eq!(
            AuthorizationClass::of_request(&mixed),
            AuthorizationClass::Gated
        );
        assert_eq!(
            AuthorizationClass::of_request(&unknown),
            AuthorizationClass::Gated
        );
        assert_eq!(
            AuthorizationClass::of_request(&invalid),
            AuthorizationClass::Gated
        );
        assert_eq!(
            AuthorizationClass::of_request(&empty),
            AuthorizationClass::Public
        );
    }
}
