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
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
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
const CHANNEL_CAPACITY: usize = 64;

/// Guard that restores the terminal out of raw mode and alternate screen on drop.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

/// RAII guard for the background terminal input reader thread that signals the thread to stop
/// whenever the TUI exits (including on early `?` error returns, future cancellation, or unwind).
struct InputReaderGuard {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl InputReaderGuard {
    /// Close `input_rx`, set `stop_flag`, and join the reader thread so it can never deadlock
    /// on a full channel buffer during shutdown.
    fn shutdown<T>(&mut self, input_rx: &mut mpsc::Receiver<T>) {
        input_rx.close();
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for InputReaderGuard {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

/// Guard that aborts an inner spawned task when the outer supervision task is cancelled.
struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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
    app.clear_in_flight();

    if app.config.demo {
        for req_id in 1..=8_u64 {
            let snap = client.fetch_snapshot(false).await?;
            app.apply_outcome(DispatchedOutcome {
                meta: RequestMeta {
                    client_epoch: app.client_epoch,
                    request_id: req_id,
                },
                outcome: CommandOutcome::Snapshot(Box::new(snap)),
            });
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
    } else {
        let snap = client.fetch_snapshot(false).await?;
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: app.client_epoch,
                request_id: 1,
            },
            outcome: CommandOutcome::Snapshot(Box::new(snap)),
        });
    }

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

/// Spawn a dedicated background thread that reads terminal events and forwards them to a bounded
/// Tokio channel, returning the receiver and an RAII [`InputReaderGuard`].
fn spawn_terminal_input_reader() -> (mpsc::Receiver<Event>, InputReaderGuard) {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop_flag);
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let handle = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            match event::poll(INPUT_POLL_SLICE) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        // `InputReaderGuard::shutdown` closes `rx` before `join()`, so `blocking_send`
                        // preserves every keystroke under backpressure while immediately unblocking on exit.
                        if tx.blocking_send(ev).is_err() {
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
    (rx, InputReaderGuard {
        stop_flag,
        handle: Some(handle),
    })
}

/// Drain and spawn eligible queued commands from `app` onto `worker_tasks`.
///
/// Each task returns `(RequestMeta, Option<String>)`, where `Some(err)` indicates that the inner
/// command execution panicked or terminated abnormally without sending a [`DispatchedOutcome`].
fn dispatch_queued_commands(
    app: &mut App,
    client: &Arc<dyn NodeClient>,
    outcome_tx: &mpsc::Sender<DispatchedOutcome>,
    worker_tasks: &mut tokio::task::JoinSet<(RequestMeta, Option<String>)>,
) {
    while let Some(dispatched) = app.pop_pending_command() {
        let meta = dispatched.meta;
        let client_clone = Arc::clone(client);
        let tx = outcome_tx.clone();
        worker_tasks.spawn(async move {
            let mut inner = tokio::spawn(async move {
                execute_dispatched_command(client_clone.as_ref(), dispatched).await
            });
            let _abort_guard = AbortTaskOnDrop(inner.abort_handle());
            match (&mut inner).await {
                Ok(outcome) => {
                    let _ = tx.send(outcome).await;
                    (meta, None)
                }
                Err(join_err) if !join_err.is_cancelled() => (meta, Some(join_err.to_string())),
                Err(_) => (meta, None),
            }
        });
    }
}

/// Reap finished background tasks, retiring the exact [`RequestMeta`] if any task panicked.
fn reap_finished_workers(
    app: &mut App,
    worker_tasks: &mut tokio::task::JoinSet<(RequestMeta, Option<String>)>,
) {
    while let Some(join_res) = worker_tasks.try_join_next() {
        if let Ok((meta, Some(panic_reason))) = join_res {
            app.on_worker_aborted_or_panicked(meta, &panic_reason);
        }
    }
}

/// Route a single [`crossterm::event::Event`] from the terminal input reader into [`App`].
pub fn apply_terminal_event(app: &mut App, ev: Event) {
    match ev {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            app.handle_key_event(key);
        }
        Event::Mouse(mouse) => {
            app.handle_mouse_event(mouse);
        }
        Event::Paste(text) => {
            app.handle_paste(&text);
        }
        _ => {}
    }
}

/// Launch the interactive TUI using a [`ResolvedTuiConfig`].
pub async fn run_with_config(config: ResolvedTuiConfig) -> Result<()> {
    enable_raw_mode()?;
    // Create the guard immediately after enabling raw mode so an error in `execute!` still
    // restores raw mode on unwind/return.
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(config);
    let mut client: Arc<dyn NodeClient> = if app.config.demo {
        Arc::new(DemoNodeClient::new())
    } else {
        Arc::new(HttpNodeClient::from_config(&app.config)?)
    };

    let (outcome_tx, mut outcome_rx) = mpsc::channel::<DispatchedOutcome>(CHANNEL_CAPACITY);
    let mut worker_tasks = tokio::task::JoinSet::new();

    let (mut input_rx, mut input_guard) = spawn_terminal_input_reader();
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

        reap_finished_workers(&mut app, &mut worker_tasks);

        terminal.draw(|frame| ui::render(frame, &mut app))?;

        if app.should_quit {
            worker_tasks.abort_all();
            while worker_tasks.join_next().await.is_some() {}
            input_guard.shutdown(&mut input_rx);
            break;
        }

        tokio::select! {
            maybe_outcome = outcome_rx.recv() => {
                if let Some(dispatched_outcome) = maybe_outcome {
                    app.apply_outcome(dispatched_outcome);
                }
            }
            maybe_ev = input_rx.recv() => {
                match maybe_ev {
                    Some(ev) => {
                        apply_terminal_event(&mut app, ev);
                    }
                    None => {
                        // Terminal input stream closed or errored; exit cleanly instead of spinning.
                        app.should_quit = true;
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
    use crate::types::DhtKeyLookupReport;
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
                version: Some("1.0.0-pre.12".to_string()),
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
                online_nodes_error: None,
                onion_exits: Vec::new(),
                onion_exits_error: None,
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

        async fn dht_lookup_key(&self, key: &str) -> Result<DhtKeyLookupReport> {
            let target = crate::types::DhtQueryTarget::parse(key);
            Ok(DhtKeyLookupReport {
                query_key: key.to_string(),
                query_mode: target.mode_label().to_string(),
                target_did: target.target_entry_did().to_string(),
                status: DhtLookupStatus::Found,
                sources: vec!["DHT Topic RPC (Network/Cache)".to_string()],
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

        let (outcome_tx, mut outcome_rx) = mpsc::channel(CHANNEL_CAPACITY);
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

        // Drain the initial refresh flag if needed and pop ConnectPeerViaHttp.
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
        // Simulate first_req completing or allow a second refresh after it:
        app.clear_in_flight();

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

        // Out-of-order `DhtLookupKey` outcomes: apply request_id = 5 first, then stale request_id = 4.
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 5,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "dhtLookupKey",
                summary: "newer lookup".to_string(),
                output: Some("newer-output".to_string()),
                measurement: None,
                dht_lookup: Some(Box::new(DhtKeyLookupReport {
                    query_key: "newer-key".to_string(),
                    ..DhtKeyLookupReport::default()
                })),
                registered_service: None,
                duration_ms: 2,
            },
        });
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 4,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "dhtLookupKey",
                summary: "older lookup".to_string(),
                output: Some("older-output".to_string()),
                measurement: None,
                dht_lookup: Some(Box::new(DhtKeyLookupReport {
                    query_key: "older-key".to_string(),
                    ..DhtKeyLookupReport::default()
                })),
                registered_service: None,
                duration_ms: 8,
            },
        });
        assert_eq!(
            app.last_dht_lookup.as_ref().map(|r| r.query_key.as_str()),
            Some("newer-key")
        );
        assert_eq!(app.last_command_output.as_deref(), Some("newer-output"));

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

    #[test]
    fn coalesces_duplicate_refreshes_and_recovers_from_worker_panic() {
        let mut app = App::new(test_config());
        // Spam `[R]` 10 times before popping any command: all must coalesce into 1 queued RefreshSnapshot.
        for _ in 0..10 {
            app.trigger_button(ButtonId::RefreshNow);
        }
        let first_refresh = app.pop_pending_command().expect("first refresh");
        assert!(matches!(
            first_refresh.command,
            NodeCommand::RefreshSnapshot { .. }
        ));
        assert!(app.has_refresh_in_flight);
        assert_eq!(app.in_flight_commands, 1);

        // Queue a read-only LookupService and a second RefreshNow while `first_refresh` is still in flight.
        app.trigger_button(ButtonId::LookupService);
        for ch in "ipfs".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.submit_modal();
        app.trigger_button(ButtonId::RefreshNow);

        // `pop_pending_command` must skip the queued `RefreshSnapshot` (since `first_refresh` is in flight)
        // and pop `LookupService`!
        let lookup_cmd = app.pop_pending_command().expect("lookup command");
        assert!(matches!(
            lookup_cmd.command,
            NodeCommand::LookupService { .. }
        ));
        assert_eq!(app.in_flight_commands, 2);
        assert!(app.pop_pending_command().is_none());

        // If `lookup_cmd` panics, `on_worker_aborted_or_panicked` must retire ONLY `lookup_cmd.meta`
        // and MUST NOT clear `has_refresh_in_flight` while `first_refresh` is still running!
        app.on_worker_aborted_or_panicked(lookup_cmd.meta, "simulated lookup panic");
        assert_eq!(app.in_flight_commands, 1);
        assert!(app.has_refresh_in_flight);
        assert!(app.pop_pending_command().is_none());

        // Now if `first_refresh` itself panics (or finishes), `has_refresh_in_flight` clears and
        // the coalesced refresh is finally dispatched.
        app.on_worker_aborted_or_panicked(first_refresh.meta, "simulated refresh panic");
        assert_eq!(app.in_flight_commands, 0);
        assert!(!app.has_refresh_in_flight);
        let recovered = app
            .pop_pending_command()
            .expect("queued refresh after recovery");
        assert!(matches!(
            recovered.command,
            NodeCommand::RefreshSnapshot { .. }
        ));
    }

    #[test]
    fn bidirectional_snapshot_and_peer_measurement_versioning_preserves_newer_data() {
        let mut app = App::new(test_config());
        let _ = app.pop_pending_command();
        app.clear_in_flight();

        // Suppose Snapshot req_id = 2 is dispatched BEFORE InspectPeer(peerA) req_id = 3,
        // and InspectPeer(peerB) req_id = 4 is also dispatched.
        // First, InspectPeer(peerB) req_id = 4 finishes, then InspectPeer(peerA) req_id = 3 finishes.
        // Because per-peer versions are tracked independently, peerB (req 4) must NOT block peerA (req 3)!
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 4,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "peerMeasurement",
                summary: "peerB measured".to_string(),
                output: None,
                measurement: Some(Box::new(PeerMeasurementInfo {
                    did: "0xpeerB".to_string(),
                    counters: PeerMeasurementCountersInfo {
                        sent: 400,
                        ..PeerMeasurementCountersInfo::default()
                    },
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                })),
                dht_lookup: None,
                registered_service: None,
                duration_ms: 1,
            },
        });
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 3,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "peerMeasurement",
                summary: "peerA measured".to_string(),
                output: None,
                measurement: Some(Box::new(PeerMeasurementInfo {
                    did: "0xpeerA".to_string(),
                    counters: PeerMeasurementCountersInfo {
                        sent: 300,
                        ..PeerMeasurementCountersInfo::default()
                    },
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                })),
                dht_lookup: None,
                registered_service: None,
                duration_ms: 1,
            },
        });

        // Now older Snapshot req_id = 2 arrives with stale `sent = 10` for `0xpeerA`.
        // Because `0xpeerA` has measurement version 3 > 2, Snapshot req_id = 2 MUST NOT clobber `sent = 300`!
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 2,
            },
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xlocal".to_string()),
                peers: vec![PeerInfo {
                    did: "0xpeerA".to_string(),
                    state: "Connected".to_string(),
                }],
                measurements: vec![PeerMeasurementInfo {
                    did: "0xpeerA".to_string(),
                    counters: PeerMeasurementCountersInfo {
                        sent: 10,
                        ..PeerMeasurementCountersInfo::default()
                    },
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                }],
                include_expired_directory: false,
                ..NodeSnapshot::default()
            })),
        });

        let rows = app.snapshot.peer_traffic_rows();
        let peer_a = rows
            .iter()
            .find(|r| r.peer_did == "0xpeerA")
            .expect("peerA");
        let peer_b = rows
            .iter()
            .find(|r| r.peer_did == "0xpeerB")
            .expect("peerB");
        assert_eq!(peer_a.sent_ok, 300);
        assert_eq!(peer_b.sent_ok, 400);

        // Direction 2: Newer Snapshot (req_id = 10, sent = 900) arrives first, and then an older
        // in-flight `InspectPeer(peerA)` (req_id = 6, sent = 500) arrives late -> must NOT overwrite 900!
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 10,
            },
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xlocal".to_string()),
                peers: vec![PeerInfo {
                    did: "0xpeerA".to_string(),
                    state: "Connected".to_string(),
                }],
                measurements: vec![PeerMeasurementInfo {
                    did: "0xpeerA".to_string(),
                    counters: PeerMeasurementCountersInfo {
                        sent: 900,
                        ..PeerMeasurementCountersInfo::default()
                    },
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                }],
                include_expired_directory: false,
                ..NodeSnapshot::default()
            })),
        });
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 6,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "peerMeasurement",
                summary: "late peerA measurement".to_string(),
                output: None,
                measurement: Some(Box::new(PeerMeasurementInfo {
                    did: "0xpeerA".to_string(),
                    counters: PeerMeasurementCountersInfo {
                        sent: 500,
                        ..PeerMeasurementCountersInfo::default()
                    },
                    credit: None,
                    reliability: PeerReliabilityInfo::Healthy,
                })),
                dht_lookup: None,
                registered_service: None,
                duration_ms: 1,
            },
        });
        let rows2 = app.snapshot.peer_traffic_rows();
        let peer_a2 = rows2
            .iter()
            .find(|r| r.peer_did == "0xpeerA")
            .expect("peerA");
        assert_eq!(peer_a2.sent_ok, 900);
    }

    #[test]
    fn input_guard_shutdown_never_deadlocks_when_channel_is_full() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop_flag);
        let (tx, mut rx) = mpsc::channel::<u32>(2);
        // Fill the bounded channel to capacity so the next `blocking_send` parks the producer thread.
        tx.try_send(1).expect("fill 1");
        tx.try_send(2).expect("fill 2");

        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                if tx.blocking_send(99).is_err() {
                    break;
                }
            }
        });

        let mut guard = InputReaderGuard {
            stop_flag,
            handle: Some(handle),
        };
        // `shutdown` closes `rx` before `join()`, immediately waking `tx.blocking_send(99)` with `Err`.
        guard.shutdown(&mut rx);
        assert!(guard.handle.is_none());
    }

    #[test]
    fn scrolled_peers_table_mouse_click_and_modal_prefill_use_offset_and_merged_order() {
        let mut app = App::new(test_config());
        let _ = app.pop_pending_command();

        // Build a snapshot with 1 connected peer (`0xpeer00`) and 15 retained measured peers
        // (`0xmeas01`..`0xmeas15`), plus a duplicate measurement for `0xpeer00`.
        let mut measurements = vec![PeerMeasurementInfo {
            did: "0xpeer00".to_string(),
            counters: PeerMeasurementCountersInfo::default(),
            credit: None,
            reliability: PeerReliabilityInfo::Healthy,
        }];
        for idx in 1..=15_u32 {
            measurements.push(PeerMeasurementInfo {
                did: format!("0xmeas{idx:02}"),
                counters: PeerMeasurementCountersInfo::default(),
                credit: None,
                reliability: PeerReliabilityInfo::Healthy,
            });
        }

        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: app.client_epoch,
                request_id: 1,
            },
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xlocal".to_string()),
                peers: vec![PeerInfo {
                    did: "0xpeer00".to_string(),
                    state: "Connected".to_string(),
                }],
                measurements,
                include_expired_directory: false,
                ..NodeSnapshot::default()
            })),
        });

        app.active_tab = TabId::Peers;
        // Move selection down to row 14 (`0xmeas14`), which forces the table in a 24-row terminal
        // to scroll (`state.offset() > 0`).
        for _ in 0..14 {
            app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(app.selected_peer_idx, 14);
        assert_eq!(app.selected_did(), Some("0xmeas14"));

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw scrolled peers tab");

        // Click the first visible data row in the Peers table (row y = 11, column x = 10).
        // Because the table scrolled (`offset > 0`), clicking the first visible row MUST select
        // `offset` (> 0), NOT row 0!
        app.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 11,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.selected_peer_idx > 0,
            "expected scrolled click to include table offset, got {}",
            app.selected_peer_idx
        );
        let expected_did = format!("0xmeas{:02}", app.selected_peer_idx);
        assert_eq!(app.selected_did(), Some(expected_did.as_str()));

        // Opening InspectPeer modal must prefill `expected_did`.
        app.trigger_button(ButtonId::InspectPeer);
        let modal = app.modal.as_ref().expect("inspect modal");
        assert_eq!(modal.fields[0].value, expected_did);
    }

    #[test]
    fn ui_focus_navigation_log_inspection_and_compact_80x24_layout_work() {
        let mut app = App::new(test_config());
        let _ = app.pop_pending_command();
        app.clear_in_flight();

        // 1. Focus toolbar via `[B]`, move left to `[Q]Quit`, then press `3` to switch to Peers.
        // Switching tabs via numeric hotkey must clear `focused_toolbar_idx` so `Enter` opens
        // InspectPeer instead of quitting!
        app.handle_key_event(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(app.focused_toolbar_idx, Some(0));
        app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.focused_toolbar_idx, Some(ButtonId::TOOLBAR.len() - 1));
        app.handle_key_event(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!(app.active_tab, TabId::Peers);
        assert_eq!(app.focused_toolbar_idx, None);
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert!(app.modal.is_some());
        app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        // 2. Verify DHT Left (`prev`) vs Right (`next`) navigation.
        app.handle_key_event(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.dht_subtab, crate::app::DhtSubtab::FingerTable);
        app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.dht_subtab, crate::app::DhtSubtab::CacheStorage);
        app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.dht_subtab, crate::app::DhtSubtab::FingerTable);

        // 3. Verify Commands Workbench top-row `Up` stays in its column and `{`/`}` navigates logs.
        app.handle_key_event(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        app.selected_workbench_idx = 2;
        app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected_workbench_idx, 2);

        app.push_log(
            "createOffer",
            true,
            "offer-1".into(),
            Some("DETAIL-1".into()),
            5,
        );
        app.push_log(
            "answerOffer",
            true,
            "answer-2".into(),
            Some("DETAIL-2".into()),
            6,
        );
        assert_eq!(app.selected_log_idx, 0);
        app.handle_key_event(KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE));
        assert_eq!(app.selected_log_idx, 1);
        assert_eq!(app.last_command_output.as_deref(), Some("DETAIL-1"));

        // 4. Verify Help overlay blocks scroll wheel events from mutating underlying selection.
        app.handle_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.show_help);
        app.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.output_scroll, 0);
        app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        // 5. Render all 6 tabs in a compact 80x24 terminal and verify CJK cursor slicing.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("80x24 terminal");
        for tab in TabId::ALL {
            app.active_tab = tab;
            terminal
                .draw(|frame| ui::render(frame, &mut app))
                .expect("draw 80x24 tab");
        }

        let sliced = ui::visible_field_slice_with_cursor("你好世界Rings节点测试输入", 4, 14, true);
        assert!(sliced.contains('█'));
        assert!(unicode_width::UnicodeWidthStr::width(sliced.as_str()) <= 14);
    }

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn paste_preserves_multiline_payloads_and_normalizes_single_line_fields() {
        let mut app = App::new(test_config());
        let _ = app.pop_pending_command();

        // 1. Single-line URL field: trailing/leading newlines are trimmed and internal CRLF becomes a single space.
        app.trigger_button(ButtonId::ConnectUrl);
        apply_terminal_event(
            &mut app,
            Event::Paste("http://127.0.0.1:50001\r\n".to_string()),
        );
        let modal = app.modal.as_ref().expect("connect url modal");
        assert_eq!(modal.fields[0].value, "http://127.0.0.1:50001");
        app.submit_modal();
        assert!(app.modal.is_none());
        let cmd = app.pop_pending_command().expect("connect command");
        assert_eq!(cmd.command, NodeCommand::ConnectPeerViaHttp {
            url: "http://127.0.0.1:50001".to_string(),
            api_token: None,
        });
        app.clear_in_flight();

        // 2. Multi-line SendMessage payload field: embedded `\n` and `\t` are preserved verbatim,
        // and `\r\n` is normalized to `\n` (not spaces!).
        app.trigger_button(ButtonId::SendMessage);
        apply_terminal_event(&mut app, Event::Paste("0x1234".to_string()));
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        apply_terminal_event(
            &mut app,
            Event::Paste("line1\r\nline2\tindented\nline3".to_string()),
        );
        app.submit_modal();
        assert!(app.modal.is_none());
        let msg_cmd = app.pop_pending_command().expect("send message command");
        assert_eq!(msg_cmd.command, NodeCommand::SendBackendMessage {
            destination_did: "0x1234".to_string(),
            namespace: "default".to_string(),
            payload: "line1\nline2\tindented\nline3".to_string(),
        });
    }

    #[test]
    fn word_wrap_reachability_unicode_width_and_help_completeness_hold() {
        // 1. Four 20-char ASCII words separated by spaces in width 38 wraps onto 4 lines (1 word per line),
        // not 3 lines (`ceil(83/38) = 3`), so the 4th word is reachable when scrolled to `output_max_scroll`.
        let four_words =
            "aaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbb cccccccccccccccccccc dddddddddddddddddddd";
        let wrapped = ui::wrap_text_for_width(four_words, 38);
        assert_eq!(wrapped, vec![
            "aaaaaaaaaaaaaaaaaaaa".to_string(),
            "bbbbbbbbbbbbbbbbbbbb".to_string(),
            "cccccccccccccccccccc".to_string(),
            "dddddddddddddddddddd".to_string(),
        ]);

        // 2. 114 Latin-1 `'é'` characters (each 1 display column wide) in width 38 wraps into 3 lines,
        // NOT 6 lines!
        let latin_e = "é".repeat(114);
        let wrapped_e = ui::wrap_text_for_width(&latin_e, 38);
        assert_eq!(wrapped_e.len(), 3);

        // 2b. Overwide whitespace lines, mid-line long whitespace, and leading indentation are preserved (F5, G1).
        let ws_sample = format!("A\n{}\nB\n    indented", " ".repeat(80));
        let wrapped_ws = ui::wrap_text_for_width(&ws_sample, 38);
        assert_eq!(wrapped_ws, vec![
            "A".to_string(),
            " ".repeat(38),
            " ".repeat(38),
            " ".repeat(4),
            "B".to_string(),
            "    indented".to_string(),
        ]);
        let inline_ws = format!("A{}B", " ".repeat(80));
        let wrapped_inline_ws = ui::wrap_text_for_width(&inline_ws, 38);
        assert_eq!(wrapped_inline_ws, vec![
            format!("A{}", " ".repeat(37)),
            " ".repeat(38),
            "     B".to_string(),
        ]);

        // 2c. Grapheme-cluster cursor editing aligns visual cursor with insertion/deletion on combining marks,
        // including combining marks immediately after LF `\n\u{0301}X` (F4, G2).
        let nl_comb = ui::visible_field_slice_with_cursor("\n\u{0301}X", 2, 20, true);
        assert_eq!(nl_comb, "↵\u{0301}█X");
        let mut app_g = App::new(test_config());
        app_g.trigger_button(ButtonId::RegisterService);
        app_g.handle_paste("e\u{0301}X");
        {
            let m = app_g.modal.as_ref().expect("modal");
            assert_eq!(m.fields[0].grapheme_count(), 2);
            assert_eq!(m.fields[0].cursor, 2);
        }
        // Home -> Right moves cursor after the full `e\u{0301}` grapheme cluster (index 1, before `X`):
        app_g.handle_key_event(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        app_g.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        {
            let m = app_g.modal.as_ref().expect("modal");
            assert_eq!(m.fields[0].cursor, 1);
            let rendered = ui::visible_field_slice_with_cursor(
                &m.fields[0].value,
                m.fields[0].cursor,
                20,
                true,
            );
            assert_eq!(rendered, "e\u{0301}█X");
        }
        // Typing `'a'` inserts after `e\u{0301}` (producing `"e\u{0301}aX"`, NOT `"ea\u{0301}X"`):
        app_g.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(
            app_g.modal.as_ref().expect("modal").fields[0].value,
            "e\u{0301}aX"
        );
        // Delete removes `'X'`, Backspace removes `'a'`, second Backspace removes the entire `"e\u{0301}"` cluster:
        app_g.handle_key_event(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(
            app_g.modal.as_ref().expect("modal").fields[0].value,
            "e\u{0301}a"
        );
        app_g.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            app_g.modal.as_ref().expect("modal").fields[0].value,
            "e\u{0301}"
        );
        app_g.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app_g.modal.as_ref().expect("modal").fields[0].value, "");

        // 3. `visible_field_slice_with_cursor` never exceeds `max_cols` display width across ASCII,
        // Latin-1, combining marks, CJK, and multi-line markers.
        for sample in [
            "https://node1.rings.network:50001/jsonrpc",
            &latin_e,
            "cafe\u{0301}-combining-mark-test-string",
            "你好世界Rings节点测试输入长文本切分",
            "line1\nline2\tindented\nline3-long-payload",
        ] {
            let g_len = unicode_segmentation::UnicodeSegmentation::graphemes(sample, true).count();
            for cur in [0, 3, g_len / 2, g_len] {
                for max_cols in [8, 14, 24] {
                    let s = ui::visible_field_slice_with_cursor(sample, cur, max_cols, true);
                    let width = unicode_width::UnicodeWidthStr::width(s.as_str());
                    assert!(
                        width <= max_cols,
                        "width {width} exceeded max_cols {max_cols} for {s:?}"
                    );
                    assert!(s.contains('█'));
                }
            }
        }

        // 4. Help overlay on 80x24 and 120x36 shows all hotkey rows and the close instruction without clipping.
        let mut app = App::new(test_config());
        app.show_help = true;
        for (w, h) in [(80, 24), (120, 36)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| ui::render(frame, &mut app))
                .expect("draw help");
            let screen = buffer_to_string(&terminal);
            assert!(screen.contains("Navigation, Focus & Mouse Support"));
            assert!(screen.contains("[E] Toggle Expired Dir"));
            assert!(screen.contains("Press [Esc], [Enter], [?], or Click to close"));
        }
    }

    #[test]
    fn commands_focus_enum_log_pinning_viewport_stability_and_queue_full_modal_work() {
        let mut app = App::new(test_config());
        let _ = app.pop_pending_command();
        app.clear_in_flight();

        // Populate snapshot with failure/timeout metrics and an unmeasured connected peer (R6, R7).
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 1,
            },
            outcome: CommandOutcome::Snapshot(Box::new(NodeSnapshot {
                did: Some("0xlocal".to_string()),
                peers: vec![PeerInfo {
                    did: "0xunmeasured_peer".to_string(),
                    state: "Connected".to_string(),
                }],
                observability: Some(OperatorSnapshotDto {
                    messages: MessageTotalsDto {
                        sent: 9_999,
                        received: 9_999,
                        forwarded: 9_999,
                        stored: 9_999,
                        failed: 99,
                    },
                    dht_lookups: DhtLookupSnapshotDto {
                        total: 9_999,
                        succeeded: 9_999,
                        failed: 3,
                        timed_out: 77,
                        in_flight: 1,
                        latency_sum_ms: 500_000,
                        latency_count: 10_000,
                        latency_buckets: Vec::new(),
                    },
                    health: HealthSnapshotDto {
                        process_api_healthy: true,
                        overlay_ready: true,
                        ..HealthSnapshotDto::default()
                    },
                    ..OperatorSnapshotDto::default()
                }),
                ..NodeSnapshot::default()
            })),
        });

        // Verify 80x24 Overview renders Ovly:YES, Failed:99, S:9k..M:9k, Fail:3, and T/O:77 visibly without clipping (F3, G3),
        // and 3:Peers renders "Unknown" for unmeasured peer (R7).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("80x24 terminal");
        app.active_tab = TabId::Overview;
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw overview 80x24");
        let overview_screen = buffer_to_string(&terminal);
        assert!(overview_screen.contains("Ovly:YES"));
        assert!(overview_screen.contains("Failed:99"));
        assert!(overview_screen.contains("S:9k"));
        assert!(overview_screen.contains("M:9k"));
        assert!(overview_screen.contains("Fail:3"));
        assert!(overview_screen.contains("T/O:77"));

        // Verify `format_compact_count` is `<= 3` chars across all orders of magnitude up to `u64::MAX`
        // and that five `u64::MAX` counters + multi-minute latency still fit in 38 cols on 80x24 without clipping.
        for val in [
            0,
            999,
            1_000,
            9_999,
            99_999,
            100_000,
            999_999,
            99_999_999,
            100_000_000_000_000,
            u64::MAX,
        ] {
            let formatted = ui::format_compact_count(val);
            assert!(
                formatted.len() <= 3,
                "expected <= 3 chars for {val}, got {formatted:?}"
            );
        }
        if let Some(obs) = &mut app.snapshot.observability {
            obs.messages = MessageTotalsDto {
                sent: u64::MAX,
                received: u64::MAX,
                forwarded: u64::MAX,
                stored: u64::MAX,
                failed: u64::MAX,
            };
            obs.dht_lookups.latency_sum_ms = 1_200_000;
            obs.dht_lookups.latency_count = 1;
        }
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw overview u64::MAX");
        let max_screen = buffer_to_string(&terminal);
        assert!(max_screen.contains("Failed:18E"));
        assert!(max_screen.contains("M:18E"));
        assert!(max_screen.contains("20m"));

        app.active_tab = TabId::Peers;
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw peers 80x24");
        let peers_screen = buffer_to_string(&terminal);
        assert!(peers_screen.contains("Unknown"));

        // Verify R4: on 6:Commands, select ReloadToken (index 15), click OutputPanel, and press Enter.
        // Clicking OutputPanel focuses OutputPanel so Enter MUST NOT trigger ReloadToken!
        app.active_tab = TabId::Commands;
        app.selected_workbench_idx = 15; // ReloadToken
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw commands");
        app.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 20, // Inside OutputPanel on 80x24
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.commands_focus, crate::app::CommandsFocus::OutputPanel);
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.pending_token_reload);

        // Verify R5: when browsing an older log entry, new logs (e.g. from pressing 'p' or async outcomes)
        // keep the user's selected_log_id pinned and do not clobber `last_command_output`.
        app.commands_focus = crate::app::CommandsFocus::Workbench;
        app.follow_latest_log = true;
        app.push_log(
            "createOffer",
            true,
            "offer-1".into(),
            Some("OFFER-PAYLOAD-1".into()),
            4,
        );
        app.push_log(
            "answerOffer",
            true,
            "answer-2".into(),
            Some("ANSWER-PAYLOAD-2".into()),
            5,
        );
        // Step down to index 1 (`offer-1`), which pins `follow_latest_log = false`:
        app.handle_key_event(KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE));
        assert_eq!(app.selected_log_idx, 1);
        assert!(!app.follow_latest_log);
        assert_eq!(app.last_command_output.as_deref(), Some("OFFER-PAYLOAD-1"));

        // Now press 'p' (ToggleAutoPoll) which pushes a new log at index 0:
        app.handle_key_event(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        // Pinned selection shifts to index 2 (`offer-1`) and output remains `"OFFER-PAYLOAD-1"`!
        assert_eq!(app.selected_log_idx, 2);
        assert_eq!(app.last_command_output.as_deref(), Some("OFFER-PAYLOAD-1"));

        // Verify F1: when `follow_latest_log = true` and logs contain `[P0 (req_id=0), RPC2 (req_id=2)]`,
        // a late out-of-order `RPC1 (req_id=1)` is inserted AFTER `RPC2` and never desynchronizes
        // `selected_log_idx` / `selected_log_id` from `last_command_output`.
        app.follow_latest_log = true;
        app.commands_focus = crate::app::CommandsFocus::Workbench;
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 20,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "createOffer",
                summary: "rpc-20".into(),
                output: Some("RPC-20-OUT".into()),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: 2,
            },
        });
        app.handle_key_event(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.action_logs[0].command, "autoPoll");
        assert_eq!(app.action_logs[1].request_id, 20);
        // Late completion of request_id = 19 (< 20):
        app.apply_outcome(DispatchedOutcome {
            meta: RequestMeta {
                client_epoch: 1,
                request_id: 19,
            },
            outcome: CommandOutcome::CommandSuccess {
                command: "createOffer",
                summary: "rpc-19-late".into(),
                output: Some("RPC-19-STALE-OUT".into()),
                measurement: None,
                dht_lookup: None,
                registered_service: None,
                duration_ms: 9,
            },
        });
        assert_eq!(app.action_logs[0].command, "autoPoll");
        assert_eq!(app.action_logs[1].request_id, 20);
        assert_eq!(app.action_logs[2].request_id, 19);
        assert_eq!(app.selected_log_idx, 0);
        assert_eq!(app.selected_log_id, Some(app.action_logs[0].id));
        assert_eq!(
            app.last_command_output.as_deref(),
            Some(app.action_logs[0].formatted_output().as_str())
        );

        // Verify F2: 70,000-line output (> u16::MAX) can scroll all the way to line 70,000.
        let huge_output = (1..=70_000_u32)
            .map(|i| format!("LINE-{i:05}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.last_command_output = Some(huge_output);
        app.active_tab = TabId::Commands;
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw huge output");
        assert!(app.output_max_scroll > usize::from(u16::MAX));
        app.output_scroll = app.output_max_scroll;
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw huge output at bottom");
        let bottom_screen = buffer_to_string(&terminal);
        assert!(bottom_screen.contains("LINE-70000"));

        // Verify R12: when command queue is full, submitting a modal keeps the modal open and preserves input.
        for i in 0..crate::app::MAX_PENDING_COMMANDS {
            app.trigger_button(ButtonId::LookupService);
            if let Some(m) = &mut app.modal {
                m.fields[0].value = format!("svc-{i}");
            }
            app.submit_modal();
        }
        app.trigger_button(ButtonId::CreateOffer);
        if let Some(m) = &mut app.modal {
            m.fields[0].value = "0xpreserve_me".to_string();
        }
        app.submit_modal();
        let kept_modal = app
            .modal
            .as_ref()
            .expect("modal must remain open when queue is full");
        assert_eq!(kept_modal.fields[0].value, "0xpreserve_me");
        assert!(kept_modal
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Command queue is full"));

        // Verify inline seed document redaction and secret field masking.
        assert_eq!(
            crate::client::describe_seed_source(
                r#"{"peers":[{"did":"0x1111111111111111111111111111111111111111","url":"http://127.0.0.1:50001","api_token":"TOP_SECRET"}]}"#
            ),
            "<inline seed document>"
        );
        assert_eq!(
            crate::client::describe_seed_source("~/.rings/seed.json"),
            "~/.rings/seed.json"
        );
        assert_eq!(
            ui::visible_field_slice_with_cursor_masked("s3cr3t", 3, 20, true, true),
            "***█***"
        );
        app.modal = None;
        while app.pop_pending_command().is_some() {}
        app.trigger_button(ButtonId::ConnectUrl);
        if let Some(m) = &mut app.modal {
            assert!(m.fields[1].secret);
            m.fields[1].value = "super-secret-bearer".to_string();
            m.fields[1].cursor = m.fields[1].grapheme_count();
        }
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .expect("draw connect url modal");
        let modal_screen = buffer_to_string(&terminal);
        assert!(!modal_screen.contains("super-secret-bearer"));
    }
}
