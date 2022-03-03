use crate::did::Did;
use crate::types::channel::Events;
use anyhow::anyhow;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;

const PREDECESSOR_NOTIFY: [u8; 2] = [105, 31];

fn match_messsage_route(msg_type: [u8; 2], message: &[u8]) -> Result<Message> {
    match msg_type {
        PREDECESSOR_NOTIFY => {
            let m: PredecessorNotify = serde_json::from_slice(message).map_err(|e| anyhow!(e))?;
            Ok(Message::PredecessorNotify(m))
        }
        _ => Ok(Message::None),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PredecessorNotify {
    pub did: Did,
}

#[derive(Debug)]
pub enum Message {
    None,
    PredecessorNotify(PredecessorNotify),
}

impl TryFrom<Message> for Events {
    type Error = anyhow::Error;

    fn try_from(msg: Message) -> Result<Events> {
        match msg {
            Message::PredecessorNotify(m) => {
                let mut to_veced: Vec<u8> = serde_json::to_vec(&m).map_err(|e| anyhow!(e))?;
                to_veced.reverse();
                to_veced.push(PREDECESSOR_NOTIFY[1]);
                to_veced.push(PREDECESSOR_NOTIFY[0]);
                to_veced.reverse();
                return Ok(Events::SendMsg(to_veced));
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
                match_messsage_route(buf, &send_msg[2..])
            }
            _ => {
                panic!("Not implement other situtations");
            }
        }
    }
}
