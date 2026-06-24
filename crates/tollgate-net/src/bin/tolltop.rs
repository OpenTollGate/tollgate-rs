//! `tolltop` — a top-like live monitor for a running TollGate node.
//!
//! Reads the node's status over its Unix control socket (the TollGate analogue
//! of FIPS's `fipstop`). Runs a refreshing TUI by default; `--once` prints a
//! single plain-text table and exits (scriptable, no terminal control).

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;

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
    /// Print the status table once and exit (no TUI).
    #[arg(long)]
    once: bool,
    /// Refresh interval in milliseconds (TUI mode).
    #[arg(long, default_value_t = 1000)]
    interval: u64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.once {
        let status = control::query(&cli.socket)?;
        println!("{}", status::render_table(&status));
        return Ok(());
    }
    run_tui(&cli.socket, Duration::from_millis(cli.interval))
}

/// Drive the refreshing TUI until the user quits (`q`, `Esc`, or `Ctrl-C`).
/// Always restores the terminal, even on error.
fn run_tui(socket: &Path, interval: Duration) -> anyhow::Result<()> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    // In raw mode crossterm delivers Ctrl-C as a key event (Char('c') + CONTROL)
    // rather than a SIGINT, so we match it here alongside q / Esc.
    let is_quit = |key: KeyEvent| {
        matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    };

    let mut terminal = ratatui::init();
    let outcome = loop {
        let status = control::query(socket);
        if let Err(e) = terminal.draw(|frame| draw(frame, socket, status.as_ref())) {
            break Err(anyhow::Error::from(e));
        }
        // Block for up to `interval`, returning early if a key is pressed.
        match event::poll(interval) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if is_quit(key) => break Ok(()),
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

/// Render one frame: a header line, the peer table, and a footer — or an error
/// banner if the node can't be reached.
fn draw(frame: &mut ratatui::Frame, socket: &Path, status: Result<&NodeStatus, &anyhow::Error>) {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Borders, Cell, Row, Table};

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(frame.area());

    match status {
        Ok(s) => {
            let (active, suspended, other) = s.phase_counts();
            let header = format!(
                " tolltop — node {}  unit={}   peers: {} ({}A {}S {}O)",
                status::short(&s.pubkey),
                s.unit,
                s.peers.len(),
                active,
                suspended,
                other,
            );
            frame.render_widget(
                Line::from(header).style(Style::default().fg(Color::Cyan)),
                rows[0],
            );

            let head = Row::new([
                "PEER",
                "IP",
                "PHASE",
                "BALANCE",
                "ACCESS",
                "DELIVERED",
                "IDLE",
            ])
            .style(Style::default().add_modifier(Modifier::BOLD));

            let body = s.peers.iter().map(|p| {
                let access = if p.allowed {
                    Cell::from("allowed").style(Style::default().fg(Color::Green))
                } else {
                    Cell::from("blocked").style(Style::default().fg(Color::Red))
                };
                Row::new(vec![
                    Cell::from(status::short(&p.pubkey)),
                    Cell::from(p.ip.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(p.phase.clone()),
                    Cell::from(p.balance.to_string()),
                    access,
                    Cell::from(p.delivered.to_string()),
                    Cell::from(format!("{}s", p.idle_ms / 1000)),
                ])
            });

            let widths = [
                Constraint::Length(14),
                Constraint::Length(16),
                Constraint::Length(11),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(7),
            ];
            let table = Table::new(body, widths)
                .header(head)
                .block(Block::default().borders(Borders::ALL).title("peers"));
            frame.render_widget(table, rows[1]);
        }
        Err(e) => {
            let msg = format!("cannot reach node at {}: {e}", socket.display());
            frame.render_widget(
                Line::from(msg).style(Style::default().fg(Color::Red)),
                rows[1],
            );
        }
    }

    frame.render_widget(
        Line::from(" q / Ctrl-C to quit").style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}
