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
//!
//! One machine may run several instances of `tollgated`. Without `--socket`,
//! every running one is found and looked for again every few seconds, so ones
//! that start or stop come and go on their own. Tab moves to the next. The
//! frame always says which instance is on screen and how many there are.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use tollgate_net::control::{self, PeerSnapshot, Snapshot};
use tollgate_net::instance;

#[derive(Parser, Debug)]
#[command(name = "tolltop", about = "Watch a TollGate node")]
struct Args {
    /// Watch this control socket and nothing else. Without it, every running
    /// instance is found and Tab moves between them.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// Which instance to start on, by name: its socket is
    /// `/run/tollgate-<instance>/control.sock`. Tab still moves to the others.
    #[arg(short, long, conflicts_with = "socket")]
    instance: Option<String>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

/// How often to look again for instances that started or stopped.
const DISCOVER_EVERY: Duration = Duration::from_secs(2);

/// Which node the display is on, out of the ones it can see.
#[derive(Debug)]
struct Instances {
    /// Every instance found, as `(name, control socket)`, in the order
    /// discovery gives them.
    found: Vec<(String, PathBuf)>,
    /// The one on screen, by name.
    ///
    /// Kept by name so that others starting or stopping do not move the
    /// display. Kept even when that one stops, so a node that restarts comes
    /// back on screen instead of the view jumping to another node.
    current: Option<String>,
    /// Set by `--socket`: one socket, never looked for again, no cycling.
    pinned: bool,
}

impl Instances {
    /// One socket the caller named.
    fn pinned(path: PathBuf) -> Self {
        let name = instance_name(&path).unwrap_or_else(|| path.display().to_string());
        Self {
            found: vec![(name.clone(), path)],
            current: Some(name),
            pinned: true,
        }
    }

    /// Whatever is running, starting on `start` if it is named.
    fn discovered(start: Option<String>) -> Self {
        Self {
            found: Vec::new(),
            current: start,
            pinned: false,
        }
    }

    /// Take a fresh list of running instances. Returns whether the one on
    /// screen changed.
    fn update(&mut self, found: Vec<(String, PathBuf)>) -> bool {
        if self.pinned {
            return false;
        }
        self.found = found;
        if self.current.is_none()
            && let Some((name, _)) = self.found.first()
        {
            self.current = Some(name.clone());
            return true;
        }
        false
    }

    /// Where the one on screen is in the list, if it is running.
    fn position(&self) -> Option<usize> {
        let current = self.current.as_deref()?;
        self.found.iter().position(|(name, _)| name == current)
    }

    /// The control socket of the one on screen, if it is running.
    fn socket(&self) -> Option<&Path> {
        self.position().map(|i| self.found[i].1.as_path())
    }

    /// Move to the next running instance, wrapping at the end. Returns
    /// whether the one on screen changed.
    fn next(&mut self) -> bool {
        if self.pinned || self.found.is_empty() {
            return false;
        }
        let next = match self.position() {
            Some(i) => (i + 1) % self.found.len(),
            None => 0,
        };
        let name = &self.found[next].0;
        if self.current.as_deref() == Some(name.as_str()) {
            return false;
        }
        self.current = Some(name.clone());
        true
    }

    /// Whether there is anything to cycle to.
    fn can_cycle(&self) -> bool {
        !self.pinned
    }

    /// Which instance is on screen, and how many there are.
    fn title(&self) -> String {
        if self.pinned {
            let (name, path) = &self.found[0];
            return match instance_name(path) {
                Some(_) => format!("instance {name}"),
                None => format!("socket {name}"),
            };
        }
        match (&self.current, self.position()) {
            (None, _) => "no instance running".into(),
            (Some(name), Some(i)) => {
                format!("instance {name} ({}/{})", i + 1, self.found.len())
            }
            (Some(name), None) => format!(
                "instance {name}, not running ({} other(s) running)",
                self.found.len()
            ),
        }
    }
}

/// The instance a control socket belongs to, from its runtime directory's
/// name: `…/tollgate-ip/control.sock` is instance `ip`.
fn instance_name(socket: &Path) -> Option<String> {
    let dir = socket.parent()?.file_name()?.to_str()?;
    let name = dir.strip_prefix("tollgate-")?;
    instance::validate(name).ok()?;
    Some(name.to_owned())
}

/// Everything the display is currently doing, as opposed to what the node is.
struct App {
    instances: Instances,
    snapshot: Snapshot,
    /// Why the last refresh failed, if it did. The previous snapshot stays on
    /// screen: a node that is restarting should not erase what it was doing.
    error: Option<String>,
    /// Which peer the cursor is on, and whether its detail is open.
    peers: TableState,
    detail: bool,
}

impl App {
    fn new(instances: Instances) -> Self {
        Self {
            instances,
            snapshot: Snapshot::default(),
            error: None,
            peers: TableState::default(),
            detail: false,
        }
    }

    /// Forget what the last node showed, after moving to another one. Its
    /// peers are not this one's.
    fn reset_view(&mut self) {
        self.snapshot = Snapshot::default();
        self.error = None;
        self.peers = TableState::default();
        self.detail = false;
    }

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
    if let Some(name) = &args.instance {
        instance::validate(name)?;
    }
    // Looked for rather than assumed: a node run by a service manager puts its
    // runtime directory in /run, one run by a person under XDG_RUNTIME_DIR,
    // and a tool that only knew one of them would report "no such file" about
    // a node that is running perfectly well.
    let instances = match args.socket {
        Some(path) => Instances::pinned(path),
        None => Instances::discovered(args.instance),
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

    let mut app = App::new(instances);
    let mut last_look: Option<Instant> = None;

    loop {
        // Look again now and then, so an instance that starts or stops shows
        // up or goes away without restarting the display.
        if last_look.is_none_or(|at| at.elapsed() >= DISCOVER_EVERY) {
            if app.instances.update(instance::control_sockets()) {
                app.reset_view();
            }
            last_look = Some(Instant::now());
        }

        match app.instances.socket() {
            Some(socket) => match runtime.block_on(control::fetch(socket)) {
                Ok(fresh) => {
                    app.snapshot = fresh;
                    app.error = None;
                }
                Err(e) => app.error = Some(format!("{}: {e:#}", socket.display())),
            },
            // Nothing to ask. The body says so, and the next look may find it.
            None => {
                app.snapshot = Snapshot::default();
                app.error = None;
            }
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
                KeyCode::Tab | KeyCode::Char('i') => {
                    if app.instances.next() {
                        app.reset_view();
                    }
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

    if app.instances.socket().is_some() {
        peers_view(frame, app, body);
    } else {
        frame.render_widget(waiting(app), body);
    }
    frame.render_widget(status(app), footer);
}

/// What the body says when there is no node to ask: none is running, or the
/// one on screen has stopped.
fn waiting(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let others: Vec<&str> = app
        .instances
        .found
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let mut lines = vec![match &app.instances.current {
        Some(name) => Line::from(Span::styled(
            format!("Instance {name} is not running."),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        None => Line::from(Span::styled(
            "No tollgated is running.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    }];
    lines.push(Line::from(""));
    if !others.is_empty() {
        lines.push(Line::from(format!(
            "Running: {}. Tab moves to the next one.",
            others.join(", ")
        )));
    }
    let looked: Vec<String> = instance::bases()
        .iter()
        .map(|b| {
            b.join("tollgate-*")
                .join(instance::CONTROL_SOCKET)
                .display()
                .to_string()
        })
        .collect();
    lines.push(Line::from(Span::styled(
        format!("Looked for {}.", looked.join(", ")),
        dim,
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "Looking again every {}s. q quits.",
            DISCOVER_EVERY.as_secs()
        ),
        dim,
    )));
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" waiting "))
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
        // Half the width cannot hold every column, so the table drops to the
        // few that say whether this peering is working. The rest of them are
        // in the panel beside it anyway.
        peer_table(frame, app, left, true);
        frame.render_widget(peer_detail(app), right);
    } else {
        peer_table(frame, app, area, false);
    }
}

fn peer_table(frame: &mut Frame, app: &mut App, area: Rect, compact: bool) {
    let titles: &[&str] = if compact {
        &["peer", "access", "budget", "ends", "reserved", "bought"]
    } else {
        &[
            "peer",
            "access", // what they bought from us
            "speed",
            "budget",
            "ends",
            "reserved",
            "in", // what we bought from them
            "bought",
            "ends",
            "reserved",
            "want",
            "w",
            "out", // and what moved
            "from payer",
        ]
    };
    let widths: &[Constraint] = if compact {
        &[
            Constraint::Length(8),  // peer
            Constraint::Length(7),  // access
            Constraint::Length(10), // budget
            Constraint::Length(7),  // ends
            Constraint::Length(12), // reserved
            Constraint::Length(10), // bought
        ]
    } else {
        &[
            Constraint::Length(8),  // peer
            Constraint::Length(7),  // access
            Constraint::Length(12), // speed
            Constraint::Length(10), // budget
            Constraint::Length(7),  // ends
            Constraint::Length(12), // reserved
            Constraint::Length(13), // in (channel)
            Constraint::Length(10), // bought
            Constraint::Length(7),  // ends
            Constraint::Length(12), // reserved
            Constraint::Length(12), // want
            Constraint::Length(2),  // w
            Constraint::Length(13), // out (channel)
            Constraint::Length(10), // from payer (a total)
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

/// The columns that say whether a peering is working, for when the detail
/// panel has taken the rest of the width.
fn compact_peer_row(peer: &PeerSnapshot) -> Row<'_> {
    Row::new(vec![
        Cell::from(short(&peer.pubkey)),
        Cell::from(Span::styled(
            peer.access.clone(),
            access_style(&peer.access),
        )),
        Cell::from(budget(peer.budget)),
        Cell::from(deadline(peer.budget_expires_in_ms)),
        Cell::from(rate(peer.reserved_rate)),
        Cell::from(budget(peer.bought_budget)),
    ])
}

/// What is left of a budget.
///
/// A budget with nothing left leaves the peer on the minimum flow allowance,
/// which is a normal resting state rather than a fault — so it is dimmed, not
/// red.
fn budget(units_left: u64) -> Span<'static> {
    if units_left == 0 {
        Span::styled("empty", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(units(units_left))
    }
}

/// How long until a deadline, or a dimmed dash when there is none.
fn deadline(expires_in_ms: u64) -> Span<'static> {
    if expires_in_ms == 0 {
        Span::styled("—", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(duration(expires_in_ms))
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
        Cell::from(budget(peer.budget)),
        Cell::from(deadline(peer.budget_expires_in_ms)),
        Cell::from(rate(peer.reserved_rate)),
        Cell::from(channels(&peer.incoming_channels)),
        if peer.refused_terms {
            // Their terms were refused, so there is nothing bought to show.
            Cell::from(Span::styled("refused", Style::default().fg(Color::Yellow)))
        } else {
            Cell::from(budget(peer.bought_budget))
        },
        Cell::from(deadline(peer.bought_expires_in_ms)),
        Cell::from(rate(peer.bought_reserved_rate)),
        Cell::from(rate(peer.demand)),
        Cell::from(peer.their_from_payer_weight.to_string()),
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
        Cell::from(units(peer.from_payer)),
    ])
}

/// One peering, in full.
///
/// The table has to fit many columns across a terminal, so it shows the short
/// form and leaves the rest out. This is where the rest goes: the whole key,
/// the phase, both directions in full, the counters, and every channel rather
/// than the first one.
fn peer_detail(app: &App) -> Paragraph<'_> {
    let Some(peer) = app.selected() else {
        return Paragraph::new("no peer selected")
            .block(Block::default().borders(Borders::ALL).title(" peer "));
    };

    let dim = Style::default().fg(Color::DarkGray);
    let heading = Style::default().fg(Color::Cyan);
    let field = |name: &'static str, value: Span<'static>| {
        Line::from(vec![Span::styled(format!("{name:<20}"), dim), value])
    };
    let text = |value: String| Span::raw(value);

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
        Line::from(Span::styled("what this peer bought from us", heading)),
        field("budget", budget_detail(peer.budget)),
        field("deadline", deadline_detail(peer.budget_expires_in_ms)),
        field("reserved rate", reserved_detail(peer.reserved_rate)),
        field(
            "from-payer weight",
            text(format!("{}x on what they send us", peer.from_payer_weight)),
        ),
        field("speed we allow", text(rate(peer.shaped_rate))),
        field("authorized", text(units(peer.authorized))),
        field("consumed", text(units(peer.consumed))),
    ];

    if peer.incoming_channels.is_empty() {
        lines.push(field("channels on", text("none".into())));
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
        heading,
    )));
    lines.push(field(
        "terms",
        if peer.refused_terms {
            Span::styled(
                "refused — their from-payer weight is above ours, so we buy nothing",
                Style::default().fg(Color::Yellow),
            )
        } else {
            text("accepted".into())
        },
    ));
    lines.push(field(
        "budget, our count",
        budget_detail(peer.bought_budget),
    ));
    lines.push(field(
        "deadline",
        deadline_detail(peer.bought_expires_in_ms),
    ));
    lines.push(field(
        "reserved rate",
        reserved_detail(peer.bought_reserved_rate),
    ));
    lines.push(field(
        "their weight",
        text(format!(
            "{}x on what we send them",
            peer.their_from_payer_weight
        )),
    ));
    lines.push(field(
        "their last Balance",
        match &peer.reported_balance {
            Some(b) => text(balance(b)),
            None => Span::styled("none yet", dim),
        },
    ));
    lines.push(field("demand we observe", text(rate(peer.demand))));
    lines.push(field("we are pushing", text(rate(peer.upload_rate))));
    lines.push(field(
        "channel we pay on",
        text(match &peer.outgoing_channel {
            Some(c) => channel_detail(c),
            None => "none".into(),
        }),
    ));
    lines.push(field(
        "replacement funded",
        text(if peer.rollover_ready {
            "yes — waiting for the one in use to fill".into()
        } else {
            "no".into()
        }),
    ));

    // --- what moved -------------------------------------------------------
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("what moved", heading)));
    lines.push(field("to payer", text(units(peer.to_payer))));
    lines.push(field("from payer", text(units(peer.from_payer))));

    Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" peer {} ", short(&peer.pubkey))),
    )
}

fn budget_detail(units_left: u64) -> Span<'static> {
    if units_left == 0 {
        Span::styled(
            "empty — on the minimum flow allowance",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(units(units_left))
    }
}

fn deadline_detail(expires_in_ms: u64) -> Span<'static> {
    if expires_in_ms == 0 {
        Span::styled("none", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(format!("in {}", duration(expires_in_ms)))
    }
}

/// A reserved rate, or that there is none and each unit is paid as it is used.
fn reserved_detail(units_per_second: u64) -> Span<'static> {
    if units_per_second == 0 {
        Span::styled("none — pay per use", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(rate(units_per_second))
    }
}

/// A Balance the peer sent us, and how old it is. It is what they said, not
/// what is true now, so its age is part of reading it.
fn balance(b: &control::BalanceSnapshot) -> String {
    let deadline = if b.expires_in_ms == 0 {
        "no deadline".to_string()
    } else {
        format!("deadline in {}", duration(b.expires_in_ms))
    };
    let reserved = if b.reserved_rate == 0 {
        "no reserved rate".to_string()
    } else {
        format!("reserved {}", rate(b.reserved_rate))
    };
    format!(
        "{} left, {deadline}, {reserved} — said {} ago",
        units(b.remaining),
        duration(b.age_ms)
    )
}

fn status(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    // Which instance this is stays in the frame whatever else is on screen.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" tolltop — {} ", app.instances.title()));

    if let Some(message) = &app.error {
        return Paragraph::new(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )))
        .block(block);
    }

    let cycle = if app.instances.can_cycle() {
        "   tab next instance (or i)"
    } else {
        ""
    };
    let hints = if app.instances.socket().is_none() {
        format!("{}   q quits", cycle.trim_start())
    } else if app.detail {
        format!("esc closes   ↑↓ selects{cycle}   q quits")
    } else {
        format!("↑↓ selects   enter opens{cycle}   q quits")
    };

    if app.instances.socket().is_none() {
        return Paragraph::new(Line::from(Span::styled(hints, dim))).block(block);
    }

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
    .block(block)
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

/// A span of time, to the two largest units: deadlines run to thirty days,
/// and a count in seconds that long is unreadable.
fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600)
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
            budget: 1 << 19,
            budget_expires_in_ms: 4_000,
            reserved_rate: 125_000,
            from_payer_weight: 1,
            authorized: 1 << 20,
            consumed: 1 << 19,
            incoming_channels: Vec::new(),
            bought_budget: 0,
            bought_expires_in_ms: 0,
            bought_reserved_rate: 0,
            their_from_payer_weight: 1,
            reported_balance: None,
            demand: 0,
            upload_rate: 0,
            refused_terms: false,
            outgoing_channel: None,
            rollover_ready: false,
            to_payer: 0,
            from_payer: 0,
        }
    }

    fn app_with_two_peers() -> App {
        let mut app = App::new(Instances::pinned(PathBuf::from("/tmp/x.sock")));
        app.snapshot = Snapshot {
            pubkey: "02aa".into(),
            unit: "byte".into(),
            peers: vec![peer("02111111aaaa"), peer("02222222bbbb")],
            ..Snapshot::default()
        };
        app.clamp_selection();
        app
    }

    fn running(names: &[&str]) -> Vec<(String, PathBuf)> {
        names
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    PathBuf::from(format!("/run/tollgate-{n}/control.sock")),
                )
            })
            .collect()
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

    #[test]
    fn tab_cycles_through_instances_in_order_and_wraps() {
        let mut instances = Instances::discovered(None);
        assert!(instances.update(running(&["fips", "ip", "lan"])));
        assert_eq!(instances.current.as_deref(), Some("fips"));

        assert!(instances.next());
        assert_eq!(instances.current.as_deref(), Some("ip"));
        assert!(instances.next());
        assert_eq!(instances.current.as_deref(), Some("lan"));
        assert!(instances.next(), "past the last comes the first");
        assert_eq!(instances.current.as_deref(), Some("fips"));
    }

    #[test]
    fn a_named_instance_is_where_the_display_starts() {
        let mut instances = Instances::discovered(Some("lan".into()));
        assert!(!instances.update(running(&["fips", "ip", "lan"])));
        assert_eq!(
            instances.socket(),
            Some(Path::new("/run/tollgate-lan/control.sock"))
        );
        assert_eq!(instances.title(), "instance lan (3/3)");
    }

    #[test]
    fn others_starting_or_stopping_do_not_move_the_display() {
        let mut instances = Instances::discovered(Some("ip".into()));
        instances.update(running(&["ip", "lan"]));
        assert_eq!(instances.title(), "instance ip (1/2)");

        instances.update(running(&["fips", "ip", "lan"]));
        assert_eq!(instances.title(), "instance ip (2/3)");

        // The one on screen stopping keeps it on screen, so a restart comes
        // back to it; Tab moves on.
        instances.update(running(&["fips", "lan"]));
        assert_eq!(instances.socket(), None);
        assert!(instances.title().contains("ip, not running"));
        assert!(instances.next());
        assert_eq!(instances.current.as_deref(), Some("fips"));
    }

    #[test]
    fn a_pinned_socket_does_not_cycle() {
        let mut instances = Instances::pinned(PathBuf::from("/run/tollgate-ip/control.sock"));
        assert!(!instances.update(running(&["fips", "ip", "lan"])));
        assert!(!instances.next());
        assert_eq!(instances.title(), "instance ip");

        let other = Instances::pinned(PathBuf::from("/tmp/x.sock"));
        assert_eq!(other.title(), "socket /tmp/x.sock");
    }

    #[test]
    fn the_frame_names_the_instance_on_screen() {
        let mut instances = Instances::discovered(None);
        instances.update(running(&["fips", "ip", "lan"]));
        instances.next();
        let mut app = App::new(instances);
        app.snapshot.peers = vec![peer("02111111aaaa")];
        app.clamp_selection();
        let screen = render(&mut app).join("\n");
        assert!(screen.contains("instance ip (2/3)"), "{screen}");
        assert!(screen.contains("tab next instance"), "{screen}");
    }

    #[test]
    fn with_nothing_running_the_display_says_so_and_waits() {
        let mut app = App::new(Instances::discovered(None));
        let screen = render(&mut app).join("\n");
        assert!(screen.contains("No tollgated is running."), "{screen}");
        assert!(screen.contains("no instance running"), "{screen}");
    }

    #[test]
    fn long_deadlines_read_in_days() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59_999), "59s");
        assert_eq!(duration(61_000), "1m01s");
        assert_eq!(duration(3_600_000), "1h00m");
        assert_eq!(duration(86_399_000), "23h59m");
        assert_eq!(duration(86_400_000), "1d00h");
        assert_eq!(duration(30 * 86_400_000), "30d00h");
        assert_eq!(duration(30 * 86_400_000 - 1_000), "29d23h");
    }

    #[test]
    fn a_row_shows_budget_deadline_and_reserved_rate() {
        let mut app = app_with_two_peers();
        app.snapshot.peers[0].budget_expires_in_ms = 30 * 86_400_000;
        app.snapshot.peers[1].budget = 0;
        app.snapshot.peers[1].reserved_rate = 0;
        let screen = render(&mut app);
        let first = screen.iter().find(|l| l.contains("02111111")).unwrap();
        assert!(first.contains("512.0 KiB"), "{first}");
        assert!(first.contains("30d00h"), "{first}");
        assert!(first.contains("122.1 KiB/s"), "{first}");
        let second = screen.iter().find(|l| l.contains("02222222")).unwrap();
        assert!(
            second.contains("empty"),
            "an empty budget says so: {second}"
        );
    }

    #[test]
    fn the_detail_shows_both_directions_and_the_counters() {
        let mut app = app_with_two_peers();
        app.snapshot.peers[0].reported_balance = Some(control::BalanceSnapshot {
            remaining: 1 << 20,
            expires_in_ms: 2 * 86_400_000,
            reserved_rate: 0,
            age_ms: 3_000,
        });
        app.detail = true;
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for wanted in [
            "what this peer bought from us",
            "what we bought from this peer",
            "deadline",
            "reserved rate",
            "from-payer weight",
            "their last Balance",
            "said 3s ago",
            "pay per use",
            "to payer",
            "from payer",
        ] {
            assert!(screen.contains(wanted), "{wanted:?} missing:\n{screen}");
        }
    }
}
