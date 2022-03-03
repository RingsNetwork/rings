use crate::did::Did;
use crate::types::channel::Events;
use anyhow::anyhow;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;

const PREDECESSOR_NOTIFY: [u8; 2] = [105, 31];

fn generate_message(msg_type: [u8; 2], message: &[u8]) -> Result<Message> {
    match msg_type {
        PREDECESSOR_NOTIFY => {
            let m: PredecessorNotify = serde_json::from_slice(message).map_err(|e| anyhow!(e))?;
            Ok(Message::PredecessorNotify(m))
        }
        _ => Ok(Message::None),
    }
}

fn genterate_event_data(msg_type: [u8; 2], mut message: Vec<u8>) -> Vec<u8> {
    log::info!("MsgType: {:?}, Message: {:?}", msg_type, message);
    message.reverse();
    message.push(msg_type[1]);
    message.push(msg_type[0]);
    message.reverse();
    return message;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredecessorNotify {
    pub request_id: u128,
    pub current: Did,
    pub successor: Did,
}

#[derive(Debug, Clone)]
pub enum Message {
    None,
    PredecessorNotify(PredecessorNotify),
}

impl TryFrom<Message> for Events {
    type Error = anyhow::Error;

    fn try_from(msg: Message) -> Result<Events> {
        match msg {
            Message::PredecessorNotify(m) => {
                let to_veced: Vec<u8> = serde_json::to_vec(&m).map_err(|e| anyhow!(e))?;
                let data = genterate_event_data(PREDECESSOR_NOTIFY, to_veced);
                return Ok(Events::SendMsg(data));
            }
            Message::None => Ok(Events::Null),
        }
    }
}

impl TryFrom<Events> for Message {
    type Error = anyhow::Error;

    fn try_from(event: Events) -> Result<Message> {
        match event {
            Events::SendMsg(send_msg) => {
                let buf: [u8; 2] = [send_msg[0], send_msg[1]];
                generate_message(buf, &send_msg[2..])
            }
            _ => {
                panic!("Not implement other situtations");
            }
        }
    }
}

impl From<PredecessorNotify> for Message {
    fn from(event: PredecessorNotify) -> Message {
        Message::PredecessorNotify(event)
    }
}
