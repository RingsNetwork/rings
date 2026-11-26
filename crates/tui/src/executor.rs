//! Asynchronous command executor and terminal input stream adapter.
//!
//! This module isolates effectful RPC execution, wall-clock duration measurement, and blocking
//! terminal input reads from the pure [`crate::app::App`] state machine.

use std::time::Instant;

use crate::action::CommandOutcome;
use crate::action::DispatchedCommand;
use crate::action::DispatchedOutcome;
use crate::action::NodeCommand;
use crate::client::NodeClient;

/// Execute a [`DispatchedCommand`] against `client` and wrap the result in a [`DispatchedOutcome`]
/// carrying the originating [`crate::action::RequestMeta`].
pub async fn execute_dispatched_command(
    client: &dyn NodeClient,
    dispatched: DispatchedCommand,
) -> DispatchedOutcome {
    let outcome = execute_node_command(client, dispatched.command).await;
    DispatchedOutcome {
        meta: dispatched.meta,
        outcome,
    }
}

/// Execute a single [`NodeCommand`] against `client` and package its [`CommandOutcome`].
pub async fn execute_node_command(client: &dyn NodeClient, cmd: NodeCommand) -> CommandOutcome {
    let started = Instant::now();
    let command_name = cmd.title();

    match cmd {
        NodeCommand::RefreshSnapshot {
            include_expired_directory,
        } => match client.fetch_snapshot(include_expired_directory).await {
            Ok(snap) => CommandOutcome::Snapshot(Box::new(snap)),
            Err(err) => CommandOutcome::SnapshotError {
                include_expired_directory,
                error: err.to_string(),
            },
        },
        NodeCommand::ConnectPeerViaHttp { url, api_token } => {
            match client.connect_peer_via_http(&url, api_token).await {
                Ok(did) => CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!("Connected to {url} -> Peer DID: {did}"),
                    output: Some(format!("Remote Peer DID: {did}\nEndpoint: {url}")),
                    measurement: None,
                    dht_lookup: None,
                    registered_service: None,
                    duration_ms: elapsed_ms(started),
                },
                Err(err) => CommandOutcome::CommandError {
                    command: command_name,
                    error: err.to_string(),
                    duration_ms: elapsed_ms(started),
                },
            }
        }
        NodeCommand::ConnectWithDid { did } => match client.connect_with_did(&did).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Dialed peer DID {did} via DHT"),
                output: None,
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::ConnectWithSeed { source } => match client.connect_with_seed(&source).await {
            Ok(count) => {
                let display_source = crate::client::describe_seed_source(&source);
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!(
                        "Connected seed ({count} bootstrap peer(s)) from {display_source}"
                    ),
                    output: None,
                    measurement: None,
                    dht_lookup: None,
                    registered_service: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::Disconnect { did } => match client.disconnect(&did).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Disconnected peer {did}"),
                output: None,
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::CreateOffer { did } => match client.create_offer(&did).await {
            Ok(offer) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Created WebRTC offer for {did} ({} chars)", offer.len()),
                output: Some(format!(
                    "=== WebRTC Offer for {did} ===\n{offer}\n\nNext step: pass this offer to peer's Answer Offer ([A])."
                )),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::AnswerOffer { offer } => match client.answer_offer(&offer).await {
            Ok(answer) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Generated handshake reply ({} chars)", answer.len()),
                output: Some(format!(
                    "=== WebRTC Answer / Reply Payload ===\n{answer}\n\nNext step: return this payload to offerer's Accept Answer ([Y])."
                )),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::AcceptAnswer { answer } => match client.accept_answer(&answer).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: "Accepted WebRTC answer; manual handshake completed!".to_string(),
                output: Some("Manual WebRTC handshake completed.".to_string()),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::SendBackendMessage {
            destination_did,
            namespace,
            payload,
        } => match client
            .send_backend_message(&destination_did, &namespace, &payload)
            .await
        {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!(
                    "Sent {} bytes to {destination_did} [ns: {namespace}]",
                    payload.len()
                ),
                output: Some(format!(
                    "To: {destination_did}\nNamespace: {namespace}\nPayload: {payload}"
                )),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::PublishTopic { topic, message } => {
            match client.publish_topic_message(&topic, &message).await {
                Ok(()) => CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!("Published message to topic '{topic}'"),
                    output: Some(format!("Topic: {topic}\nPublished: {message}")),
                    measurement: None,
                    dht_lookup: None,
                    registered_service: None,
                    duration_ms: elapsed_ms(started),
                },
                Err(err) => CommandOutcome::CommandError {
                    command: command_name,
                    error: err.to_string(),
                    duration_ms: elapsed_ms(started),
                },
            }
        }
        NodeCommand::FetchTopic { topic, skip } => {
            match client.fetch_topic_messages(&topic, skip).await {
                Ok(messages) => {
                    let body = if messages.is_empty() {
                        format!("Topic '{topic}' (skip={skip}): <no messages>")
                    } else {
                        let lines = messages
                            .iter()
                            .enumerate()
                            .map(|(idx, m)| format!("[{}] {}", skip.saturating_add(idx as i64), m))
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("=== Topic '{topic}' ({} messages) ===\n{lines}", messages.len())
                    };
                    CommandOutcome::CommandSuccess {
                        command: command_name,
                        summary: format!(
                            "Fetched {} message(s) from topic '{topic}' (skip={skip})",
                            messages.len()
                        ),
                        output: Some(body),
                        measurement: None,
                        dht_lookup: None,
                        registered_service: None,
                        duration_ms: elapsed_ms(started),
                    }
                }
                Err(err) => CommandOutcome::CommandError {
                    command: command_name,
                    error: err.to_string(),
                    duration_ms: elapsed_ms(started),
                },
            }
        }
        NodeCommand::RegisterService { name } => match client.register_service(&name).await {
            Ok(()) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("Registered local service '{name}' in DHT"),
                output: Some(format!("Service registered: {name}")),
                measurement: None,
                dht_lookup: None,
                registered_service: Some(name),
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::LookupService { name } => match client.lookup_service(&name).await {
            Ok(dids) => {
                let body = if dids.is_empty() {
                    format!("Service '{name}': no provider DIDs found")
                } else {
                    format!(
                        "=== Providers for Service '{name}' ({}) ===\n{}",
                        dids.len(),
                        dids.join("\n")
                    )
                };
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!("Service '{name}' resolved to {} provider(s)", dids.len()),
                    output: Some(body),
                    measurement: None,
                    dht_lookup: None,
                    registered_service: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::DhtLookupKey { key } => match client.dht_lookup_key(&key).await {
            Ok(report) => {
                let sources_str = if report.sources.is_empty() {
                    report.status.label().to_string()
                } else {
                    report.sources.join(" + ")
                };
                let elems_body = if report.elements.is_empty() {
                    "<no elements stored>".to_string()
                } else {
                    report
                        .elements
                        .iter()
                        .enumerate()
                        .map(|(idx, el)| format!("[{idx}] {el}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let warn_line = report
                    .warning
                    .as_ref()
                    .map(|w| format!("\nWarning: {w}"))
                    .unwrap_or_default();
                let body = format!(
                    "=== DHT Lookup Report ===\nQuery Key: {}\nMode: {}\nEntry Key DID (VNode): {}\nStatus: {} ({})\nKind: {} ({} elements){}\n\n{}",
                    report.query_key,
                    report.query_mode,
                    report.target_did,
                    report.status.label(),
                    sources_str,
                    report.record_kind,
                    report.elements.len(),
                    warn_line,
                    elems_body
                );
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!(
                        "DHT Key '{}' -> {} ({}: {} elements, [{}])",
                        report.query_key,
                        report.target_did,
                        report.status.label(),
                        report.elements.len(),
                        sources_str
                    ),
                    output: Some(body),
                    measurement: None,
                    dht_lookup: Some(Box::new(report)),
                    registered_service: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
        NodeCommand::InspectPeer { did } => match client.peer_measurement(&did).await {
            Ok(Some(meas)) => {
                let credit_line = match &meas.credit {
                    Some(c) => format!(
                        "Score: {:.2} | Bytes TX: {} | Bytes RX: {} | Last Seen: {}s",
                        c.score, c.bytes_sent_to_peer, c.bytes_received_from_peer, c.last_seen_seconds
                    ),
                    None => "No persistent byte credit record".to_string(),
                };
                let details = format!(
                    "=== Peer Measurement: {} ===\nReliability: {:?}\nConnected: {} | Disconnected: {}\nSent: {} (failed: {}) | Received: {} (failed: {})\n{}",
                    meas.did,
                    meas.reliability,
                    meas.counters.connected,
                    meas.counters.disconnected,
                    meas.counters.sent,
                    meas.counters.failed_to_send,
                    meas.counters.received,
                    meas.counters.failed_to_receive,
                    credit_line
                );
                CommandOutcome::CommandSuccess {
                    command: command_name,
                    summary: format!(
                        "Peer {did}: {:?}, sent={}, recv={}",
                        meas.reliability, meas.counters.sent, meas.counters.received
                    ),
                    output: Some(details),
                    measurement: Some(Box::new(meas)),
                    dht_lookup: None,
                    registered_service: None,
                    duration_ms: elapsed_ms(started),
                }
            }
            Ok(None) => CommandOutcome::CommandSuccess {
                command: command_name,
                summary: format!("No measurement record found for peer {did}"),
                output: Some(format!("No local measurement record for {did}")),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: elapsed_ms(started),
            },
            Err(err) => CommandOutcome::CommandError {
                command: command_name,
                error: err.to_string(),
                duration_ms: elapsed_ms(started),
            },
        },
    }
}

/// Return elapsed milliseconds since `started`, saturating at `u64::MAX`.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
