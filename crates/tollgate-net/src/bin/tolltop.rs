//! A live view of what a TollGate node is doing.
//!
//! Reads the node's control socket and redraws. Two tabs, because the node does
//! two separable things: it carries traffic for peers, and it sells the
//! vouchers that pay for it.
//!
//! The peers tab is deliberately organised around the thing the protocol makes
//! hard to see: **two independent payment streams per peer**. What a peer
//! bought from us and what we bought from it are different channels, different
//! mints, different windows, bought at different moments — so they get separate
//! columns rather than one netted number, because netting them would invent a
//! relationship the protocol does not have. A row is a summary; Enter opens
//! everything the node knows about that peering.
//!
//! The pricing tab is the one place an operator changes something. The price is
//! the one setting worth moving while a node runs — an uplink becomes scarce,
//! or stops being — and moving it touches no session, grant or channel: it
//! decides what the *next* buyer of vouchers pays, and nothing already sold.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, TerminalOptions, Viewport};
use tollgate_net::control::{self, PeerSnapshot, Request, Response, Snapshot};

#[derive(Parser, Debug)]
#[command(name = "tolltop", about = "Watch a TollGate node")]
struct Args {
    /// The node's control socket. Found automatically if not given.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

/// Which tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Peers,
    Pricing,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Peers, Tab::Pricing];

    fn label(self) -> &'static str {
        match self {
            Tab::Peers => "peers",
            Tab::Pricing => "pricing",
        }
    }

    fn next(self) -> Self {
        match self {
            Tab::Peers => Tab::Pricing,
            Tab::Pricing => Tab::Peers,
        }
    }
}

/// Everything the display is currently doing, as opposed to what the node is.
struct App {
    socket: PathBuf,
    snapshot: Snapshot,
    /// Why the last refresh failed, if it did. The previous snapshot stays on
    /// screen: a node that is restarting should not erase what it was doing.
    error: Option<String>,
    /// Something the node refused, kept until the operator does something else.
    /// It cannot share `error`, which is recomputed every refresh and would
    /// wipe this before it was read.
    notice: Option<String>,
    tab: Tab,
    /// Which peer the cursor is on, and whether its detail is open.
    peers: TableState,
    detail: bool,
    /// `Some` while a new price is being typed. Editing is modal because a
    /// keystroke that means "quit" in one mode and "9" in the other has to be
    /// told which it is.
    editing: Option<String>,
}

impl App {
    fn selected(&self) -> Option<&PeerSnapshot> {
        self.peers
            .selected()
            .and_then(|i| self.snapshot.peers.get(i))
    }

    /// Keep the cursor on a row that exists.
    ///
    /// Peers come and go while the display is open, and a selection pointing
    /// past the end would show nothing while looking like it should show
    /// something.
    fn clamp_selection(&mut self) {
        let count = self.snapshot.peers.len();
        match (count, self.peers.selected()) {
            (0, _) => {
                self.peers.select(None);
                self.detail = false;
            }
            (_, None) => self.peers.select(Some(0)),
            (n, Some(i)) if i >= n => self.peers.select(Some(n - 1)),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.snapshot.peers.len();
        if count == 0 {
            return;
        }
        let current = self.peers.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, count as isize - 1);
        self.peers.select(Some(next as usize));
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Looked for rather than assumed: a node run by a service manager puts its
    // socket in /run, one run by a person puts it under XDG_RUNTIME_DIR, and a
    // tool that only knew about /tmp would report "no such file" about a node
    // that is running perfectly well.
    let socket = match args.socket {
        Some(path) => path,
        None => control::find_socket()?,
    };
    let interval = Duration::from_millis(args.interval.max(50));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut terminal = ratatui::init_with_options(TerminalOptions {
        viewport: Viewport::Fullscreen,
    });

    let mut app = App {
        socket,
        snapshot: Snapshot::default(),
        error: None,
        notice: None,
        tab: Tab::Peers,
        peers: TableState::default(),
        detail: false,
        editing: None,
    };

    loop {
        match runtime.block_on(control::fetch(&app.socket)) {
            Ok(fresh) => {
                app.snapshot = fresh;
                app.error = None;
            }
            Err(e) => app.error = Some(format!("{e:#}")),
        }
        app.clamp_selection();

        terminal.draw(|frame| draw(frame, &mut app))?;

        if event::poll(interval)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Editing takes every key it can use before anything else looks at
            // one: while a price is half-typed, `q` is not "quit".
            if let Some(text) = app.editing.as_mut() {
                match key.code {
                    KeyCode::Esc => app.editing = None,
                    KeyCode::Enter => {
                        app.notice = match text.trim().parse::<u64>() {
                            Ok(bytes_per_sat) => runtime
                                .block_on(set_price(&app.socket, bytes_per_sat))
                                .err()
                                .map(|e| format!("{e:#}")),
                            // Nothing typed is a change of mind, not a price of
                            // zero: closing the market is worth typing a `0`.
                            Err(_) => None,
                        };
                        app.editing = None;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => text.push(c),
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Tab | KeyCode::Right => {
                    app.tab = app.tab.next();
                    app.notice = None;
                }
                KeyCode::BackTab | KeyCode::Left => {
                    app.tab = app.tab.next();
                    app.notice = None;
                }
                // Esc backs out of whatever is open, and quits from the top.
                KeyCode::Esc if app.detail => app.detail = false,
                KeyCode::Esc => break,

                KeyCode::Up | KeyCode::Char('k') if app.tab == Tab::Peers => app.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') if app.tab == Tab::Peers => {
                    app.move_selection(1)
                }
                KeyCode::Enter if app.tab == Tab::Peers => {
                    app.detail = !app.detail && app.selected().is_some();
                }

                // On the pricing tab, editing is the only thing to do, so
                // Enter starts it as well as `e`.
                KeyCode::Enter | KeyCode::Char('e') if app.tab == Tab::Pricing => {
                    app.notice = None;
                    app.editing = Some(app.snapshot.bytes_per_sat.to_string());
                }
                _ => {}
            }
        }
    }

    ratatui::restore();
    Ok(())
}

/// Tell the node what to charge, and fail loudly if it will not.
async fn set_price(socket: &std::path::Path, bytes_per_sat: u64) -> Result<()> {
    match control::send(socket, &Request::SetPrice { bytes_per_sat }).await? {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => Err(anyhow::anyhow!(message)),
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    frame.render_widget(node_header(app), header);
    match (app.tab, app.detail) {
        (Tab::Peers, false) => peer_table(frame, app, body),
        (Tab::Peers, true) => frame.render_widget(peer_detail(app), body),
        (Tab::Pricing, _) => frame.render_widget(pricing(app), body),
    }
    frame.render_widget(status(app), footer);
}

/// The node, and the tabs, on one line: which node this is never stops being
/// the first thing worth knowing.
fn node_header(app: &App) -> Paragraph<'_> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled("node ", dim),
        Span::styled(
            short(&app.snapshot.pubkey),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("   selling ", dim),
        Span::raw(&app.snapshot.unit),
        Span::styled("   at ", dim),
        Span::raw(price(app.snapshot.bytes_per_sat)),
        Span::styled("   up ", dim),
        Span::raw(duration(app.snapshot.uptime_ms)),
        Span::styled("      ", dim),
    ];

    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", dim));
        }
        spans.push(Span::styled(
            tab.label(),
            if *tab == app.tab {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            },
        ));
    }

    Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title(" tolltop "))
}

fn peer_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let header = Row::new(vec![
        Cell::from("peer"),
        Cell::from("access"),
        // What they bought from us.
        Cell::from("sold"),
        Cell::from("left"),
        Cell::from("in"),
        Cell::from("up"),
        // What we bought from them.
        Cell::from("bought"),
        Cell::from("want"),
        Cell::from("m"),
        Cell::from("out"),
        Cell::from("down"),
    ])
    .style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app.snapshot.peers.iter().map(peer_row).collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(10), // peer
            Constraint::Length(11), // access
            Constraint::Length(12), // sold
            Constraint::Length(7),  // left
            Constraint::Length(14), // in (channel)
            Constraint::Length(12), // up
            Constraint::Length(12), // bought
            Constraint::Length(12), // want
            Constraint::Length(3),  // m
            Constraint::Length(14), // out (channel)
            Constraint::Length(12), // down
        ],
    )
    .header(header)
    .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" peers — left of the divide is what they bought from us, right is what we bought from them "),
    );

    frame.render_stateful_widget(table, area, &mut app.peers);
}

fn peer_row(peer: &PeerSnapshot) -> Row<'_> {
    // Colour carries the one thing worth noticing at a glance: whether traffic
    // is flowing for this peer at all.
    let access_style = access_style(&peer.access);

    // A grant that has lapsed leaves the peer on the allowance, which is a
    // normal resting state rather than a fault — so it is dimmed, not red.
    let left = if peer.grant_expires_in_ms == 0 {
        Span::styled("lapsed", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(format!("{:.1}s", peer.grant_expires_in_ms as f64 / 1000.0))
    };

    Row::new(vec![
        Cell::from(short(&peer.pubkey)),
        Cell::from(Span::styled(peer.access.clone(), access_style)),
        Cell::from(rate(peer.shaped_rate)),
        Cell::from(left),
        Cell::from(channels(&peer.incoming_channels)),
        Cell::from(rate(peer.upload_rate)),
        Cell::from(rate(peer.bought_rate)),
        Cell::from(rate(peer.demand)),
        Cell::from(peer.received_multiplier.to_string()),
        Cell::from(
            peer.outgoing_channel
                .as_ref()
                .map(|c| {
                    let mut text = channel(c);
                    if peer.rollover_ready {
                        // A replacement is funded and waiting for this one to
                        // fill — worth showing, since it is the thing that
                        // keeps buying from stalling at a channel boundary.
                        text.push('+');
                    }
                    text
                })
                .unwrap_or_else(|| "—".into()),
        ),
        Cell::from(rate(peer.received)),
    ])
}

/// One peering, in full.
///
/// The table has to fit eleven columns across a terminal, so it shows rates and
/// elides the rest. This is where the rest goes: the whole key, the phase, the
/// cumulative totals, and every channel rather than the first one.
fn peer_detail(app: &App) -> Paragraph<'_> {
    let Some(peer) = app.selected() else {
        return Paragraph::new("no peer selected")
            .block(Block::default().borders(Borders::ALL).title(" peer "));
    };

    let dim = Style::default().fg(Color::DarkGray);
    let field = |name: &'static str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name:<22}"), dim),
            Span::raw(value),
        ])
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("peer ", dim),
            Span::styled(
                peer.pubkey.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<22}", "access"), dim),
            Span::styled(peer.access.clone(), access_style(&peer.access)),
            Span::styled("   phase ", dim),
            Span::raw(peer.phase.clone()),
        ]),
        Line::from(""),
        // --- what they bought from us -------------------------------------
        Line::from(Span::styled(
            "what this peer bought from us",
            Style::default().fg(Color::Cyan),
        )),
        field("shaped rate", rate(peer.shaped_rate)),
        field(
            "grant expires in",
            if peer.grant_expires_in_ms == 0 {
                "lapsed — on the allowance".into()
            } else {
                format!("{:.1}s", peer.grant_expires_in_ms as f64 / 1000.0)
            },
        ),
        field("authorized", units(peer.authorized)),
        field("consumed", units(peer.consumed)),
        field("delivered to them", units(peer.delivered)),
        field("they are pushing", rate(peer.upload_rate)),
    ];

    if peer.incoming_channels.is_empty() {
        lines.push(field("channels they pay on", "none".into()));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{:<22}", "channels they pay on"),
            dim,
        )));
        for c in &peer.incoming_channels {
            lines.push(Line::from(format!("  {}", channel_detail(c))));
        }
    }

    // --- what we bought from them -----------------------------------------
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "what we bought from this peer",
        Style::default().fg(Color::Cyan),
    )));
    lines.push(field("bought rate", rate(peer.bought_rate)));
    lines.push(field("demand we observe", rate(peer.demand)));
    lines.push(field("received from them", units(peer.received)));
    lines.push(field(
        "their surcharge",
        format!("{}x on what we push", peer.received_multiplier),
    ));
    lines.push(field(
        "channel we pay on",
        match &peer.outgoing_channel {
            Some(c) => channel_detail(c),
            None => "none".into(),
        },
    ));
    lines.push(field(
        "replacement funded",
        if peer.rollover_ready {
            "yes — waiting for the one in use to fill".into()
        } else {
            "no".into()
        },
    ));

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" peer {} ", short(&peer.pubkey))),
    )
}

/// What this node charges, and what that means.
fn pricing(app: &App) -> Paragraph<'_> {
    let dim = Style::default().fg(Color::DarkGray);
    let bytes_per_sat = app.snapshot.bytes_per_sat;

    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{:<22}", "price"), dim),
            Span::styled(
                price(bytes_per_sat),
                Style::default()
                    .fg(if bytes_per_sat == 0 {
                        Color::Red
                    } else {
                        Color::Green
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<22}", "as configured"), dim),
            Span::raw(format!("{bytes_per_sat} bytes per sat")),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<22}", "mint"), dim),
            Span::raw(app.snapshot.mint_url.clone()),
        ]),
        Line::from(""),
    ];

    if bytes_per_sat == 0 {
        lines.push(Line::from(Span::styled(
            "The market is closed: quotes are refused, so nobody can buy this",
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(Span::styled(
            "node's vouchers here. Peers that already hold them are unaffected.",
            Style::default().fg(Color::Red),
        )));
    } else {
        // The numbers an operator actually reasons in. A price per byte is
        // unreadable; a price per gigabyte is a decision.
        lines.push(Line::from(vec![
            Span::styled(format!("{:<22}", "a gigabyte costs"), dim),
            Span::raw(sats(1_000_000_000, bytes_per_sat)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<22}", "a megabyte costs"), dim),
            Span::raw(sats(1_000_000, bytes_per_sat)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "This is what the next buyer of vouchers pays. It changes nothing",
            dim,
        )));
        lines.push(Line::from(Span::styled(
            "already sold: a grant in force, a channel funded, a quote handed",
            dim,
        )));
        lines.push(Line::from(Span::styled(
            "out — all keep the terms they were made on.",
            dim,
        )));
    }

    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" pricing "))
}

fn status(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);

    // Editing wins over everything: the operator is mid-keystroke, and what
    // they need to see is what they have typed so far.
    if let Some(text) = &app.editing {
        return Paragraph::new(Line::from(vec![
            Span::styled(
                "price (bytes per sat): ",
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{text}_"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("   enter to set, esc to cancel, 0 to stop selling", dim),
        ]))
        .block(Block::default().borders(Borders::ALL));
    }

    if let Some(message) = app.notice.as_ref().or(app.error.as_ref()) {
        return Paragraph::new(Line::from(Span::styled(
            format!("{}: {message}", app.socket.display()),
            Style::default().fg(Color::Red),
        )))
        .block(Block::default().borders(Borders::ALL));
    }

    let hints = match (app.tab, app.detail) {
        (Tab::Peers, false) => "tab to switch   ↑↓ to select   enter for detail   q to quit",
        (Tab::Peers, true) => "esc to go back   tab to switch   q to quit",
        (Tab::Pricing, _) => "e to change the price   tab to switch   q to quit",
    };

    Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{} peer(s)", app.snapshot.peers.len()),
            Style::default().fg(Color::Green),
        ),
        Span::styled(format!("   {hints}"), dim),
    ]))
    .block(Block::default().borders(Borders::ALL))
}

fn access_style(access: &str) -> Style {
    match access {
        "active" => Style::default().fg(Color::Green),
        "free" => Style::default().fg(Color::Cyan),
        "suspended" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// How full a channel is. The number that matters is how close it is to needing
/// a rollover, not its absolute size.
fn channel(c: &control::ChannelSnapshot) -> String {
    let pct = if c.capacity == 0 {
        0
    } else {
        (c.signed as u128 * 100 / c.capacity as u128) as u64
    };
    format!("{} {pct}%", c.id)
}

fn channel_detail(c: &control::ChannelSnapshot) -> String {
    format!(
        "{} — {} of {} signed",
        channel(c),
        units(c.signed),
        units(c.capacity)
    )
}

fn channels(list: &[control::ChannelSnapshot]) -> String {
    match list {
        [] => "—".into(),
        [one] => channel(one),
        // Several at once is a rollover in flight, or a payer spending from
        // more than one accepted mint.
        many => format!("{} +{}", channel(&many[0]), many.len() - 1),
    }
}

/// Binary units, because the unit sold is the byte and grants decompose into
/// power-of-two proofs.
fn rate(bytes_per_second: u64) -> String {
    match bytes_per_second {
        0 => "—".into(),
        n => format!("{}/s", units(n)),
    }
}

fn units(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let v = bytes as f64;
    if v >= GIB {
        format!("{:.2} GiB", v / GIB)
    } else if v >= MIB {
        format!("{:.2} MiB", v / MIB)
    } else if v >= KIB {
        format!("{:.1} KiB", v / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// What a sat buys, in the units an operator thinks in.
///
/// Decimal rather than binary, unlike a rate: this is a price, and a price is
/// quoted against the sat, which has nothing to do with powers of two.
fn price(bytes_per_sat: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = KB * 1_000.0;
    const GB: f64 = MB * 1_000.0;
    let v = bytes_per_sat as f64;
    if bytes_per_sat == 0 {
        "not selling".into()
    } else if v >= GB {
        format!("{:.2} GB/sat", v / GB)
    } else if v >= MB {
        format!("{:.2} MB/sat", v / MB)
    } else if v >= KB {
        format!("{:.1} kB/sat", v / KB)
    } else {
        format!("{bytes_per_sat} B/sat")
    }
}

/// What a quantity costs at the price in force.
fn sats(bytes: u64, bytes_per_sat: u64) -> String {
    if bytes_per_sat == 0 {
        return "—".into();
    }
    let sats = bytes as f64 / bytes_per_sat as f64;
    if sats >= 1.0 {
        format!("{sats:.0} sat")
    } else {
        // Below a sat the interesting figure is millisats, because that is what
        // the invoice will actually be written for.
        format!("{:.0} msat", sats * 1_000.0)
    }
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn short(hex_key: &str) -> String {
    hex_key.chars().take(8).collect()
}
