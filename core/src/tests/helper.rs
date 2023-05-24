use std::collections::HashMap;
use std::pin::Pin;
use futures::future::BoxFuture;
use regex::Regex;

use crate::ecc::SecretKey;
use crate::message::new_payload;
use crate::message::Message;
use crate::message::MessagePayload;

pub type LogicTimestamp = u8;
#[derive(Default)]
pub struct NodeContext<'a>(HashMap<crate::dht::Did, &'a crate::message::MessageHandler>);
/// Checker is living as long as ref of MessagePayload
pub type Checker = Box<dyn Fn(MessagePayload<Message>, &TestContext) -> bool>;
pub type Operator = Box<dyn FnOnce(&NodeContext) -> BoxFuture<'static, ()>>;

fn get_message_type(m: &MessagePayload<Message>) -> String {
    let msg_dbg_name = format!("{:?}", m.data);
    let re = Regex::new(r"(\w+)\(").unwrap();
    let name = re.captures(&msg_dbg_name).unwrap().get(1).unwrap().as_str();
    name.to_string()
}

#[derive(Default)]
pub struct EventAssertion(Option<String>, Option<Checker>);

impl EventAssertion {
    fn new(event: Option<String>, checker: Option<Checker>) -> Self {
        Self(event, checker)
    }

    fn check_event(&self, msg: MessagePayload<Message>) -> bool {
        if let Some(ev) = &self.0 {
            ev == &get_message_type(&msg)
        } else {
            true
        }
    }

    fn check_checker(&mut self, msg: MessagePayload<Message>, ctx: &TestContext) -> bool
    {
        if let Some(func) = self.1.take() {
            let ret = func(msg, ctx);
            ret
        } else {
            true
        }
    }

    fn check(&mut self, msg: MessagePayload<Message>, ctx: &TestContext) -> bool
    {
        self.check_event(msg.clone()) && self.check_checker(msg.clone(), ctx)
    }
}

/// Assertion for Test Suit
#[derive(Default)]
pub struct Assertion {
    /// Logic Timestamp
    time: LogicTimestamp,
    /// Event list
    events: Vec<EventAssertion>,
    operator: Option<Operator>,
}

#[derive(Default)]
pub struct TestContext<'a> {
    ctx: NodeContext<'a>,
    asserts: Vec<Assertion>,
    index: u8,
}

impl<'a> TestContext<'a> {
    fn assert_push(&mut self, xs: Vec<Assertion>) -> () {
        for x in xs {
            self.asserts.push(x);
        }
    }

    fn next<'b>(&'b mut self) -> &'b Assertion
    where 'b: 'a {
        let ret = &self.asserts[self.index as usize];
        self.index += 1;
        ret
    }
}

#[tokio::test]
async fn test_message_type() -> () {
    let key = SecretKey::random();
    let addr = key.address();
    let message = Message::JoinDHT(crate::message::JoinDHT { did: addr.into() });
    let payload = new_payload(message, addr.into());
    let name = get_message_type(&payload);
    assert_eq!(name, "JoinDHT");
}

#[tokio::test]
async fn test_message_checker() -> ()
{
    let key = SecretKey::random();
    let addr = key.address();
    let message = Message::JoinDHT(crate::message::JoinDHT { did: addr.into() });
    let payload = new_payload(message, addr.into());
    let checker = Box::new(
	move |msg: MessagePayload<Message>, _ctx: & TestContext| {
	    if let Message::JoinDHT(ev) = &msg.data {
                ev.did == addr.into()
	    } else {
                false
	    }
	}
    );
    let ctx = TestContext::default();  // move ctx creation here
    let mut assertion = EventAssertion::new(None, Some(checker));
    assert!(assertion.check(payload, &ctx));
}
