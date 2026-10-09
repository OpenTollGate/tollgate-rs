//! A live view of what a TollGate node is doing for its peers.
//!
//! Reads `tollgated`'s control socket and redraws. Peers only: prices and the
//! wallet belong to `merchantd` now, and `merchanttop` is where they are shown.
//!
//! The display is deliberately organised around the thing the protocol makes
//! hard to see: **two independent payment streams per peer**. What a peer
//! bought from us and what we bought from it are different channels, different
//! mints, different windows, bought at different moments — so they get separate
//! columns rather than one netted number, because netting them would invent a
//! relationship the protocol does not have. A row is a summary; Enter opens
//! everything the node knows about that peering.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use tollgate_net::control::{self, PeerSnapshot, Snapshot};

#[derive(Parser, Debug)]
#[command(name = "tolltop", about = "Watch a TollGate node")]
struct Args {
    /// The node's control socket. Found automatically if not given.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// Which instance to watch, by name: its socket is
    /// `/run/tollgate-<instance>/control.sock`. Needed only when several run.
    #[arg(short, long, conflicts_with = "socket")]
    instance: Option<String>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

/// Everything the display is currently doing, as opposed to what the node is.
struct App {
    socket: PathBuf,
    snapshot: Snapshot,
    /// Why the last refresh failed, if it did. The previous snapshot stays on
    /// screen: a node that is restarting should not erase what it was doing.
    error: Option<String>,
    /// Which peer the cursor is on, and whether its detail is open.
    peers: TableState,
    detail: bool,
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

    /// Move the cursor, stopping at either end rather than wrapping.
    ///
    /// Wrapping in a list this short reads as the cursor jumping rather than as
    /// reaching the end.
    fn move_selection(&mut self, delta: isize) {
        let count = self.snapshot.peers.len();
        if count == 0 {
            return;
        }
        let current = self.peers.selected().unwrap_or(0) as isize;
        self.peers
            .select(Some((current + delta).clamp(0, count as isize - 1) as usize));
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Looked for rather than assumed: a node run by a service manager puts its
    // runtime directory in /run, one run by a person under XDG_RUNTIME_DIR,
    // and a tool that only knew one of them would report "no such file" about
    // a node that is running perfectly well.
    let socket = match args.socket {
        Some(path) => path,
        None => control::find_socket(args.instance.as_deref())?,
    };
    let interval = Duration::from_millis(args.interval.max(50));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut terminal = ratatui::init();
    // A full repaint of a known-blank screen before the first draw. Entering
    // the alternate screen does not clear it, and the first draw only emits
    // cells that differ from an assumed-blank buffer — so on a terminal that
    // hands back an alternate buffer with something already in it (tmux, and
    // most things over ssh) the old contents show through the gaps.
    let _ = terminal.clear();

    let mut app = App {
        socket,
        snapshot: Snapshot::default(),
        error: None,
        peers: TableState::default(),
        detail: false,
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
            match key.code {
                KeyCode::Char('q') => break,
                // Esc backs out of the detail, and quits from the top.
                KeyCode::Esc if app.detail => app.detail = false,
                KeyCode::Esc => break,

                KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                KeyCode::Enter => {
                    app.detail = !app.detail && app.selected().is_some();
                }
                _ => {}
            }
        }
    }

    ratatui::restore();
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(frame.area());

    peers_view(frame, app, body);
    frame.render_widget(status(app), footer);
}

/// The peers table, with the detail beside it rather than instead of it.
///
/// Split rather than swapped: the row a detail belongs to is context for
/// reading it, and losing the table to open one peering means losing sight of
/// how it compares to the others.
fn peers_view(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.detail {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        // Half the width cannot hold eleven columns, so the table drops to
        // the five that say whether this peering is working. The rest of them
        // are in the panel beside it anyway.
        peer_table(frame, app, left, true);
        frame.render_widget(peer_detail(app), right);
    } else {
        peer_table(frame, app, area, false);
    }
}

fn peer_table(frame: &mut Frame, app: &mut App, area: Rect, compact: bool) {
    let titles: &[&str] = if compact {
        &["peer", "access", "sold", "left", "bought"]
    } else {
        &[
            "peer", "access", // what they bought from us
            "sold", "left", "in", "up", // what we bought from them
            "bought", "want", "m", "out", // and what has actually moved
            "taken",
        ]
    };
    let widths: &[Constraint] = if compact {
        &[
            Constraint::Length(10), // peer
            Constraint::Length(11), // access
            Constraint::Length(12), // sold
            Constraint::Length(7),  // left
            Constraint::Length(12), // bought
        ]
    } else {
        &[
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
            Constraint::Length(12), // taken (a total)
        ]
    };

    let rows: Vec<Row> = if compact {
        app.snapshot.peers.iter().map(compact_peer_row).collect()
    } else {
        app.snapshot.peers.iter().map(peer_row).collect()
    };

    let title = if compact {
        " peers ".to_string()
    } else {
        " peers — left of the divide is what they bought from us, right is what we bought from them "
            .to_string()
    };

    let table = Table::new(rows, widths.to_vec())
        .header(header_row(titles))
        .row_highlight_style(SELECTED)
        .highlight_symbol(CURSOR)
        .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_stateful_widget(table, area, &mut app.peers);
}

/// How the row under the cursor is drawn.
///
/// Reversed rather than given a background colour of its own. A coloured bar is
/// what a header looks like, and one sitting under another was unreadable as a
/// cursor — worse when there was a single row, where a permanently highlighted
/// line reads as decoration rather than as a thing that moves.
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// And the mark in front of it, so a cursor is a cursor even where the terminal
/// renders reversed video badly.
const CURSOR: &str = "▶ ";

/// A column header: a label, not a selection.
fn header_row<'a>(titles: &'a [&'a str]) -> Row<'a> {
    Row::new(titles.iter().map(|t| Cell::from(*t))).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

/// The five columns that say whether a peering is working, for when the detail
/// panel has taken the rest of the width.
fn compact_peer_row(peer: &PeerSnapshot) -> Row<'_> {
    Row::new(vec![
        Cell::from(short(&peer.pubkey)),
        Cell::from(Span::styled(
            peer.access.clone(),
            access_style(&peer.access),
        )),
        Cell::from(rate(peer.shaped_rate)),
        Cell::from(grant_left(peer)),
        Cell::from(rate(peer.bought_rate)),
    ])
}

/// How long the grant in force has left.
///
/// A grant that has lapsed leaves the peer on the allowance, which is a normal
/// resting state rather than a fault — so it is dimmed, not red.
fn grant_left(peer: &PeerSnapshot) -> Span<'static> {
    if peer.grant_expires_in_ms == 0 {
        Span::styled("lapsed", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(format!("{:.1}s", peer.grant_expires_in_ms as f64 / 1000.0))
    }
}

fn peer_row(peer: &PeerSnapshot) -> Row<'_> {
    Row::new(vec![
        Cell::from(short(&peer.pubkey)),
        // Colour carries the one thing worth noticing at a glance: whether
        // traffic is flowing for this peer at all.
        Cell::from(Span::styled(
            peer.access.clone(),
            access_style(&peer.access),
        )),
        Cell::from(rate(peer.shaped_rate)),
        Cell::from(grant_left(peer)),
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
        // A total, not a rate. Nothing measures a receive rate the way
        // `upload_rate` measures the sending one, and formatting a cumulative
        // counter with a `/s` on the end made it read as one — a peering that
        // had taken 178 KiB looked like it was taking 178 KiB every second.
        Cell::from(units(peer.received)),
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
            Span::styled(format!("{name:<20}"), dim),
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
            Span::styled(format!("{:<20}", "access"), dim),
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
        lines.push(field("channels on", "none".into()));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{:<20}", "channels on"),
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

    Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" peer {} ", short(&peer.pubkey))),
    )
}
fn status(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);

    if let Some(message) = &app.error {
        return Paragraph::new(Line::from(Span::styled(
            format!("{}: {message}", app.socket.display()),
            Style::default().fg(Color::Red),
        )))
        .block(Block::default().borders(Borders::ALL).title(" tolltop "));
    }

    let hints = if app.detail {
        "esc closes   ↑↓ selects   q quits"
    } else {
        "↑↓ selects   enter opens   q quits"
    };

    // Which node this is stays visible whatever is open: the table above says
    // what is being looked at, not what it is being looked at *on*.
    Paragraph::new(Line::from(vec![
        Span::styled(
            short(&app.snapshot.pubkey),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  selling ", dim),
        Span::raw(app.snapshot.unit.clone()),
        Span::styled("  up ", dim),
        Span::raw(duration(app.snapshot.uptime_ms)),
        Span::styled("  ", dim),
        Span::styled(
            format!("{} peer(s)", app.snapshot.peers.len()),
            Style::default().fg(Color::Green),
        ),
        Span::styled(format!("   {hints}"), dim),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" tolltop "))
}

fn access_style(access: &str) -> Style {
    match access {
        "active" => Style::default().fg(Color::Green),
        "free" => Style::default().fg(Color::Cyan),
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

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn peer(pubkey: &str) -> PeerSnapshot {
        PeerSnapshot {
            pubkey: pubkey.into(),
            phase: "established".into(),
            access: "active".into(),
            shaped_rate: 125_000,
            authorized: 1 << 20,
            consumed: 1 << 19,
            grant_expires_in_ms: 4_000,
            incoming_channels: Vec::new(),
            bought_rate: 0,
            demand: 0,
            upload_rate: 0,
            received_multiplier: 1,
            outgoing_channel: None,
            rollover_ready: false,
            delivered: 0,
            received: 0,
        }
    }

    fn app_with_two_peers() -> App {
        let mut app = App {
            socket: PathBuf::from("/tmp/x.sock"),
            snapshot: Snapshot {
                pubkey: "02aa".into(),
                unit: "byte".into(),
                peers: vec![peer("02111111aaaa"), peer("02222222bbbb")],
                ..Snapshot::default()
            },
            error: None,
            peers: TableState::default(),
            detail: false,
        };
        app.clamp_selection();
        app
    }

    fn render(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(160, 20)).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect()
    }

    /// The rows the cursor is on, by what they contain.
    fn cursored(screen: &[String]) -> Vec<String> {
        screen
            .iter()
            .filter(|line| line.contains(CURSOR.trim()))
            .map(|line| line.trim().to_string())
            .collect()
    }

    #[test]
    fn the_cursor_is_on_a_row_rather_than_on_the_header() {
        // The bug this replaces: the header was a solid grey bar and the
        // selected row was another one just under it, so the header read as the
        // selection and nothing looked like a cursor at all.
        let mut app = app_with_two_peers();
        let screen = render(&mut app);

        let marked = cursored(&screen);
        assert_eq!(marked.len(), 1, "exactly one row carries the cursor");
        assert!(marked[0].contains("02111111"), "{:?}", marked);

        let header = screen
            .iter()
            .find(|l| l.contains("access"))
            .expect("a header");
        assert!(
            !header.contains(CURSOR.trim()),
            "the header is a label, not a selection: {header:?}"
        );
    }

    #[test]
    fn the_cursor_moves_between_peers_and_stops_at_the_ends() {
        let mut app = app_with_two_peers();

        app.move_selection(1);
        assert!(cursored(&render(&mut app))[0].contains("02222222"));

        // Past the end stays at the end rather than wrapping, which in a list
        // this short reads as the cursor jumping.
        app.move_selection(1);
        assert!(cursored(&render(&mut app))[0].contains("02222222"));

        app.move_selection(-5);
        assert!(cursored(&render(&mut app))[0].contains("02111111"));
    }

    #[test]
    fn a_peer_that_leaves_takes_the_cursor_and_the_detail_with_it() {
        let mut app = app_with_two_peers();
        app.move_selection(1);
        app.detail = true;

        app.snapshot.peers.pop();
        app.clamp_selection();
        assert_eq!(app.peers.selected(), Some(0), "back onto a row that exists");

        app.snapshot.peers.clear();
        app.clamp_selection();
        assert_eq!(app.peers.selected(), None);
        assert!(!app.detail, "no detail open on a peer that is not there");
    }

    #[test]
    fn the_detail_shows_the_whole_key() {
        let mut app = app_with_two_peers();
        app.detail = true;
        let screen = render(&mut app).join("\n");
        assert!(screen.contains("02111111aaaa"), "{screen}");
        assert!(screen.contains("what this peer bought from us"), "{screen}");
    }
}
