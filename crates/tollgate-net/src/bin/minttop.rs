//! A live view of what a node's mint is doing.
//!
//! Reads `mintd`'s control socket and redraws. The mint does one thing — issue
//! and redeem this node's vouchers — so this is one screen: where it is served,
//! whether it gives vouchers away, and how many quotes each listener has
//! served. The private listener's count is what `merchantd` has sold; the
//! public one's is what auto-accept has given away.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use tollgate_net::mintd::{Snapshot, default_control_path};

#[derive(Parser, Debug)]
#[command(name = "minttop", about = "Watch a TollGate node's mint")]
struct Args {
    /// `mintd`'s control socket.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(default_control_path);
    let interval = Duration::from_millis(args.interval.max(50));

    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            let snapshot = read(&socket);
            terminal.draw(|frame| draw(frame, &socket, &snapshot))?;
            if event::poll(interval)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    result
}

/// One snapshot, or why there is none.
fn read(socket: &std::path::Path) -> Result<Snapshot, String> {
    fetch(socket).map_err(|e| format!("{e:#}"))
}

#[cfg(unix)]
fn fetch(socket: &std::path::Path) -> Result<Snapshot> {
    use std::io::Read;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("connect to {}; is mintd running?", socket.display()))?;
    let mut body = Vec::new();
    stream.read_to_end(&mut body)?;
    serde_json::from_slice(&body).context("read mintd's snapshot")
}

#[cfg(not(unix))]
fn fetch(_socket: &std::path::Path) -> Result<Snapshot> {
    anyhow::bail!("the control socket is a Unix socket")
}

fn draw(frame: &mut Frame, socket: &std::path::Path, snapshot: &Result<Snapshot, String>) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let title = Line::from(vec![
        Span::styled("minttop", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {}", socket.display())),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)),
        header,
    );

    match snapshot {
        Err(e) => frame.render_widget(
            Paragraph::new(e.as_str()).style(Style::default().fg(Color::Red)),
            body,
        ),
        Ok(s) => frame.render_widget(table(s), body),
    }

    frame.render_widget(Paragraph::new("q quit"), footer);
}

fn table(s: &Snapshot) -> Table<'static> {
    let free = if s.auto_accept {
        Cell::from("on — service is free").style(Style::default().fg(Color::Yellow))
    } else {
        Cell::from("off")
    };
    let limit = if s.issue_quotes_per_minute == 0 {
        "unlimited".to_string()
    } else {
        format!("{} quotes/min", s.issue_quotes_per_minute)
    };
    let row = |k: &'static str, v: String| Row::new([Cell::from(k), Cell::from(v)]);
    let rows = vec![
        row("url", s.url.clone()),
        row("unit", s.unit.clone()),
        row("keyset", s.keyset.clone()),
        row("public", s.public.clone()),
        row("private", s.private.clone()),
        Row::new([Cell::from("auto-accept"), free]),
        row("public limit", limit),
        row("", String::new()),
        row("sold (private quotes)", s.private_quotes.to_string()),
        row("given away (public)", s.public_quotes.to_string()),
        row("refused (over limit)", s.refused_quotes.to_string()),
        row("uptime", uptime(s.uptime_secs)),
    ];
    Table::new(rows, [Constraint::Length(24), Constraint::Min(0)])
        .block(Block::default().borders(Borders::ALL).title("mint"))
}

fn uptime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m{s:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_reads_as_hours_and_minutes_once_it_is_long() {
        assert_eq!(uptime(59), "0m59s");
        assert_eq!(uptime(3_725), "1h02m");
    }
}
