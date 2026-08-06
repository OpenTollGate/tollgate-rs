//! A live view of what a TollGate node is doing.
//!
//! Reads the node's control socket and redraws. It is deliberately organised
//! around the thing the protocol makes hard to see: **two independent payment
//! streams per peer**. What a peer bought from us and what we bought from it
//! are different channels, different mints, different windows, bought at
//! different moments — so they get separate columns rather than one netted
//! number, because netting them would invent a relationship the protocol does
//! not have.
//!
//! It is also the place an operator changes what the node charges. That is the
//! one setting worth moving while a node runs — an uplink becomes scarce, or
//! stops being — and it touches no session, grant or channel: it decides what
//! the *next* buyer of vouchers pays, and nothing that has already been sold.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{Frame, TerminalOptions, Viewport};
use tollgate_net::control::{self, PeerSnapshot, Request, Response, Snapshot};

#[derive(Parser, Debug)]
#[command(name = "tolltop", about = "Watch a TollGate node")]
struct Args {
    /// The node's control socket.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(control::default_socket_path);
    let interval = Duration::from_millis(args.interval.max(50));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut terminal = ratatui::init_with_options(TerminalOptions {
        viewport: Viewport::Fullscreen,
    });

    let mut snapshot = Snapshot::default();
    let mut error;
    // Some(text) while the operator is typing a new price. Editing is modal
    // rather than a prompt in a corner: a keystroke that means "quit" in one
    // mode and "9" in the other has to be told which it is.
    let mut editing: Option<String> = None;
    // A refusal from the node, kept on screen until the operator does
    // something else. It cannot share `error` with the fetch loop: that one is
    // recomputed every refresh, which would wipe this before it was read.
    let mut notice: Option<String> = None;

    loop {
        match runtime.block_on(control::fetch(&socket)) {
            Ok(fresh) => {
                snapshot = fresh;
                error = None;
            }
            // Keep the last good snapshot on screen rather than blanking: a
            // node that is restarting should not erase what it was doing.
            Err(e) => error = Some(format!("{e:#}")),
        }

        terminal.draw(|frame| {
            draw(
                frame,
                &snapshot,
                notice.as_deref().or(error.as_deref()),
                &socket,
                &editing,
            )
        })?;

        if event::poll(interval)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match (&mut editing, key.code) {
                // --- editing a price ---
                (Some(_), KeyCode::Esc) => editing = None,
                (Some(text), KeyCode::Enter) => {
                    // An empty entry is a change of mind, not a price of zero:
                    // closing the market is worth typing a `0` for.
                    notice = match text.trim().parse::<u64>() {
                        Ok(bytes_per_sat) => runtime
                            .block_on(set_price(&socket, bytes_per_sat))
                            .err()
                            .map(|e| format!("{e:#}")),
                        // Nothing typed is a change of mind, not a price of
                        // zero: closing the market is worth typing a `0` for.
                        Err(_) => None,
                    };
                    editing = None;
                }
                (Some(text), KeyCode::Backspace) => {
                    text.pop();
                }
                (Some(text), KeyCode::Char(c)) if c.is_ascii_digit() => text.push(c),

                // --- watching ---
                (None, KeyCode::Char('p')) => {
                    notice = None;
                    editing = Some(snapshot.bytes_per_sat.to_string());
                }
                (None, KeyCode::Char('q') | KeyCode::Esc) => break,
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

fn draw(
    frame: &mut Frame,
    snapshot: &Snapshot,
    error: Option<&str>,
    socket: &std::path::Path,
    editing: &Option<String>,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    frame.render_widget(node_header(snapshot), header);
    frame.render_widget(peer_table(snapshot), body);
    frame.render_widget(status(error, socket, snapshot.peers.len(), editing), footer);
}

fn node_header(snapshot: &Snapshot) -> Paragraph<'_> {
    let line = Line::from(vec![
        Span::styled("node ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            short(&snapshot.pubkey),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("   selling ", Style::default().fg(Color::DarkGray)),
        Span::raw(&snapshot.unit),
        Span::styled("   mint ", Style::default().fg(Color::DarkGray)),
        Span::raw(&snapshot.mint_url),
        Span::styled("   at ", Style::default().fg(Color::DarkGray)),
        Span::raw(price(snapshot.bytes_per_sat)),
        Span::styled("   up ", Style::default().fg(Color::DarkGray)),
        Span::raw(duration(snapshot.uptime_ms)),
    ]);
    Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" tolltop "))
}

fn peer_table(snapshot: &Snapshot) -> Table<'_> {
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

    let rows = snapshot.peers.iter().map(peer_row);

    Table::new(
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
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" peers — left of the divide is what they bought from us, right is what we bought from them "),
    )
}

fn peer_row(peer: &PeerSnapshot) -> Row<'_> {
    // Colour carries the one thing worth noticing at a glance: whether traffic
    // is flowing for this peer at all.
    let access_style = match peer.access.as_str() {
        "active" => Style::default().fg(Color::Green),
        "free" => Style::default().fg(Color::Cyan),
        "suspended" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    };

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

fn status(
    error: Option<&str>,
    socket: &std::path::Path,
    peers: usize,
    editing: &Option<String>,
) -> Paragraph<'static> {
    // Editing wins over the error line: the operator is mid-keystroke, and
    // what they need to see is what they have typed so far.
    let line = match (editing, error) {
        (Some(text), _) => Line::from(vec![
            Span::styled(
                "price (bytes per sat): ",
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{text}_"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   enter to set, esc to cancel, 0 to stop selling",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        (None, Some(e)) => Line::from(Span::styled(
            format!("{}: {e}", socket.display()),
            Style::default().fg(Color::Red),
        )),
        (None, None) => Line::from(vec![
            Span::styled(
                format!("{peers} peer(s)"),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                "   p to price   q to quit",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    };
    Paragraph::new(line).block(Block::default().borders(Borders::ALL))
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
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let v = bytes_per_second as f64;
    if bytes_per_second == 0 {
        "—".into()
    } else if v >= MIB {
        format!("{:.2} MiB/s", v / MIB)
    } else if v >= KIB {
        format!("{:.1} KiB/s", v / KIB)
    } else {
        format!("{bytes_per_second} B/s")
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
