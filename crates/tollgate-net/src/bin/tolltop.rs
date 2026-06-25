//! `tolltop` — a top-like live monitor for a running TollGate node.
//!
//! Reads the node's status over its Unix control socket (the TollGate analogue
//! of FIPS's `fipstop`). Runs a refreshing tabbed TUI by default; `--once` prints
//! a single plain-text snapshot and exits (scriptable, no terminal control).

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use ratatui::layout::Rect;

use tollgate_net::control;
use tollgate_net::status::{self, NodeStatus};

#[derive(Parser, Debug)]
#[command(
    name = "tolltop",
    version,
    about = "Live monitor for a TollGate node (reads its control socket)"
)]
struct Cli {
    /// Control socket of the node to watch.
    #[arg(long, default_value = "/tmp/tollgate.sock")]
    socket: PathBuf,
    /// Print the status once and exit (no TUI).
    #[arg(long)]
    once: bool,
    /// Refresh interval in milliseconds (TUI mode).
    #[arg(long, default_value_t = 1000)]
    interval: u64,
}

/// The TUI tabs, in order.
const TABS: [&str; 2] = ["Peers", "Pricing"];

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.once {
        let status = control::query(&cli.socket)?;
        println!("{}", status::render_table(&status));
        println!();
        println!("{}", status::render_pricing(&status));
        return Ok(());
    }
    run_tui(&cli.socket, Duration::from_millis(cli.interval))
}

/// Drive the refreshing TUI until the user quits (`q`, `Esc`, or `Ctrl-C`).
/// `Tab`/`←`/`→`/number keys switch tabs. Always restores the terminal.
fn run_tui(socket: &Path, interval: Duration) -> anyhow::Result<()> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    // In raw mode crossterm delivers Ctrl-C as a key event (Char('c') + CONTROL)
    // rather than a SIGINT, so we match it here alongside q / Esc.
    let is_quit = |key: KeyEvent| {
        matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    };

    let mut tab = 0usize;
    let mut terminal = ratatui::init();
    let outcome = loop {
        let status = control::query(socket);
        if let Err(e) = terminal.draw(|frame| draw(frame, socket, status.as_ref(), tab)) {
            break Err(anyhow::Error::from(e));
        }
        // Block for up to `interval`, returning early on a key press.
        match event::poll(interval) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if is_quit(key) => break Ok(()),
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Tab | KeyCode::Right => tab = (tab + 1) % TABS.len(),
                    KeyCode::BackTab | KeyCode::Left => tab = (tab + TABS.len() - 1) % TABS.len(),
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let n = c as usize - '0' as usize;
                        if (1..=TABS.len()).contains(&n) {
                            tab = n - 1;
                        }
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => break Err(anyhow::Error::from(e)),
            },
            Ok(false) => {}
            Err(e) => break Err(anyhow::Error::from(e)),
        }
    };
    ratatui::restore();
    outcome
}

/// Render one frame: header, tab bar, the selected tab's content, footer — or an
/// error banner if the node can't be reached.
fn draw(
    frame: &mut ratatui::Frame,
    socket: &Path,
    status: Result<&NodeStatus, &anyhow::Error>,
    tab: usize,
) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use ratatui::widgets::Tabs;

    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // tab bar
        Constraint::Min(0),    // content
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    let header = match status {
        Ok(s) => format!(
            " tolltop — node {}  unit={}",
            status::short(&s.pubkey),
            s.unit
        ),
        Err(_) => " tolltop".to_string(),
    };
    frame.render_widget(
        Line::from(header).style(Style::default().fg(Color::Cyan)),
        rows[0],
    );

    let tabs = Tabs::new(TABS.to_vec()).select(tab).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(tabs, rows[1]);

    match status {
        Ok(s) if tab == 1 => render_pricing(frame, rows[2], s),
        Ok(s) => render_peers(frame, rows[2], s),
        Err(e) => {
            let msg = format!("cannot reach node at {}: {e}", socket.display());
            frame.render_widget(
                Line::from(msg).style(Style::default().fg(Color::Red)),
                rows[2],
            );
        }
    }

    frame.render_widget(
        Line::from(" q / Ctrl-C: quit    Tab / ←→: switch tabs")
            .style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );
}

/// The Peers tab: one row per tracked peer.
fn render_peers(frame: &mut ratatui::Frame, area: Rect, s: &NodeStatus) {
    use ratatui::layout::Constraint;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let (active, suspended, other) = s.state_counts();
    let title = format!(
        "peers: {} ({}A {}S {}O)",
        s.peers.len(),
        active,
        suspended,
        other
    );

    // One row per peer — a peering is bidirectional. DELIVERED/RECEIVED = units we
    // delivered to them / they delivered to us. WE_HOLD = their prepayment we hold
    // (our own ledger — reliable); THEY_HOLD = our prepayment they hold (our
    // estimate from their reports — trust it only as far as DRIFT allows). NET =
    // the signed position (+ earner / - spender). DRIFT = metering disagreement.
    let head = Row::new([
        "PEER",
        "IP",
        "STATE",
        "DELIVERED",
        "RECEIVED",
        "WE_HOLD",
        "THEY_HOLD",
        "NET",
        "DRIFT",
        "METERED",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body =
        s.peers.iter().map(|p| {
            let net = p.net_balance();
            let net_cell = Cell::from(status::fmt_net(net))
                .style(Style::default().fg(if net >= 0 { Color::Green } else { Color::Red }));
            Row::new(vec![
                Cell::from(status::short(&p.pubkey)),
                Cell::from(p.ip.clone().unwrap_or_else(|| "-".to_string())),
                Cell::from(p.state.clone()),
                Cell::from(status::fmt_units(p.delivered, &s.unit)),
                Cell::from(status::fmt_units(p.received, &s.unit)),
                Cell::from(status::fmt_held(p.their_balance)),
                Cell::from(status::fmt_held(p.our_balance)),
                net_cell,
                Cell::from(status::fmt_drift(p.drift)),
                Cell::from(format!("{}s", p.metered_secs)),
            ])
        });

    let widths = [
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(8),
    ];
    let table = Table::new(body, widths)
        .header(head)
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(table, area);
}

/// The Pricing tab: what this node advertises (products × mint options).
fn render_pricing(frame: &mut ratatui::Frame, area: Rect, s: &NodeStatus) {
    use ratatui::layout::Constraint;
    use ratatui::style::{Modifier, Style};
    use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

    let p = &s.pricing;
    // We sell `s.unit` (e.g. bytes); prices are per that unit / per second, in
    // each mint's currency (the CCY column).
    let title = format!(
        "pricing — selling {}  ·  {} product(s)  interval {}–{} ms",
        s.unit,
        p.products.len(),
        p.min_interval_ms,
        p.max_interval_ms
    );
    let block = Block::default().borders(Borders::ALL).title(title);

    if p.products.is_empty() {
        let para = Paragraph::new(" no products — this node sells nothing").block(block);
        frame.render_widget(para, area);
        return;
    }

    let head = Row::new(["PRODUCT", "MINT", "CCY", "PER_SEC", "PER_UNIT"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let mut body: Vec<Row> = Vec::new();
    for product in &p.products {
        if product.mints.is_empty() {
            body.push(Row::new(vec![
                Cell::from(status::short(&product.product_id)),
                Cell::from("(no mints)"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
            ]));
        }
        for m in &product.mints {
            body.push(Row::new(vec![
                Cell::from(status::short(&product.product_id)),
                Cell::from(m.mint_url.clone()),
                Cell::from(m.mint_unit.clone()),
                Cell::from(m.price_per_second.to_string()),
                Cell::from(m.price_per_unit.to_string()),
            ]));
        }
    }

    let widths = [
        Constraint::Length(14),
        Constraint::Length(28),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(9),
    ];
    let table = Table::new(body, widths).header(head).block(block);
    frame.render_widget(table, area);
}
