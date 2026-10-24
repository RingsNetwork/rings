//! The DNS shell: answers queries for the relayed zones over UDP and TCP.
//!
//! `hickory-server` owns the sockets, message framing and truncation; this module turns one
//! request into the pure [`answer`] and sends it:
//!
//! ```text
//!   request ─▶ place(qname) ─▶ Owned? ─▶ directory.announcement(o) ─▶ registry.address_for(o)
//!                  │                              │ Err                        │ Err
//!                  │                              ▼                            ▼
//!                  │                          SERVFAIL                     SERVFAIL
//!                  ▼
//!            View ─▶ answer(view, qname, qtype) ─▶ response
//! ```
//!
//! Only standard queries with one question are answered; any other opcode gets `NOTIMP`, and a
//! request without exactly one question gets `FORMERR`. EDNS is answered as `hickory-server`'s
//! own catalog answers it: version 0, the client's payload size but at least 512.

use std::sync::Arc;

use async_trait::async_trait;
use hickory_proto::op::Edns;
use hickory_proto::op::Metadata;
use hickory_proto::op::OpCode;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::Name;
use hickory_server::net::runtime::Time;
use hickory_server::server::Request;
use hickory_server::server::RequestHandler;
use hickory_server::server::ResponseHandler;
use hickory_server::server::ResponseInfo;
use hickory_server::zone_handler::MessageResponseBuilder;

use crate::authority::answer;
use crate::authority::Announced;
use crate::authority::Answer;
use crate::authority::AnswerTtl;
use crate::authority::View;
use crate::capability::Directory;
use crate::owner::Owner;
use crate::registry::AddressRegistry;
use crate::state::now_ms;
use crate::zone::Placement;
use crate::zone::Zones;

/// The EDNS version the relay speaks.
const EDNS_VERSION: u8 = 0;
/// The smallest UDP payload every DNS client accepts (RFC 1035).
const MIN_UDP_PAYLOAD: u16 = 512;

/// The relay's DNS request handler: its zones, TTLs, address registry and directory.
pub struct DnsAuthority<D>
where D: Directory
{
    zones: Arc<Zones>,
    ttl: AnswerTtl,
    registry: Arc<AddressRegistry<D::Owner>>,
    directory: Arc<D>,
}

impl<D> DnsAuthority<D>
where D: Directory
{
    /// A handler answering for `zones` with `ttl`, reading owners from `directory` and binding
    /// their addresses in `registry`.
    pub fn new(
        zones: Arc<Zones>,
        ttl: AnswerTtl,
        registry: Arc<AddressRegistry<D::Owner>>,
        directory: Arc<D>,
    ) -> Self {
        Self {
            zones,
            ttl,
            registry,
            directory,
        }
    }

    /// The answer to the standard query `request` (one question): its name placed, its owner
    /// resolved, and the pure [`answer`] taken.
    async fn answer_query(&self, request: &Request) -> Result<Answer, ResponseCode> {
        let info = request.request_info().map_err(|_| ResponseCode::FormErr)?;
        let qname = Name::from(info.query.name());
        let qtype = info.query.query_type();
        let answer_in = |view: View<'_>| answer(view, &qname, qtype, self.ttl);
        match self.zones.place::<D::Owner>(info.query.name()) {
            Placement::Foreign => Ok(answer_in(View::Foreign)),
            Placement::Apex(zone) => Ok(answer_in(View::Apex(zone))),
            Placement::Absent(zone) => Ok(answer_in(View::Absent(zone))),
            Placement::Owned { zone, owner, sub } => {
                let announcement = self.directory.announcement(owner).await.map_err(|error| {
                    tracing::warn!("dns relay: {error}");
                    ResponseCode::ServFail
                })?;
                let Some(announcement) = announcement else {
                    return Ok(answer_in(View::Owned {
                        zone,
                        sub: &sub,
                        announced: None,
                    }));
                };
                let address =
                    self.registry
                        .address_for(owner, now_ms())
                        .await
                        .map_err(|error| {
                            tracing::warn!("dns relay: no address for {}: {error}", owner.label());
                            ResponseCode::ServFail
                        })?;
                Ok(answer_in(View::Owned {
                    zone,
                    sub: &sub,
                    announced: Some(Announced {
                        announcement: &announcement,
                        address,
                    }),
                }))
            }
        }
    }
}

/// The response EDNS for a request carrying `request`, as the module doc says; `Err` with it
/// when the request asks for a later EDNS version.
fn response_edns(request: &Edns) -> Result<Edns, Edns> {
    let mut edns = Edns::new();
    edns.set_max_payload(request.max_payload().max(MIN_UDP_PAYLOAD));
    edns.set_version(EDNS_VERSION);
    if request.version() > EDNS_VERSION {
        return Err(edns);
    }
    Ok(edns)
}

#[async_trait]
impl<D> RequestHandler for DnsAuthority<D>
where D: Directory
{
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        let edns = request.edns.as_ref().map(response_edns);
        let mut builder = MessageResponseBuilder::from_message_request(request);
        let outcome = match &edns {
            Some(Err(edns)) => {
                builder.edns(edns);
                Err(ResponseCode::BADVERS)
            }
            Some(Ok(edns)) => {
                builder.edns(edns);
                Ok(())
            }
            None => Ok(()),
        }
        .and_then(|()| match request.metadata.op_code {
            OpCode::Query => Ok(()),
            _ => Err(ResponseCode::NotImp),
        });
        let answer = match outcome {
            Ok(()) => self.answer_query(request).await,
            Err(code) => Err(code),
        };
        let sent = match &answer {
            Ok(answer) => {
                let mut metadata = Metadata::response_from_request(&request.metadata);
                metadata.response_code = answer.code;
                metadata.authoritative = answer.authoritative;
                metadata.recursion_available = false;
                let response = builder.build(
                    metadata,
                    answer.answers.iter(),
                    answer.authority.iter(),
                    std::iter::empty(),
                    std::iter::empty(),
                );
                response_handle.send_response(response).await
            }
            Err(code) => {
                let response = builder.error_msg(&request.metadata, *code);
                response_handle.send_response(response).await
            }
        };
        match sent {
            Ok(info) => info,
            Err(error) => {
                tracing::warn!("dns relay: response not sent: {error}");
                let mut metadata = Metadata::response_from_request(&request.metadata);
                metadata.response_code = ResponseCode::ServFail;
                ResponseInfo::from(hickory_proto::op::Header {
                    metadata,
                    counts: hickory_proto::op::HeaderCounts::default(),
                })
            }
        }
    }
}
