#![warn(missing_docs)]
//! This module provides helper function for handle DHT related Actions

use async_recursion::async_recursion;

use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Result;
use crate::message::types::FindSuccessorSend;
use crate::message::types::Message;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;

/// This accept a action instance, handler_func and error_msg string as parameter.
/// This macro is used for handling `PeerRingAction::MultiActions`.
///
/// It accepts three parameters:
/// * `$actions`: This parameter represents the actions that will be processed.
/// * `$handler_func`: This is the handler function that will be used to process each action. It is expected to be
///                    an expression that evaluates to a closure. The closure should be an asynchronous function
///                    which accepts a single action and returns a `Result<MessageHandlerEvent>`.
///                    The function will be called for each action in `$actions`, and should handle the action appropriately.
///
/// * `$error_msg`: This is a string that will be used as the error message if the handler function returns an error.
///                 The string should include one set of braces `{}` that will be filled with the `Debug` representation
///                 of the error returned from the handler function.
///
/// The macro returns a `Result<Vec<MessageHandlerEvent>>`. If all actions are processed successfully, it returns
/// `Ok(Vec<MessageHandlerEvent>)`, where the vector includes all the successful results from the handler function.
/// If any action fails, an error message will be logged, but the error will not be returned from the macro; instead,
/// it will continue with the next action.
///
/// The macro is asynchronous, so it should be used within an `async` context.
#[macro_export]
macro_rules! handle_multi_actions {
    ($actions:expr, $handler_func:expr, $error_msg:expr) => {{
        let ret: Vec<MessageHandlerEvent> =
            futures::future::join_all($actions.iter().map($handler_func))
                .await
                .iter()
                .map(|x| {
                    if x.is_err() {
                        tracing::error!($error_msg, x)
                    };
                    x
                })
                .filter_map(|x| x.as_ref().ok())
                .flat_map(|xs| xs.iter())
                .cloned()
                .collect();
        Ok(ret)
    }};
}

/// Handler of join dht event from PeerRing DHT.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub async fn handle_dht_events(
    act: &PeerRingAction,
    ctx: &MessagePayload,
) -> Result<Vec<MessageHandlerEvent>> {
    match act {
        PeerRingAction::None => Ok(vec![]),
        // Ask next hop to find successor for did,
        // if there is only two nodes A, B, it may cause loop, for example
        // A's successor is B, B ask A to find successor for B
        // A may send message to it's successor, which is B
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did)) => {
            if next != did {
                Ok(vec![MessageHandlerEvent::SendDirectMessage(
                    Message::FindSuccessorSend(FindSuccessorSend {
                        did: *did,
                        strict: false,
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                    }),
                    *next,
                )])
            } else {
                Ok(vec![])
            }
        }
        // A new successor is set, request the new successor for it's successor list
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
            Ok(vec![MessageHandlerEvent::SendDirectMessage(
                Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(*next)),
                *next,
            )])
        }
        PeerRingAction::RemoteAction(did, PeerRingRemoteAction::TryConnect) => {
            Ok(vec![MessageHandlerEvent::ConnectVia(
                *did,
                ctx.relay.origin_sender(),
            )])
        }
        PeerRingAction::RemoteAction(did, PeerRingRemoteAction::Notify(target_id)) => {
            if did == target_id {
                tracing::warn!("Did is equal to target_id, may implement wrong.");
                return Ok(vec![]);
            }
            Ok(vec![MessageHandlerEvent::Notify(*target_id)])
        }
        PeerRingAction::MultiActions(acts) => {
            handle_multi_actions!(
                acts,
                |act| async { handle_dht_events(act, ctx).await },
                "Failed on handle multi actions: {:#?}"
            )
        }
        _ => unreachable!(),
    }
}
