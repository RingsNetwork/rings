pub(in crate::swarm::transport) use crate::message::MessageClass as TransferClass;
pub(in crate::swarm::transport) use crate::message::MessageMeta as OutboundMessageMeta;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum OutboundCompletion {
    Detached,
    Tracked,
}
