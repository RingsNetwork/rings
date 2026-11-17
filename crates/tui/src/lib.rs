#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event;
use crossterm::event::KeyEventKind;
use crossterm::execute;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

/// Button commands, modal forms, and hit-test regions.
pub mod action;
/// Pure application state machine and command scheduler.
pub mod app;
/// Asynchronous JSON-RPC and HTTP status client for `rings-node`.
pub mod client;
/// CLI argument and YAML config / API token resolution.
pub mod config;
/// Error types used by `rings-tui`.
pub mod error;
/// Asynchronous command executor separating RPC side effects from `App`.
pub mod executor;
/// Status DTOs, combined node snapshots, and telemetry history series.
pub mod types;
/// Terminal rendering with `ratatui`.
pub mod ui;

use crate::action::CommandOutcome;
use crate::action::DispatchedOutcome;
use crate::action::RequestMeta;
use crate::app::App;
use crate::client::DemoNodeClient;
use crate::client::HttpNodeClient;
use crate::client::NodeClient;
use crate::config::ResolvedTuiConfig;
use crate::config::TuiArgs;
use crate::error::Result;
use crate::executor::execute_dispatched_command;

const INPUT_POLL_SLICE: Duration = Duration::from_millis(50);

/// Guard that restores the terminal out of raw mode and alternate screen on drop.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
        let _ = disable_raw_mode();
    }
}

/// Launch the interactive TUI using the supplied command-line arguments.
pub async fn run_from_args(args: TuiArgs) -> Result<()> {
    let print_preview = args.print_preview;
    let config = ResolvedTuiConfig::from_args(&args)?;
    if print_preview {
        let preview = render_preview_frames(config, 116, 32).await?;
        println!("{preview}");
        return Ok(());
    }
    run_with_config(config).await
}

/// Render all TUI tabs into a human-readable string using an in-memory buffer for previewing.
pub async fn render_preview_frames(
    config: ResolvedTuiConfig,
    width: u16,
    height: u16,
) -> Result<String> {
    use ratatui::backend::TestBackend;

    let mut app = App::new(config);
    let client: Arc<dyn NodeClient> = if app.config.demo {
        Arc::new(DemoNodeClient::new())
    } else {
        Arc::new(HttpNodeClient::from_config(&app.config)?)
    };

    // Drain the initial queued refresh command so `in_flight_commands` stays 0 in the preview.
    let _ = app.pop_pending_command();
    app.in_flight_commands = 0;

    for req_id in 1..=8_u64 {
        if let Ok(snap) = client.fetch_snapshot(false).await {
            app.apply_outcome(DispatchedOutcome {
                meta: RequestMeta {
                    client_epoch: app.client_epoch,
                    request_id: req_id,
                },
                outcome: CommandOutcome::Snapshot(Box::new(snap)),
            });
        }
    }
    if let Ok(report) = client.dht_lookup_key("rings-chat").await {
        app.last_dht_lookup = Some(report);
    }
    app.push_log(
        "connectPeerViaHttp",
        true,
        "Connected to http://127.0.0.1:50001 -> Peer DID: 0x9f1c...2a3b".to_string(),
        Some("Remote Peer DID: 0x9f1c2e3a4b5d6e7f8091a2b3c4d5e6f708192a3b".to_string()),
        12,
    );

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    let mut output = String::new();

    for tab in crate::action::TabId::ALL {
        app.active_tab = tab;
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        output.push_str(&format!("=== VIEW: {} ===\n", tab.title()));
        let buf = terminal.backend().buffer();
        for y in 0..height {
            let mut line = String::with_capacity(usize::from(width));
            for x in 0..width {
                if let Some(cell) = buf.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            output.push_str(line.trim_end());
            output.push('\n');
        }
        output.push('\n');
    }

    Ok(output)
}

/// Spawn a dedicated background thread that reads terminal events and forwards them to a Tokio
/// channel, preventing blocking `crossterm::event::poll` calls from starving `current-thread`
/// Tokio executors.
fn spawn_terminal_input_reader(
    stop_flag: Arc<AtomicBool>,
) -> (mpsc::UnboundedReceiver<Event>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = std::thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match event::poll(INPUT_POLL_SLICE) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    (rx, handle)
}

/// Drain and spawn all queued commands from `app` onto `worker_tasks`.
fn dispatch_queued_commands(
    app: &mut App,
    client: &Arc<dyn NodeClient>,
    outcome_tx: &mpsc::UnboundedSender<DispatchedOutcome>,
    worker_tasks: &mut tokio::task::JoinSet<()>,
) {
    while let Some(dispatched) = app.pop_pending_command() {
        let client_clone = Arc::clone(client);
        let tx = outcome_tx.clone();
        worker_tasks.spawn(async move {
            let outcome = execute_dispatched_command(client_clone.as_ref(), dispatched).await;
            let _ = tx.send(outcome);
        });
    }
}

/// Launch the interactive TUI using a [`ResolvedTuiConfig`].
pub async fn run_with_config(config: ResolvedTuiConfig) -> Result<()> {
    enable_raw_mode()?;
    // Create the guard immediately after enabling raw mode so an error in `execute!` still
    // restores raw mode on unwind/return.
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(config);
    let mut client: Arc<dyn NodeClient> = if app.config.demo {
        Arc::new(DemoNodeClient::new())
    } else {
        Arc::new(HttpNodeClient::from_config(&app.config)?)
    };

    let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel::<DispatchedOutcome>();
    let mut worker_tasks = tokio::task::JoinSet::new();

    let stop_input = Arc::new(AtomicBool::new(false));
    let (mut input_rx, input_thread) = spawn_terminal_input_reader(Arc::clone(&stop_input));
    let mut tick_interval = tokio::time::interval(app.config.refresh_interval);
    // Consume the immediate first tick because `App::new` already queued an initial refresh.
    tick_interval.tick().await;

    loop {
        if app.pending_token_reload {
            app.pending_token_reload = false;
            match app.config.load_token_candidate() {
                Ok(candidate) => {
                    match HttpNodeClient::new(&app.config.endpoint_url, Some(candidate.clone())) {
                        Ok(new_client) => {
                            client = Arc::new(new_client);
                            app.config.commit_reloaded_token(candidate);
                            worker_tasks.abort_all();
                            app.bump_client_epoch();
                            app.push_log(
                                "reloadToken",
                                true,
                                format!(
                                    "Reloaded API token from {} (epoch {})",
                                    app.config.token_path.display(),
                                    app.client_epoch
                                ),
                                None,
                                0,
                            );
                        }
                        Err(err) => {
                            app.push_log("reloadToken", false, err.to_string(), None, 0);
                        }
                    }
                }
                Err(err) => {
                    app.push_log("reloadToken", false, err.to_string(), None, 0);
                }
            }
        }

        dispatch_queued_commands(&mut app, &client, &outcome_tx, &mut worker_tasks);

        while let Ok(dispatched_outcome) = outcome_rx.try_recv() {
            app.apply_outcome(dispatched_outcome);
        }

        while worker_tasks.try_join_next().is_some() {}

        terminal.draw(|frame| ui::render(frame, &mut app))?;

        if app.should_quit {
            stop_input.store(true, Ordering::Relaxed);
            worker_tasks.abort_all();
            while worker_tasks.join_next().await.is_some() {}
            let _ = input_thread.join();
            break;
        }

        tokio::select! {
            maybe_outcome = outcome_rx.recv() => {
                if let Some(dispatched_outcome) = maybe_outcome {
                    app.apply_outcome(dispatched_outcome);
                }
            }
            maybe_ev = input_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    match ev {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            app.handle_key_event(key);
                        }
                        Event::Mouse(mouse) => {
                            app.handle_mouse_event(mouse);
                        }
                        _ => {}
                    }
                }
            }
            _ = tick_interval.tick() => {
                app.on_tick();
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use crossterm::event::MouseButton;
    use crossterm::event::MouseEvent;
    use crossterm::event::MouseEventKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use rings_rpc::protos::rings_node::DhtInfo;
    use rings_rpc::protos::rings_node::FingerTableRange;
    use rings_rpc::protos::rings_node::PeerCreditInfo;
    use rings_rpc::protos::rings_node::PeerInfo;
    use rings_rpc::protos::rings_node::PeerMeasurementCountersInfo;
    use rings_rpc::protos::rings_node::PeerMeasurementInfo;
    use rings_rpc::protos::rings_node::PeerReliabilityInfo;
    use rings_rpc::protos::rings_node::SwarmInfo;

    use super::*;
    use crate::action::ButtonId;
    use crate::action::NodeCommand;
    use crate::action::TabId;
    use crate::config::CredentialSource;
    use crate::error::TuiError;
    use crate::types::DhtLookupSnapshotDto;
    use crate::types::DhtLookupStatus;
    use crate::types::GatewayProbeState;
    use crate::types::GatewayStatusDto;
    use crate::types::HealthSnapshotDto;
    use crate::types::LatencyBucketDto;
    use crate::types::MessageTotalsDto;
    use crate::types::NodeSnapshot;
    use crate::types::OperatorSnapshotDto;
    use crate::types::RecentMessageEventDto;
    use crate::types::SessionKeySnapshotDto;

    #[derive(Default)]
    struct FakeNodeClient {
        fail_register_service: AtomicBool,
        topic_fetch_calls: AtomicUsize,
    }

    #[async_trait]
    impl NodeClient for FakeNodeClient {
        async fn fetch_snapshot(&self, include_expired: bool) -> Result<NodeSnapshot> {
            // Yield once to verify `current_thread` runtime scheduling of spawned worker tasks.
            tokio::task::yield_now().await;
            Ok(NodeSnapshot {
                did: Some("0x1111222233334444555566667777888899990000".to_string()),
                wire_version: Some(1),
                version: Some("1.0.0-pre.3".to_string()),
                swarm: Some(SwarmInfo {
                    peers: vec![PeerInfo {
                        did: "0xaaaa000000000000000000000000000000000001".to_string(),
                        state: "Connected".to_string(),
                    }],
                    dht: Some(DhtInfo {
                        did: "0x1111222233334444555566667777888899990000".to_string(),
                        successors: vec!["0xaaaa000000000000000000000000000000000001".to_string()],
                        predecessor: Some("0xbbbb000000000000000000000000000000000002".to_string()),
                        finger_table_ranges: vec![FingerTableRange {
                            did: Some("0xaaaa000000000000000000000000000000000001".to_string()),
                            start: 0,
                            end: 63,
                        }],
                    }),
                    persistence_storage: None,
                    cache_storage: None,
                }),
                peers: vec![PeerInfo {
                    did: "0xaaaa000000000000000000000000000000000001".to_string(),
                    state: "Connected".to_string(),
                }],
                measurements: vec![PeerMeasurementInfo {
                    did: "0xaaaa000000000000000000000000000000000001".to_string(),
                    counters: PeerMeasurementCountersInfo {
                        connected: 1,
                        disconnected: 0,
                        sent: 42,
                        failed_to_send: 0,
                        received: 39,
                        failed_to_receive: 0,
                    },
                    credit: Some(PeerCreditInfo {
                        bytes_sent_to_peer: 12_345,
                        bytes_received_from_peer: 23_456,
                        last_seen_seconds: 1_700_000_000,
                        score: 3.5,
                    }),
                    reliability: PeerReliabilityInfo::Healthy,
                }],
                observability: Some(OperatorSnapshotDto {
                    schema_version: 1,
                    generated_at_ms: 1_700_000_060_000,
                    process_started_at_ms: 1_700_000_000_000,
                    counter_scope: "node_local_process_lifetime".to_string(),
                    messages: MessageTotalsDto {
                        sent: 100,
                        received: 95,
                        forwarded: 30,
                        stored: 4,
                        failed: 1,
                    },
                    recent_messages: vec![RecentMessageEventDto {
                        sequence: 1,
                        observed_at_ms: 1_700_000_059_500,
                        action: "sent".to_string(),
                        category: "application".to_string(),
                        message_class: "CustomMessage".to_string(),
                        outcome: "succeeded".to_string(),
                    }],
                    session_key: SessionKeySnapshotDto {
                        valid: true,
                        created_at_ms: 1_700_000_000_000,
                        expires_at_ms: 1_702_592_000_000,
                        remaining_ms: 2_591_940_000,
                        rotation_succeeded_total: 1,
                        rotation_failed_total: 0,
                        runtime_rotation_supported: true,
                    },
                    mailboxes: Default::default(),
                    dht_lookups: DhtLookupSnapshotDto {
                        total: 12,
                        succeeded: 11,
                        failed: 0,
                        timed_out: 1,
                        in_flight: 0,
                        latency_sum_ms: 240,
                        latency_count: 12,
                        latency_buckets: vec![
                            LatencyBucketDto {
                                le_ms: 10,
                                count: 6,
                            },
                            LatencyBucketDto {
                                le_ms: 50,
                                count: 11,
                            },
                            LatencyBucketDto {
                                le_ms: 1_000,
                                count: 12,
                            },
                        ],
                    },
                    peer_ratings: Vec::new(),
                    health: HealthSnapshotDto {
                        process_api_healthy: true,
                        admitted_peer_count: 1,
                        has_admitted_peer: true,
                        has_successor: true,
                        has_predecessor: true,
                        overlay_ready: true,
                    },
                }),
                gateway: GatewayProbeState::Available(GatewayStatusDto {
                    health: "active".to_string(),
                    interface: Some("utun9".to_string()),
                    capture_routes: vec!["198.18.0.0/15".to_string()],
                    exit_availability: "available".to_string(),
                    active_flows: 3,
                    reason: None,
                    last_flow_reset: None,
                }),
                online_nodes: Vec::new(),
                onion_exits: Vec::new(),
                include_expired_directory: include_expired,
                warnings: Vec::new(),
                fetched_at_ms: 1_700_000_060_000,
                poll_latency_ms: 4,
            })
        }

        async fn connect_peer_via_http(
            &self,
            _url: &str,
            _remote_api_token: Option<String>,
        ) -> Result<String> {
            Ok("0xcccc000000000000000000000000000000000003".to_string())
        }

        async fn connect_with_did(&self, _did: &str) -> Result<()> {
            Ok(())
        }

        async fn connect_with_seed(&self, _source: &str) -> Result<usize> {
            Ok(2)
        }

        async fn disconnect(&self, _did: &str) -> Result<()> {
            Ok(())
        }

        async fn create_offer(&self, _did: &str) -> Result<String> {
            Ok("base58-offer-payload".to_string())
        }

        async fn answer_offer(&self, _offer: &str) -> Result<String> {
            Ok("base58-answer-payload".to_string())
        }

        async fn accept_answer(&self, _answer: &str) -> Result<()> {
            Ok(())
        }

        async fn send_backend_message(
            &self,
            _destination_did: &str,
            _namespace: &str,
            _data: &str,
        ) -> Result<()> {
            Ok(())
        }

        async fn publish_topic_message(&self, _topic: &str, _data: &str) -> Result<()> {
            Ok(())
        }

        async fn fetch_topic_messages(&self, _topic: &str, _skip: i64) -> Result<Vec<String>> {
            self.topic_fetch_calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec!["hello-1".to_string(), "hello-2".to_string()])
        }

        async fn register_service(&self, _name: &str) -> Result<()> {
            if self.fail_register_service.load(Ordering::Relaxed) {
                return Err(TuiError::Rpc("simulated registerService RPC error".into()));
            }
            Ok(())
        }

        async fn lookup_service(&self, _name: &str) -> Result<Vec<String>> {
            Ok(vec![
                "0xaaaa000000000000000000000000000000000001".to_string()
            ])
        }

        async fn peer_measurement(&self, did: &str) -> Result<Option<PeerMeasurementInfo>> {
            Ok(Some(PeerMeasurementInfo {
                did: did.to_string(),
                counters: PeerMeasurementCountersInfo {
                    connected: 1,
                    disconnected: 0,
                    sent: 10,
                    failed_to_send: 0,
                    received: 10,
                    failed_to_receive: 0,
                },
                credit: None,
                reliability: PeerReliabilityInfo::Healthy,
            }))
        }

        async fn dht_lookup_key(&self, key: &str) -> Result<crate::types::DhtKeyLookupReport> {
            let target = crate::types::DhtQueryTarget::parse(key);
            Ok(crate::types::DhtKeyLookupReport {
                query_key: key.to_string(),
                query_mode: target.mode_label().to_string(),
                target_did: target.target_entry_did().to_string(),
                status: DhtLookupStatus::Found,
                sources: vec!["Live DHT Topic Network".to_string()],
                record_kind: "Data/Append".to_string(),
                elements: vec!["value-1".to_string()],
                warning: None,
                duration_ms: 3,
            })
        }
    }

    fn test_config() -> ResolvedTuiConfig {
        ResolvedTuiConfig {
            config_path: PathBuf::from("~/.rings/config.yaml"),
            config_loaded: true,
            endpoint_url: "http://127.0.0.1:50000".to_string(),
            external_api_addr: Some("127.0.0.1:50001".to_string()),
            onion_http_proxy_addr: Some("127.0.0.1:18080".to_string()),
            network_id: Some(1),
            advertise_onion_relay: true,
            advertise_onion_exit: false,
            token_path: PathBuf::from("~/.rings/api-token"),
            api_token: Some("0123456789abcdef0123456789abcdef".to_string()),
            credential_source: CredentialSource::TokenFile,
            warning: None,
            refresh_interval: Duration::from_secs(2),
            demo: false,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_runtime_executes_spawned_tasks_and_renders_all_tabs() {
        let client: Arc<dyn NodeClient> = Arc::new(FakeNodeClient::default());
        let mut app = App::new(test_config());

        let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel();
        let mut worker_tasks = tokio::task::JoinSet::new();

        // Dispatch initial refresh onto a JoinSet on a `current_thread` Tokio runtime and await via `outcome_rx.recv()`.
        dispatch_queued_commands(&mut app, &client, &outcome_tx, &mut worker_tasks);
        let outcome = outcome_rx.recv().await.expect("worker outcome");
        app.apply_outcome(outcome);

        assert_eq!(app.connection_state, crate::app::ConnectionState::Online);
        assert_eq!(
            app.snapshot.did.as_deref(),
            Some("0x1111222233334444555566667777888899990000")
        );

        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        // Render every tab and verify hit regions are populated.
        for tab in TabId::ALL {
            app.active_tab = tab;
            terminal
                .draw(|frame| ui::render(frame, &mut app))
                .expect("draw tab");
        }

        // Trigger ConnectUrl button via hotkey 'c', type URL, and submit via Enter.
        app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.modal.is_some());
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw modal");

        for ch in "http://127.0.0.1:50001".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.modal.is_none());

        let connect_cmd = app.pop_pending_command().expect("connect command");
        assert!(matches!(
            connect_cmd.command,
            NodeCommand::ConnectPeerViaHttp { .. }
        ));
        let connect_outcome = execute_dispatched_command(client.as_ref(), connect_cmd).await;
        app.apply_outcome(connect_outcome);
        assert_eq!(app.action_logs.len(), 1);
        assert!(app.action_logs[0].succeeded);

        // Test mouse click on a tab in the tab bar (row 4, column 25 -> Monitor tab).
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw before mouse click");
        app.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.active_tab, TabId::Monitor);

        // Test triggering CreateOffer button directly.
        app.trigger_button(ButtonId::CreateOffer);
        assert!(app.modal.is_some());
        app.submit_modal();
        let _auto_refresh = app.pop_pending_command();
        let offer_cmd = app.pop_pending_command().expect("create offer command");
        let offer_outcome = execute_dispatched_command(client.as_ref(), offer_cmd).await;
        app.apply_outcome(offer_outcome);
        assert!(app
            .last_command_output
            .as_deref()
            .unwrap_or_default()
            .contains("base58-offer-payload"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn register_service_updates_state_only_after_rpc_success() {
        let client = FakeNodeClient::default();
        client.fail_register_service.store(true, Ordering::Relaxed);
        let mut app = App::new(test_config());
        let _ = app.pop_pending_command();

        // 1. Open RegisterService modal and submit while RPC is configured to fail.
        app.trigger_button(ButtonId::RegisterService);
        for ch in "broken-svc".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.submit_modal();
        // Before RPC completion, `registered_services` must NOT be mutated optimistically.
        assert!(app.registered_services.is_empty());

        let dispatched = app.pop_pending_command().expect("register command");
        let failed_outcome = execute_dispatched_command(&client, dispatched).await;
        app.apply_outcome(failed_outcome);
        assert!(app.registered_services.is_empty());
        assert!(!app.action_logs[0].succeeded);

        // 2. Now allow RPC to succeed and verify `registered_services` is updated on outcome.
        client.fail_register_service.store(false, Ordering::Relaxed);
        app.trigger_button(ButtonId::RegisterService);
        for ch in "valid-svc".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.submit_modal();
        assert!(app.registered_services.is_empty());

        let dispatched_ok = app.pop_pending_command().expect("register command");
        let ok_outcome = execute_dispatched_command(&client, dispatched_ok).await;
        app.apply_outcome(ok_outcome);
        assert_eq!(app.registered_services, vec!["valid-svc".to_string()]);
    }

    #[test]
    fn rejects_stale_epoch_out_of_order_and_mismatched_filter_snapshots() {
        let mut app = App::new(test_config());
        let first_req = app.pop_pending_command().expect("initial refresh");
        assert_eq!(first_req.meta.client_epoch, 1);
        assert_eq!(first_req.meta.request_id, 1);

        // Queue a second refresh (request_id = 2) and apply it first.
        app.trigger_button(ButtonId::RefreshNow);
        let second_req = app.pop_pending_command().expect("second refresh");
        assert_eq!(second_req.meta.request_id, 2);

        app.apply_outcome(DispatchedOutcome {
            meta: second_req.meta,
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xnewer".to_string()),
                include_expired_directory: false,
                ..NodeSnapshot::default()
            })),
        });
        assert_eq!(app.snapshot.did.as_deref(), Some("0xnewer"));

        // Now deliver out-of-order response for `first_req` (request_id = 1 < 2) -> must be ignored.
        app.apply_outcome(DispatchedOutcome {
            meta: first_req.meta,
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xolder".to_string()),
                include_expired_directory: false,
                ..NodeSnapshot::default()
            })),
        });
        assert_eq!(app.snapshot.did.as_deref(), Some("0xnewer"));

        // Deliver a response with mismatched `include_expired_directory` -> must be ignored.
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 3,
            },
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xwrong_filter".to_string()),
                include_expired_directory: true,
                ..NodeSnapshot::default()
            })),
        });
        assert_eq!(app.snapshot.did.as_deref(), Some("0xnewer"));

        // Bump client epoch (e.g. token reload) -> old epoch outcomes must be ignored.
        app.bump_client_epoch();
        assert_eq!(app.client_epoch, 2);
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 99,
            },
            outcome: CommandOutcome::SnapshotError {
                include_expired_directory: false,
                error: "stale client error".to_string(),
            },
        });
        assert_eq!(app.connection_state, crate::app::ConnectionState::Online);
    }
}
