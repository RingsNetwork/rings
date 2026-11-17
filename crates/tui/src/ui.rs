//! Terminal User Interface rendering with `ratatui` and mouse hit-region registration.

use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Cell;
use ratatui::widgets::Clear;
use ratatui::widgets::Gauge;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Row;
use ratatui::widgets::Sparkline;
use ratatui::widgets::Table;
use ratatui::widgets::TableState;
use ratatui::widgets::Wrap;
use ratatui::Frame;

use crate::action::ButtonId;
use crate::action::ClickTarget;
use crate::action::CommandModal;
use crate::action::TabId;
use crate::app::App;
use crate::app::ConnectionState;
use crate::app::DhtSubtab;
use crate::app::DirectorySubtab;
use crate::types::DhtLookupStatus;
use crate::types::GatewayProbeState;

/// Render the complete TUI frame and populate `app.hit_map` for mouse interactions.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    app.hit_map.clear();
    let area = frame.area();

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header: node identity & live connection badge
            Constraint::Length(3), // Navigation tabs (clickable)
            Constraint::Length(3), // Interactive button command toolbar (clickable)
            Constraint::Min(10),   // Main view body
            Constraint::Length(2), // Footer status bar & latest log notification
        ])
        .split(area);

    if let Some(&header_rect) = root.first() {
        render_header(frame, app, header_rect);
    }
    if let Some(&tabs_rect) = root.get(1) {
        render_tab_bar(frame, app, tabs_rect);
    }
    if let Some(&toolbar_rect) = root.get(2) {
        render_button_toolbar(frame, app, toolbar_rect);
    }
    if let Some(&body_rect) = root.get(3) {
        match app.active_tab {
            TabId::Overview => render_overview_tab(frame, app, body_rect),
            TabId::Monitor => render_monitor_tab(frame, app, body_rect),
            TabId::Peers => render_peers_tab(frame, app, body_rect),
            TabId::DhtStorage => render_dht_storage_tab(frame, app, body_rect),
            TabId::Directory => render_directory_tab(frame, app, body_rect),
            TabId::Commands => render_commands_tab(frame, app, body_rect),
        }
    }
    if let Some(&footer_rect) = root.get(4) {
        render_footer(frame, app, footer_rect);
    }

    if let Some(modal) = app.modal.clone() {
        render_modal(frame, app, &modal, area);
    }
    if app.show_help {
        render_help_overlay(frame, area);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (status_badge, status_color) = match &app.connection_state {
        ConnectionState::Connecting => (" CONNECTING ", Color::Yellow),
        ConnectionState::Online => (" ONLINE ", Color::Green),
        ConnectionState::Offline(_) => (" OFFLINE ", Color::Red),
    };

    let overlay_ready = app
        .snapshot
        .observability
        .as_ref()
        .map(|o| o.health.overlay_ready);
    let (ready_badge, ready_color) = match overlay_ready {
        Some(true) => (" OVERLAY READY ", Color::Cyan),
        Some(false) => (" OVERLAY DEGRADED ", Color::Yellow),
        None => (" OVERLAY UNKNOWN ", Color::DarkGray),
    };

    let did = app.snapshot.did.as_deref().unwrap_or("unconnected");
    let version = app.snapshot.version.as_deref().unwrap_or("-");
    let wire_ver = app
        .snapshot
        .wire_version
        .map(|w| format!("v{w}"))
        .unwrap_or_else(|| "-".to_string());
    let uptime = app
        .snapshot
        .uptime_secs()
        .map(format_duration_secs)
        .unwrap_or_else(|| "-".to_string());
    let poll_badge = if app.auto_poll {
        format!("AUTO {}ms", app.config.refresh_interval.as_millis())
    } else {
        "PAUSED".to_string()
    };

    let line = Line::from(vec![
        Span::styled(
            " RINGS-TUI ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            status_badge,
            Style::default()
                .fg(Color::Black)
                .bg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            ready_badge,
            Style::default()
                .fg(Color::Black)
                .bg(ready_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("DID: ", Style::default().fg(Color::Gray)),
        Span::styled(
            did,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("Ver: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{version} (wire {wire_ver})"),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled("Uptime: ", Style::default().fg(Color::Gray)),
        Span::styled(uptime, Style::default().fg(Color::Green)),
        Span::raw("  "),
        Span::styled("Poll: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{poll_badge} ({}ms)", app.snapshot.poll_latency_ms),
            Style::default().fg(Color::LightCyan),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Endpoint: {} ", app.config.endpoint_url))
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn render_tab_bar(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Views (Click or press 1-6 / Tab) ")
        .border_style(Style::default().fg(Color::Blue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let constraints = TabId::ALL
        .iter()
        .map(|_| Constraint::Ratio(1, TabId::ALL.len() as u32))
        .collect::<Vec<_>>();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(inner);

    for (tab, &rect) in TabId::ALL.iter().copied().zip(cols.iter()) {
        app.hit_map.register(rect, ClickTarget::Tab(tab));
        let is_active = app.active_tab == tab;
        let style = if is_active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        };
        let label = format!(" {} ", tab.title());
        let p = Paragraph::new(label)
            .style(style)
            .alignment(Alignment::Center);
        frame.render_widget(p, rect);
    }
}

fn render_button_toolbar(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Quick Command Buttons (Mouse Click, Hotkey, or [B] + Arrows + Enter) ")
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let count = ButtonId::TOOLBAR.len();
    let constraints = (0..count)
        .map(|_| Constraint::Ratio(1, count as u32))
        .collect::<Vec<_>>();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(inner);

    for (idx, (&btn, &rect)) in ButtonId::TOOLBAR.iter().zip(cols.iter()).enumerate() {
        app.hit_map.register(rect, ClickTarget::Button(btn));
        let is_focused = app.focused_toolbar_idx == Some(idx);
        let style = if is_focused {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if btn == ButtonId::Quit {
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD)
        } else if btn == ButtonId::ToggleAutoPoll && !app.auto_poll {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };
        let p = Paragraph::new(btn.toolbar_label())
            .style(style)
            .alignment(Alignment::Center);
        frame.render_widget(p, rect);
    }
}

fn render_overview_tab(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Top row: Health, Delegation Key, Gateway & Mailboxes
            Constraint::Length(6), // Middle row: Message Totals & DHT Lookup Summary
            Constraint::Min(5),    // Bottom row: Recent Message Activity & Latest Command Output
        ])
        .split(area);

    if let Some(&top_rect) = rows.first() {
        let top_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(top_rect);

        if let Some(&col0) = top_cols.first() {
            render_health_card(frame, app, col0);
        }
        if let Some(&col1) = top_cols.get(1) {
            render_session_key_card(frame, app, col1);
        }
        if let Some(&col2) = top_cols.get(2) {
            render_gateway_and_mailbox_card(frame, app, col2);
        }
    }

    if let Some(&mid_rect) = rows.get(1) {
        let mid_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(mid_rect);

        if let Some(&left) = mid_cols.first() {
            render_message_totals_card(frame, app, left);
        }
        if let Some(&right) = mid_cols.get(1) {
            render_lookup_summary_card(frame, app, right);
        }
    }

    if let Some(&bot_rect) = rows.get(2) {
        let bot_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(bot_rect);

        if let Some(&left) = bot_cols.first() {
            render_overview_connected_peers(frame, app, left);
        }
        if let Some(&right) = bot_cols.get(1) {
            render_overview_service_consumers(frame, app, right);
        }
    }
}

fn render_overview_connected_peers(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let peer_rows = app.snapshot.peer_traffic_rows();
    let header = Row::new(vec![
        Cell::from("Peer DID"),
        Cell::from("Chord Role"),
        Cell::from("State"),
        Cell::from("Traffic (TX / RX)"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows = peer_rows
        .iter()
        .map(|peer| {
            let traffic = format!(
                "{} / {}",
                format_bytes(peer.bytes_tx),
                format_bytes(peer.bytes_rx)
            );
            Row::new(vec![
                Cell::from(peer.peer_did.clone()),
                Cell::from(peer.chord_role.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(peer.transport_state.clone()).style(Style::default().fg(Color::Green)),
                Cell::from(traffic),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [
        Constraint::Min(22),
        Constraint::Length(14),
        Constraint::Length(11),
        Constraint::Length(18),
    ])
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " Connected Peers & Traffic ({}) — [3:Peers] ",
                peer_rows.len()
            ))
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(table, area);
}

fn render_overview_service_consumers(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let vnodes = app.snapshot.hosted_vnode_rows();
    let svcs = if app.registered_services.is_empty() {
        "none".to_string()
    } else {
        app.registered_services.join(",")
    };

    let header = Row::new(vec![
        Cell::from("Entry Key DID (VNode)"),
        Cell::from("Tier / Kind"),
        Cell::from("Elem"),
        Cell::from("Storage Key"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows = vnodes
        .iter()
        .map(|v| {
            Row::new(vec![
                Cell::from(v.entry_did.clone()),
                Cell::from(format!("{}/{}", v.storage_tier, v.entry_kind))
                    .style(Style::default().fg(Color::Magenta)),
                Cell::from(format!("{}", v.element_count)).style(Style::default().fg(Color::Cyan)),
                Cell::from(v.storage_key.clone()).style(Style::default().fg(Color::Green)),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [
        Constraint::Min(22),
        Constraint::Length(16),
        Constraint::Length(6),
        Constraint::Length(16),
    ])
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " Hosted VNodes ({}) | Local Svc: [{svcs}] ",
                vnodes.len()
            ))
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(table, area);
}

fn render_health_card(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let obs = app.snapshot.observability.as_ref();
    let health = obs.map(|o| &o.health);
    let dht = app.snapshot.swarm.as_ref().and_then(|s| s.dht.as_ref());

    let api_ok = health.map(|h| h.process_api_healthy).unwrap_or(false);
    let overlay_ok = health.map(|h| h.overlay_ready).unwrap_or(false);
    let admitted = health
        .map(|h| h.admitted_peer_count)
        .unwrap_or_else(|| app.snapshot.peers.len() as u64);
    let has_succ = health
        .map(|h| h.has_successor)
        .unwrap_or_else(|| dht.is_some_and(|d| !d.successors.is_empty()));
    let has_pred = health
        .map(|h| h.has_predecessor)
        .unwrap_or_else(|| dht.is_some_and(|d| d.predecessor.is_some()));

    let lines = vec![
        kv_line(
            "Process API:",
            if api_ok { "HEALTHY" } else { "UNREACHABLE" },
            if api_ok { Color::Green } else { Color::Red },
        ),
        kv_line(
            "Overlay Ready:",
            if overlay_ok { "YES" } else { "NO" },
            if overlay_ok {
                Color::Green
            } else {
                Color::Yellow
            },
        ),
        kv_line(
            "Admitted Peers:",
            &format!("{admitted} (listed: {})", app.snapshot.peers.len()),
            Color::Cyan,
        ),
        kv_line(
            "Chord Ring:",
            &format!(
                "Succ: {} | Pred: {}",
                if has_succ { "YES" } else { "NO" },
                if has_pred { "YES" } else { "NO" }
            ),
            if has_succ {
                Color::Green
            } else {
                Color::Yellow
            },
        ),
        kv_line(
            "Online / Exits:",
            &format!(
                "{} online | {} onion exits",
                app.snapshot.online_nodes.len(),
                app.snapshot.onion_exits.len()
            ),
            Color::LightBlue,
        ),
        kv_line(
            "Network Roles:",
            &format!(
                "net_id={} relay={} exit={}",
                app.config
                    .network_id
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                app.config.advertise_onion_relay,
                app.config.advertise_onion_exit
            ),
            Color::Gray,
        ),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Node & Overlay Health ")
        .border_style(Style::default().fg(Color::Green));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_session_key_card(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Session Delegation Key ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(inner);

    let sk = app.snapshot.observability.as_ref().map(|o| &o.session_key);
    let ratio = sk.map(|s| s.remaining_ratio()).unwrap_or(0.0);
    let remaining_str = sk
        .map(|s| format_duration_secs(u64::try_from(s.remaining_ms / 1_000).unwrap_or(0)))
        .unwrap_or_else(|| "N/A".to_string());
    let valid = sk.map(|s| s.valid).unwrap_or(false);

    if let Some(&gauge_rect) = chunks.first() {
        let gauge = Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(if valid { Color::Green } else { Color::Red })
                    .bg(Color::DarkGray),
            )
            .ratio(ratio)
            .label(format!(
                "TTL Remaining: {remaining_str} ({:.0}%)",
                ratio * 100.0
            ));
        frame.render_widget(gauge, gauge_rect);
    }

    if let Some(&info_rect) = chunks.get(1) {
        let lines = match sk {
            Some(s) => vec![
                kv_line(
                    "Status:",
                    if s.valid { "VALID" } else { "EXPIRED" },
                    if s.valid { Color::Green } else { Color::Red },
                ),
                kv_line(
                    "Auto-Rotation:",
                    if s.runtime_rotation_supported {
                        "ENABLED (delegator_key present)"
                    } else {
                        "DISABLED (fixed horizon)"
                    },
                    if s.runtime_rotation_supported {
                        Color::Cyan
                    } else {
                        Color::Yellow
                    },
                ),
                kv_line(
                    "Rotations:",
                    &format!(
                        "{} ok / {} failed",
                        s.rotation_succeeded_total, s.rotation_failed_total
                    ),
                    Color::White,
                ),
            ],
            None => vec![Line::from(Span::styled(
                "Waiting for /status?view=observability...",
                Style::default().fg(Color::DarkGray),
            ))],
        };
        frame.render_widget(Paragraph::new(lines), info_rect);
    }
}

fn render_gateway_and_mailbox_card(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mb = app
        .snapshot
        .observability
        .as_ref()
        .map(|o| o.mailboxes)
        .unwrap_or_default();

    let mut lines = match &app.snapshot.gateway {
        GatewayProbeState::Available(gw) => {
            let color = match gw.health.as_str() {
                "active" => Color::Green,
                "degraded" | "transitioning" => Color::Yellow,
                "failed" => Color::Red,
                _ => Color::Gray,
            };
            vec![
                kv_line("Gateway Health:", &gw.health.to_uppercase(), color),
                kv_line(
                    "Interface / Flows:",
                    &format!(
                        "{} | {} active flows",
                        gw.interface.as_deref().unwrap_or("-"),
                        gw.active_flows
                    ),
                    Color::Cyan,
                ),
                kv_line(
                    "Onion Exit:",
                    &format!(
                        "{} {}",
                        gw.exit_availability,
                        gw.reason
                            .as_deref()
                            .map(|r| format!("({r})"))
                            .unwrap_or_default()
                    ),
                    Color::White,
                ),
            ]
        }
        GatewayProbeState::Unknown | GatewayProbeState::NotMounted => vec![
            kv_line("TUN Gateway:", "NOT MOUNTED (404)", Color::DarkGray),
            kv_line(
                "HTTP Proxy:",
                app.config
                    .onion_http_proxy_addr
                    .as_deref()
                    .unwrap_or("disabled"),
                Color::Cyan,
            ),
        ],
        GatewayProbeState::Failed(err) => vec![
            kv_line("TUN Gateway:", "PROBE ERROR", Color::Red),
            kv_line("Error:", err, Color::LightRed),
        ],
    };

    lines.push(kv_line(
        "Relay Mailboxes:",
        &format!(
            "{} inboxes | {} held | {} total",
            mb.registered, mb.held_messages, mb.stored_total
        ),
        Color::LightMagenta,
    ));
    lines.push(kv_line(
        "External API:",
        app.config
            .external_api_addr
            .as_deref()
            .unwrap_or("127.0.0.1:50001"),
        Color::Gray,
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Gateway & Relay Mailboxes ")
        .border_style(Style::default().fg(Color::Magenta));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_message_totals_card(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let totals = app
        .snapshot
        .observability
        .as_ref()
        .map(|o| o.messages)
        .unwrap_or_default();

    let lines = vec![
        Line::from(vec![
            Span::styled(" Sent: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:<10}", totals.sent),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Received: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:<10}", totals.received),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Forwarded: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:<10}", totals.forwarded),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Stored (Mail): ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:<10}", totals.stored),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Failed: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:<10}", totals.failed),
                Style::default()
                    .fg(if totals.failed == 0 {
                        Color::Gray
                    } else {
                        Color::Red
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            " Tip: Switch to [2:Monitor] for real-time throughput sparklines & latency histogram",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Process Message Counters ")
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_lookup_summary_card(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lookups = app.snapshot.observability.as_ref().map(|o| &o.dht_lookups);

    let lines = match lookups {
        Some(l) => vec![
            Line::from(vec![
                Span::styled(" Total: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{:<8}", l.total),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Succeeded: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{:<8}", l.succeeded),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" In-Flight: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{:<6}", l.in_flight),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Failed: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{:<7}", l.failed),
                    Style::default().fg(if l.failed == 0 {
                        Color::Gray
                    } else {
                        Color::Red
                    }),
                ),
                Span::styled(" Timed Out: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{:<8}", l.timed_out),
                    Style::default().fg(if l.timed_out == 0 {
                        Color::Gray
                    } else {
                        Color::LightRed
                    }),
                ),
                Span::styled(" Avg Latency: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!(
                        "{:.1} ms ({:.0}% ok)",
                        l.avg_latency_ms(),
                        l.success_rate_pct()
                    ),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
        ],
        None => vec![Line::from(Span::styled(
            "No DHT lookup metrics collected yet",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Chord DHT Lookup Telemetry ")
        .border_style(Style::default().fg(Color::Blue));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_monitor_tab(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // Top: 4 Sparklines (Sent, Received, Forwarded, Failed)
            Constraint::Length(10), // Middle: DHT Lookup Latency Histogram
            Constraint::Min(6),     // Bottom: Recent Message Event Stream
        ])
        .split(area);

    if let Some(&spark_rect) = rows.first() {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(spark_rect);

        let totals = app
            .snapshot
            .observability
            .as_ref()
            .map(|o| o.messages)
            .unwrap_or_default();

        if let Some(&c0) = cols.first() {
            let data = app.history.sent_series();
            let spark = Sparkline::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Sent (total: {}) ", totals.sent)),
                )
                .data(&data)
                .style(Style::default().fg(Color::Green));
            frame.render_widget(spark, c0);
        }
        if let Some(&c1) = cols.get(1) {
            let data = app.history.received_series();
            let spark = Sparkline::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Received (total: {}) ", totals.received)),
                )
                .data(&data)
                .style(Style::default().fg(Color::Cyan));
            frame.render_widget(spark, c1);
        }
        if let Some(&c2) = cols.get(2) {
            let data = app.history.forwarded_series();
            let spark = Sparkline::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Forwarded (total: {}) ", totals.forwarded)),
                )
                .data(&data)
                .style(Style::default().fg(Color::Blue));
            frame.render_widget(spark, c2);
        }
        if let Some(&c3) = cols.get(3) {
            let data = app.history.failed_series();
            let spark = Sparkline::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Failed (total: {}) ", totals.failed)),
                )
                .data(&data)
                .style(Style::default().fg(Color::Red));
            frame.render_widget(spark, c3);
        }
    }

    if let Some(&hist_rect) = rows.get(1) {
        render_lookup_histogram(frame, app, hist_rect);
    }

    if let Some(&events_rect) = rows.get(2) {
        render_recent_events_table(frame, app, events_rect);
    }
}

fn render_lookup_histogram(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(lookups) = app.snapshot.observability.as_ref().map(|o| &o.dht_lookups) else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" DHT Lookup Latency Distribution ");
        frame.render_widget(
            Paragraph::new("No lookup histogram available").block(block),
            area,
        );
        return;
    };

    let buckets = lookups.non_cumulative_buckets();
    let max_count = buckets.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let mid = buckets.len().div_ceil(2);
    let (left_slice, right_slice) = buckets.split_at(mid.min(buckets.len()));

    for (idx, (slice, rect_opt)) in [(left_slice, cols.first()), (right_slice, cols.get(1))]
        .into_iter()
        .enumerate()
    {
        let Some(&rect) = rect_opt else {
            continue;
        };
        let lines = slice
            .iter()
            .map(|(label, count)| {
                let bar_width = usize::try_from((*count).saturating_mul(20) / max_count)
                    .unwrap_or(0)
                    .max(usize::from(*count > 0));
                let bar = "█".repeat(bar_width);
                Line::from(vec![
                    Span::styled(
                        format!(" {:>7} │ ", label),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(format!("{:<20}", bar), Style::default().fg(Color::Cyan)),
                    Span::styled(format!(" {count}"), Style::default().fg(Color::Yellow)),
                ])
            })
            .collect::<Vec<_>>();

        let title = if idx == 0 {
            format!(
                " DHT Latency Histogram (Fast: avg {:.1}ms) ",
                lookups.avg_latency_ms()
            )
        } else {
            format!(
                " DHT Latency Histogram (Slow: in-flight={}, timeout={}) ",
                lookups.in_flight, lookups.timed_out
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Blue));
        frame.render_widget(Paragraph::new(lines).block(block), rect);
    }
}

fn render_recent_events_table(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let events = app
        .snapshot
        .observability
        .as_ref()
        .map(|o| o.recent_messages.as_slice())
        .unwrap_or(&[]);

    let header = Row::new(vec![
        Cell::from("Seq"),
        Cell::from("Age"),
        Cell::from("Action"),
        Cell::from("Category"),
        Cell::from("Message Class"),
        Cell::from("Outcome"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let now_ms = app
        .snapshot
        .observability
        .as_ref()
        .map(|o| o.generated_at_ms)
        .unwrap_or(app.snapshot.fetched_at_ms);

    // Display newest events first.
    let rows = events
        .iter()
        .rev()
        .map(|ev| {
            let age_ms = now_ms.saturating_sub(ev.observed_at_ms);
            let age_str = if age_ms < 1_000 {
                format!("{age_ms}ms ago")
            } else {
                format!("{}s ago", age_ms / 1_000)
            };
            let action_color = match ev.action.as_str() {
                "sent" => Color::Green,
                "received" => Color::Cyan,
                "forwarded" => Color::Blue,
                "stored" => Color::Magenta,
                _ => Color::White,
            };
            let outcome_color = if ev.outcome == "succeeded" {
                Color::Green
            } else {
                Color::Red
            };
            Row::new(vec![
                Cell::from(format!("#{}", ev.sequence)),
                Cell::from(age_str),
                Cell::from(ev.action.clone()).style(Style::default().fg(action_color)),
                Cell::from(ev.category.clone()),
                Cell::from(ev.message_class.clone()),
                Cell::from(ev.outcome.clone()).style(Style::default().fg(outcome_color)),
            ])
        })
        .collect::<Vec<_>>();

    let event_count = events.len();
    register_table_row_hits(&mut app.hit_map, area, event_count);

    let mut state = TableState::default();
    if event_count > 0 {
        state.select(Some(app.selected_event_idx));
    }

    let table = Table::new(rows, [
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(14),
        Constraint::Min(18),
        Constraint::Length(11),
    ])
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Recent Message Activity ({event_count}) "))
            .border_style(Style::default().fg(Color::Cyan)),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(table, area, &mut state);
}

fn render_peers_tab(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(55), // Top: Peer Transport & Traffic Table (merged in domain model)
            Constraint::Percentage(45), // Bottom: Hosted VNode Storage Entries & Local Capabilities
        ])
        .split(area);

    let header = Row::new(vec![
        Cell::from("Peer DID"),
        Cell::from("Chord Role"),
        Cell::from("State"),
        Cell::from("Reliability"),
        Cell::from("Credit"),
        Cell::from("Sent (OK/Fail)"),
        Cell::from("Recv (OK/Fail)"),
        Cell::from("Bytes TX / RX"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let peer_rows = app.snapshot.peer_traffic_rows();
    let row_count = peer_rows.len();
    let rows = peer_rows
        .iter()
        .map(|peer| {
            let rel_color = match peer.reliability.as_str() {
                "healthy" => Color::Green,
                "degraded" => Color::Red,
                _ => Color::Yellow,
            };
            let credit_str = peer
                .credit_score
                .map(|s| format!("{s:.2}"))
                .unwrap_or_else(|| "-".to_string());
            let bytes_str = format!(
                "{} / {}",
                format_bytes(peer.bytes_tx),
                format_bytes(peer.bytes_rx)
            );
            Row::new(vec![
                Cell::from(peer.peer_did.clone()),
                Cell::from(peer.chord_role.clone()).style(Style::default().fg(Color::LightCyan)),
                Cell::from(peer.transport_state.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(peer.reliability.clone()).style(Style::default().fg(rel_color)),
                Cell::from(credit_str),
                Cell::from(format!("{}/{}", peer.sent_ok, peer.sent_fail)),
                Cell::from(format!("{}/{}", peer.recv_ok, peer.recv_fail)),
                Cell::from(bytes_str),
            ])
        })
        .collect::<Vec<_>>();

    if let Some(&table_rect) = chunks.first() {
        register_table_row_hits(&mut app.hit_map, table_rect, row_count);
        let mut state = TableState::default();
        if row_count > 0 {
            state.select(Some(app.selected_peer_idx));
        }

        let table = Table::new(
            rows,
            [
                Constraint::Min(24),
                Constraint::Length(14),
                Constraint::Length(11),
                Constraint::Length(11),
                Constraint::Length(7),
                Constraint::Length(13),
                Constraint::Length(13),
                Constraint::Length(18),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Connected & Measured Peers ({}) — [Enter]/[I] Inspect, [C] +URL, [D] +DID, [X] Disconnect ",
                    row_count,
                ))
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(table, table_rect, &mut state);
    }

    if let Some(&vnodes_rect) = chunks.get(1) {
        let vnodes = app.snapshot.hosted_vnode_rows();
        let svcs_str = if app.registered_services.is_empty() {
            "None (press [G] to register)".to_string()
        } else {
            app.registered_services.join(", ")
        };
        let vnode_rows = vnodes
            .iter()
            .map(|v| {
                Row::new(vec![
                    Cell::from(v.storage_key.clone()),
                    Cell::from(v.storage_tier.clone()).style(Style::default().fg(Color::Green)),
                    Cell::from(v.entry_did.clone()).style(Style::default().fg(Color::Cyan)),
                    Cell::from(v.entry_kind.clone()).style(Style::default().fg(Color::Magenta)),
                    Cell::from(format!("{}", v.element_count)),
                ])
            })
            .collect::<Vec<_>>();

        let vnode_table = Table::new(
            vnode_rows,
            [
                Constraint::Length(28),
                Constraint::Length(14),
                Constraint::Min(30),
                Constraint::Length(12),
                Constraint::Length(10),
            ],
        )
        .header(
            Row::new(vec![
                "Storage Key",
                "Tier",
                "Entry Key DID (VNode)",
                "Kind",
                "Elements",
            ])
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Hosted VNode Records ({}) | Local Registered Services: [{}] (Relay={} Exit={}) ",
                    vnodes.len(),
                    svcs_str,
                    app.config.advertise_onion_relay,
                    app.config.advertise_onion_exit
                ))
                .border_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(vnode_table, vnodes_rect);
    }
}

fn render_dht_storage_tab(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // Top: Chord Ring Topology + [ / ] DHT Lookup Key button
            Constraint::Length(7), // Middle: DHT Key Lookup Inspector
            Constraint::Min(6),    // Bottom: Finger Table / Persistent Storage / Cache Storage
        ])
        .split(area);

    let dht = app.snapshot.swarm.as_ref().and_then(|s| s.dht.as_ref());
    let pred = dht.and_then(|d| d.predecessor.as_deref()).unwrap_or("None");
    let succs = dht
        .map(|d| {
            if d.successors.is_empty() {
                "None".to_string()
            } else {
                d.successors.join(", ")
            }
        })
        .unwrap_or_else(|| "None".to_string());
    let p_count = app
        .snapshot
        .swarm
        .as_ref()
        .and_then(|s| s.persistence_storage.as_ref())
        .map(|st| st.items.len())
        .unwrap_or(0);
    let c_count = app
        .snapshot
        .swarm
        .as_ref()
        .and_then(|s| s.cache_storage.as_ref())
        .map(|st| st.items.len())
        .unwrap_or(0);

    if let Some(&top_rect) = chunks.first() {
        app.hit_map.register(top_rect, ClickTarget::SubViewToggle);
        let lines = vec![
            kv_line("Predecessor DID:", pred, Color::Cyan),
            kv_line("Successors:", &succs, Color::Green),
            Line::from(vec![
                Span::styled(
                    " Sub-View ([Space]/[Left]/[Right]): ",
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    format!(" [ {} ] ", app.dht_subtab.title()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  (Persistent: {p_count} | Cache: {c_count})   ")),
                Span::styled(
                    " [Press '/' or 'W' or Enter to Query DHT Key] ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Chord DHT Ring Topology & Key Lookup ")
            .border_style(Style::default().fg(Color::Green));
        frame.render_widget(Paragraph::new(lines).block(block), top_rect);
    }

    if let Some(&lookup_rect) = chunks.get(1) {
        app.hit_map
            .register(lookup_rect, ClickTarget::Button(ButtonId::DhtLookupKey));
        let lines = match &app.last_dht_lookup {
            Some(rep) => {
                let sources_str = if rep.sources.is_empty() {
                    rep.status.label().to_string()
                } else {
                    rep.sources.join(" + ")
                };
                let status_color = match rep.status {
                    DhtLookupStatus::Found => Color::Green,
                    DhtLookupStatus::Absent => Color::Yellow,
                    DhtLookupStatus::PartialError => Color::Red,
                };
                let preview = if rep.elements.is_empty() {
                    rep.warning
                        .clone()
                        .unwrap_or_else(|| "<no elements>".to_string())
                } else {
                    rep.elements.join("  |  ")
                };
                vec![
                    Line::from(vec![
                        Span::styled(" Query Key: ", Style::default().fg(Color::Gray)),
                        Span::styled(
                            rep.query_key.clone(),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("   -> Entry Key DID (VNode): ", Style::default().fg(Color::Gray)),
                        Span::styled(
                            rep.target_did.clone(),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled(" Status: ", Style::default().fg(Color::Gray)),
                        Span::styled(rep.status.label(), Style::default().fg(status_color)),
                        Span::styled("   Found In: ", Style::default().fg(Color::Gray)),
                        Span::styled(sources_str, Style::default().fg(Color::Green)),
                        Span::styled("   Kind: ", Style::default().fg(Color::Gray)),
                        Span::styled(rep.record_kind.clone(), Style::default().fg(Color::Magenta)),
                        Span::styled("   Mode: ", Style::default().fg(Color::Gray)),
                        Span::styled(rep.query_mode.clone(), Style::default().fg(Color::White)),
                    ]),
                    Line::from(vec![
                        Span::styled(
                            format!(" Elements ({}): ", rep.elements.len()),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::styled(preview, Style::default().fg(Color::White)),
                    ]),
                ]
            }
            None => vec![
                Line::from(Span::styled(
                    " Click here or press [/] (or [W] / [Enter]) to look up any key or DID in the Chord DHT!",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    " Supports topic names (e.g. 'rings-chat'), service names (e.g. 'ipfs-gateway'), registries ('online_nodes'), or raw 0x... Entry DIDs.",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(Span::styled(
                    " Computes the 160-bit SHA-1 Entry DID (or inspects raw 0x... VNode DID directly without double-hashing).",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" DHT Key Lookup Inspector — Click or press [/] to Query Key ")
            .border_style(Style::default().fg(Color::Yellow));
        frame.render_widget(Paragraph::new(lines).block(block), lookup_rect);
    }

    if let Some(&bot_rect) = chunks.get(2) {
        match app.dht_subtab {
            DhtSubtab::FingerTable => {
                let ranges = app
                    .snapshot
                    .swarm
                    .as_ref()
                    .and_then(|s| s.dht.as_ref())
                    .map(|d| d.finger_table_ranges.as_slice())
                    .unwrap_or(&[]);
                let range_count = ranges.len();
                let rows = ranges
                    .iter()
                    .map(|r| {
                        Row::new(vec![
                            Cell::from(format!("{}", r.start)),
                            Cell::from(format!("{}", r.end)),
                            Cell::from(format!("{}", r.end.saturating_sub(r.start) + 1)),
                            Cell::from(r.did.clone().unwrap_or_else(|| "<empty>".to_string()))
                                .style(Style::default().fg(if r.did.is_some() {
                                    Color::Cyan
                                } else {
                                    Color::DarkGray
                                })),
                        ])
                    })
                    .collect::<Vec<_>>();
                register_table_row_hits(&mut app.hit_map, bot_rect, range_count);
                let mut state = TableState::default();
                if range_count > 0 {
                    state.select(Some(app.selected_dht_idx));
                }
                let table = Table::new(rows, [
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Min(30),
                ])
                .header(
                    Row::new(vec!["Start", "End", "Span", "Target Peer DID"]).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Finger Table Ranges ({range_count}) "))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .row_highlight_style(Style::default().bg(Color::DarkGray));
                frame.render_stateful_widget(table, bot_rect, &mut state);
            }
            DhtSubtab::PersistentStorage | DhtSubtab::CacheStorage => {
                let items = app
                    .snapshot
                    .swarm
                    .as_ref()
                    .and_then(|s| match app.dht_subtab {
                        DhtSubtab::PersistentStorage => s.persistence_storage.as_ref(),
                        _ => s.cache_storage.as_ref(),
                    })
                    .map(|st| st.items.as_slice())
                    .unwrap_or(&[]);
                let item_count = items.len();
                let rows = items
                    .iter()
                    .map(|item| {
                        let (did, kind, elem_count) = match &item.value {
                            Some(v) => (v.did.as_str(), v.kind.as_str(), v.data.len()),
                            None => ("-", "empty", 0),
                        };
                        Row::new(vec![
                            Cell::from(item.key.clone()),
                            Cell::from(did.to_string()),
                            Cell::from(kind.to_string()).style(Style::default().fg(Color::Cyan)),
                            Cell::from(format!("{elem_count} element(s)")),
                        ])
                    })
                    .collect::<Vec<_>>();
                register_table_row_hits(&mut app.hit_map, bot_rect, item_count);
                let mut state = TableState::default();
                if item_count > 0 {
                    state.select(Some(app.selected_dht_idx));
                }
                let table = Table::new(rows, [
                    Constraint::Length(28),
                    Constraint::Length(30),
                    Constraint::Length(18),
                    Constraint::Min(14),
                ])
                .header(
                    Row::new(vec![
                        "Storage Key",
                        "Entry Key DID (VNode)",
                        "Kind",
                        "Elements",
                    ])
                    .style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ({item_count} items) ", app.dht_subtab.title()))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .row_highlight_style(Style::default().bg(Color::DarkGray));
                frame.render_stateful_widget(table, bot_rect, &mut state);
            }
        }
    }
}

fn render_directory_tab(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(8)])
        .split(area);

    if let Some(&bar_rect) = chunks.first() {
        app.hit_map.register(bar_rect, ClickTarget::SubViewToggle);
        let line = Line::from(vec![
            Span::styled(
                " Registry Mode (Click or press [Space]): ",
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format!(" [ {} ] ", app.directory_subtab.title()),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("Include Expired ([E]): {}", app.include_expired_directory),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("   "),
            Span::styled(
                "Press [Enter] or [D] to Connect to selected DID",
                Style::default().fg(Color::Green),
            ),
        ]);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue));
        frame.render_widget(Paragraph::new(line).block(block), bar_rect);
    }

    if let Some(&table_rect) = chunks.get(1) {
        match app.directory_subtab {
            DirectorySubtab::OnlineNodes => {
                let node_count = app.snapshot.online_nodes.len();
                let rows = app
                    .snapshot
                    .online_nodes
                    .iter()
                    .map(|n| {
                        let relay = if n.capabilities.onion_relay.is_some() {
                            "relay:yes"
                        } else {
                            "relay:no"
                        };
                        let mail = if n.capabilities.holds_relay_mail {
                            "mail:yes"
                        } else {
                            "mail:no"
                        };
                        Row::new(vec![
                            Cell::from(n.did.clone()),
                            Cell::from(format!("{:?}", n.node_type)),
                            Cell::from(format!("{}", n.network_id)),
                            Cell::from(format!("{relay}, {mail}"))
                                .style(Style::default().fg(Color::Cyan)),
                            Cell::from(format!("vnodes={}", n.dht_virtual_nodes)),
                            Cell::from(n.endpoint_hint.clone().unwrap_or_else(|| "-".to_string())),
                            Cell::from(n.version.clone()),
                        ])
                    })
                    .collect::<Vec<_>>();
                register_table_row_hits(&mut app.hit_map, table_rect, node_count);
                let mut state = TableState::default();
                if node_count > 0 {
                    state.select(Some(app.selected_directory_idx));
                }
                let table = Table::new(rows, [
                    Constraint::Min(28),
                    Constraint::Length(9),
                    Constraint::Length(7),
                    Constraint::Length(20),
                    Constraint::Length(11),
                    Constraint::Length(22),
                    Constraint::Length(12),
                ])
                .header(
                    Row::new(vec![
                        "Node DID",
                        "Runtime",
                        "NetID",
                        "Capabilities",
                        "DHT",
                        "Endpoint Hint",
                        "Version",
                    ])
                    .style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Online Nodes Directory ({node_count}) "))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .row_highlight_style(Style::default().bg(Color::DarkGray));
                frame.render_stateful_widget(table, table_rect, &mut state);
            }
            DirectorySubtab::OnionExits => {
                let exit_count = app.snapshot.onion_exits.len();
                let rows = app
                    .snapshot
                    .onion_exits
                    .iter()
                    .map(|e| {
                        Row::new(vec![
                            Cell::from(e.did.clone()),
                            Cell::from(e.service.clone()).style(Style::default().fg(Color::Green)),
                            Cell::from(format!("{}", e.policy.max_sessions)),
                            Cell::from(format!(
                                "{}/min",
                                format_bytes(e.policy.max_bytes_per_minute)
                            )),
                            Cell::from(format!(
                                "allow:{} deny:{}",
                                e.policy.allowed_targets.len(),
                                e.policy.denied_targets.len()
                            )),
                            Cell::from(e.version.clone()),
                        ])
                    })
                    .collect::<Vec<_>>();
                register_table_row_hits(&mut app.hit_map, table_rect, exit_count);
                let mut state = TableState::default();
                if exit_count > 0 {
                    state.select(Some(app.selected_directory_idx));
                }
                let table = Table::new(rows, [
                    Constraint::Min(28),
                    Constraint::Length(10),
                    Constraint::Length(12),
                    Constraint::Length(16),
                    Constraint::Length(18),
                    Constraint::Length(12),
                ])
                .header(
                    Row::new(vec![
                        "Exit DID",
                        "Service",
                        "Max Sess",
                        "Rate Limit",
                        "Target Policy",
                        "Version",
                    ])
                    .style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" Onion Exits Directory ({exit_count}) "))
                        .border_style(Style::default().fg(Color::Magenta)),
                )
                .row_highlight_style(Style::default().bg(Color::DarkGray));
                frame.render_stateful_widget(table, table_rect, &mut state);
            }
        }
    }
}

fn render_commands_tab(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(11), // Top: 4x4 Interactive Command Button Grid + Selected Button Description
            Constraint::Min(6), // Bottom: Split between Last Command Output & Execution History Log
        ])
        .split(area);

    if let Some(&grid_area) = chunks.first() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Interactive Command Workbench (Click any button, use Arrows + Enter, or press Hotkey) ")
            .border_style(Style::default().fg(Color::Magenta));
        let inner = block.inner(grid_area);
        frame.render_widget(block, grid_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .split(inner);

        for row_idx in 0..4 {
            let Some(&row_rect) = rows.get(row_idx) else {
                continue;
            };
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                ])
                .split(row_rect);

            for col_idx in 0..4 {
                let btn_idx = row_idx * 4 + col_idx;
                let (Some(&btn), Some(&btn_rect)) =
                    (ButtonId::WORKBENCH.get(btn_idx), cols.get(col_idx))
                else {
                    continue;
                };
                app.hit_map.register(btn_rect, ClickTarget::Button(btn));
                let is_selected = app.selected_workbench_idx == btn_idx;
                let style = if is_selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                };
                let p = Paragraph::new(format!(" {} ", btn.label()))
                    .style(style)
                    .alignment(Alignment::Center);
                frame.render_widget(p, btn_rect);
            }
        }

        if let Some(&desc_rect) = rows.get(4) {
            let desc = ButtonId::WORKBENCH
                .get(app.selected_workbench_idx)
                .map(|b| format!(" ▶ {}: {}", b.label(), b.description()))
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(desc).style(Style::default().fg(Color::Yellow)),
                desc_rect,
            );
        }
    }

    if let Some(&bot_area) = chunks.get(1) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(bot_area);

        if let Some(&out_rect) = cols.first() {
            let text = app.last_command_output.as_deref().unwrap_or(
                "No command output yet.\n\nClick any command button above (or press its hotkey) to run an RPC operation.\nOutputs such as WebRTC SDP offers/answers, PubSub topic messages, and service lookup DIDs will be displayed here.",
            );
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Command Output / Handshake & PubSub Inspector ")
                .border_style(Style::default().fg(Color::Green));
            frame.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }).block(block),
                out_rect,
            );
        }

        if let Some(&log_rect) = cols.get(1) {
            render_recent_command_logs_panel(frame, app, log_rect);
        }
    }
}

fn render_recent_command_logs_panel(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app
        .action_logs
        .iter()
        .take(20)
        .map(|entry| {
            let badge = if entry.succeeded { "OK" } else { "ERR" };
            let color = if entry.succeeded {
                Color::Green
            } else {
                Color::Red
            };
            Row::new(vec![
                Cell::from(format!("#{}", entry.id)),
                Cell::from(badge).style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Cell::from(entry.command),
                Cell::from(format!("{}ms", entry.duration_ms)),
                Cell::from(entry.message.clone()),
            ])
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, [
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(18),
        Constraint::Length(7),
        Constraint::Min(20),
    ])
    .header(
        Row::new(vec!["#", "St", "Command", "Time", "Result"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " Command Execution Log ({}) ",
                app.action_logs.len()
            ))
            .border_style(Style::default().fg(Color::Blue)),
    );
    frame.render_widget(table, area);
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let busy = if app.in_flight_commands > 0 {
        format!(" [RUNNING {} RPC(s)...] ", app.in_flight_commands)
    } else {
        String::new()
    };

    let status_text =
        match &app.connection_state {
            ConnectionState::Offline(err) => format!("Node API Error: {err}"),
            _ => match app.action_logs.front() {
                Some(last) => format!("Last Action [{}]: {}", last.command, last.message),
                None if !app.snapshot.warnings.is_empty() => {
                    format!(
                        "Partial Poll Warnings: {}",
                        app.snapshot.warnings.join(" | ")
                    )
                }
                None => app.config.warning.clone().unwrap_or_else(|| {
                    "Ready. Click any button or press [?] for help.".to_string()
                }),
            },
        };

    let line = Line::from(vec![
        Span::styled(
            busy,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(status_text, Style::default().fg(Color::White)),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn render_modal(frame: &mut Frame<'_>, app: &mut App, modal: &CommandModal, area: Rect) {
    let height = u16::try_from(modal.fields.len().saturating_mul(3).saturating_add(7))
        .unwrap_or(16)
        .min(area.height.saturating_sub(2));
    let width = area.width.saturating_sub(8).clamp(44, 78);
    let modal_rect = centered_rect(width, height, area);

    frame.render_widget(Clear, modal_rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", modal.title))
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(modal_rect);
    frame.render_widget(block, modal_rect);

    let mut constraints = modal
        .fields
        .iter()
        .map(|_| Constraint::Length(3))
        .collect::<Vec<_>>();
    constraints.push(Constraint::Length(1)); // Error or hint line
    constraints.push(Constraint::Length(3)); // Submit / Cancel buttons

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (idx, field) in modal.fields.iter().enumerate() {
        let Some(&field_rect) = chunks.get(idx) else {
            continue;
        };
        app.hit_map
            .register(field_rect, ClickTarget::ModalField(idx));
        let is_active = modal.active_field == idx;
        let border_color = if is_active {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let display_text = if field.value.is_empty() && !is_active {
            Span::styled(field.placeholder, Style::default().fg(Color::DarkGray))
        } else if is_active {
            Span::styled(
                format!("{}█", field.value),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(field.value.clone(), Style::default().fg(Color::White))
        };

        let f_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", field.label))
            .border_style(Style::default().fg(border_color));
        frame.render_widget(
            Paragraph::new(Line::from(display_text)).block(f_block),
            field_rect,
        );
    }

    let err_idx = modal.fields.len();
    if let Some(&err_rect) = chunks.get(err_idx) {
        let line = match &modal.error {
            Some(err) => Line::from(Span::styled(
                format!(" ! {err}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            None => Line::from(Span::styled(
                " [Tab/Up/Down] Switch Field  [Ctrl+U] Clear  [Enter] Submit  [Esc] Cancel",
                Style::default().fg(Color::Gray),
            )),
        };
        frame.render_widget(Paragraph::new(line), err_rect);
    }

    if let Some(&btn_row_rect) = chunks.get(err_idx + 1) {
        let btn_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(btn_row_rect);

        if let Some(&submit_rect) = btn_cols.first() {
            app.hit_map.register(submit_rect, ClickTarget::ModalSubmit);
            let submit = Paragraph::new(" [ Execute / Submit (Enter) ] ")
                .style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL));
            frame.render_widget(submit, submit_rect);
        }

        if let Some(&cancel_rect) = btn_cols.get(1) {
            app.hit_map.register(cancel_rect, ClickTarget::ModalCancel);
            let cancel = Paragraph::new(" [ Cancel (Esc) ] ")
                .style(
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL));
            frame.render_widget(cancel, cancel_rect);
        }
    }
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let rect = centered_rect(72, 22, area);
    frame.render_widget(Clear, rect);

    let lines = vec![
        Line::from(Span::styled(
            "Navigation & Mouse Support",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  • Click any Tab, Toolbar Button, Table Row, or Modal Button with the mouse."),
        Line::from("  • Press [1]-[6] or [Tab]/[Shift+Tab] to switch between views."),
        Line::from("  • Press [Up]/[Down] (or j/k) to scroll tables; [Space] toggles sub-views."),
        Line::from("  • Press [B] to focus the Quick Command Button bar with keyboard arrows."),
        Line::from(""),
        Line::from(Span::styled(
            "Global Button Command Hotkeys",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  [R] Refresh Now           [P] Pause/Resume Auto-Poll"),
        Line::from("  [C] Connect Peer via URL  [D] Connect Peer via DID (DHT)"),
        Line::from("  [S] Connect via Seed      [X] Disconnect Selected Peer"),
        Line::from("  [O] Create WebRTC Offer   [A] Answer WebRTC Offer"),
        Line::from("  [Y] Accept WebRTC Answer  [M] Send Namespaced Backend Message"),
        Line::from("  [T] Publish Topic Msg     [F] Fetch Topic Messages"),
        Line::from("  [G] Register Service      [V] Lookup Service Providers"),
        Line::from("  [/] or [W] DHT Lookup Key [I] Inspect Peer Measure"),
        Line::from("  [E] Toggle Expired Dir    [U] Reload API Token      [Q] Quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Press [Esc], [Enter], or [?] (or click anywhere) to close this help window.",
            Style::default().fg(Color::Green),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" rings-tui Help & Command Reference ")
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

fn register_table_row_hits(
    hit_map: &mut crate::action::HitMap,
    table_area: Rect,
    row_count: usize,
) {
    // Border top (1) + Header row (1) = 2 rows offset inside `table_area`.
    let first_data_y = table_area.y.saturating_add(2);
    let max_y = table_area
        .y
        .saturating_add(table_area.height.saturating_sub(1));
    for idx in 0..row_count {
        let y = first_data_y.saturating_add(u16::try_from(idx).unwrap_or(u16::MAX));
        if y >= max_y {
            break;
        }
        hit_map.register(
            Rect {
                x: table_area.x.saturating_add(1),
                y,
                width: table_area.width.saturating_sub(2),
                height: 1,
            },
            ClickTarget::TableRow(idx),
        );
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn kv_line<'a>(label: &'a str, value: &str, color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!(" {label:<18} "), Style::default().fg(Color::Gray)),
        Span::styled(
            value.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Format a duration in seconds as a compact `Xd Xh Xm Xs` string.
pub fn format_duration_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let rem_secs = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m {rem_secs}s")
    } else if mins > 0 {
        format!("{mins}m {rem_secs}s")
    } else {
        format!("{rem_secs}s")
    }
}

/// Format a byte count with human-readable binary units (`B`, `KiB`, `MiB`, `GiB`).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1_024;
    const MIB: u64 = 1_024 * 1_024;
    const GIB: u64 = 1_024 * 1_024 * 1_024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}
