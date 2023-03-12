use std::sync::Arc;

use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use futures::SinkExt;
use futures::StreamExt;

use super::WsState;
use crate::jsonrpc::response::CustomBackendMessage;

/// Actual websocket statemachine (one will be spawned per connection)
pub async fn handle_socket(ws_state: Arc<WsState>, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();

    let mut send_task = tokio::spawn(async move {
        loop {
            if let Ok(data) = ws_state.receiver.lock().await.recv().await {
                if let Ok(data) = serde_json::to_value(&CustomBackendMessage::from(data)) {
                    if let Ok(data) = serde_json::to_string(&data) {
                        sender.send(Message::Text(data)).await;
                    }
                }
            }
            // tokio::time::sleep(time::Duration::new(100, 0)).await;
        }
    });
    let mut recv_task = tokio::spawn(async move {
        let mut cnt = 0;
        while let Some(Ok(msg)) = receiver.next().await {
            cnt += 1;
            // print message and break if instructed to do so
            tracing::debug!("recv message: {:?}", msg);
        }
        cnt
    });

    tokio::select! {
       rv_a = (&mut send_task) => {
            match rv_a {
                Ok(a) => tracing::debug!("{:?} messages sent", a),
                Err(a) => tracing::error!("WS Error sending messages {:?}", a)
            }
            recv_task.abort();
        },
        rv_b = (&mut recv_task) => {
            match rv_b {
                Ok(b) => tracing::debug!("Received {:?} messages", b),
                Err(b) => tracing::error!("Error receiving messages {:?}", b)
            }
            send_task.abort();
        }
    }
    tracing::info!("WS over");
}
