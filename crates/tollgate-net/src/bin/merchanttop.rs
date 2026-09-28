//! A live view of what a TollGate node charges, and what it holds.
//!
//! Reads `merchantd`'s control socket and redraws. Two tabs, because the
//! merchant does two separable things: it sells the vouchers that pay for this
//! node's capacity, and it holds what the node is paid. What the node is doing
//! for its peers is `tollgated`'s, and `tolltop` is where that is shown.
//!
//! The pricing tab is where an operator changes what the node charges. The
//! price is the one setting worth moving while a node runs — an uplink becomes
//! scarce, or stops being — and moving it touches no session, grant or channel:
//! it decides what the *next* buyer of vouchers pays, and nothing already sold.
//! It is set per Mbit in the unit the operator thinks in, and the table under it
//! is what that comes to in each accepted mint's tokens at today's BTC rate.
//!
//! The wallet tab is where the money is, and the only place the display asks
//! anything of the outside world: topping up produces an invoice somebody has
//! to pay, so it is shown as a QR as well as a string. The thing paying it is
//! usually a phone.

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
use tollgate_net::control::Response;
use tollgate_net::market::Accepted;
use tollgate_net::merchant::{self, Request, Snapshot};
use tollgate_net::pricing::{Accept, Price};
use tollgate_net::wallet::{Holding, Kind, TopUp};

#[derive(Parser, Debug)]
#[command(
    name = "merchanttop",
    about = "Watch a TollGate node's prices and wallet"
)]
struct Args {
    /// `merchantd`'s control socket. Its default place if not given.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

/// Which tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Pricing,
    Wallet,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Pricing, Tab::Wallet];

    fn label(self) -> &'static str {
        match self {
            Tab::Pricing => "pricing",
            Tab::Wallet => "wallet",
        }
    }

    /// With two tabs, next and previous are the same thing.
    fn other(self) -> Self {
        match self {
            Tab::Pricing => Tab::Wallet,
            Tab::Wallet => Tab::Pricing,
        }
    }
}

/// What a number being typed is for.
#[derive(Debug, Clone, PartialEq)]
enum Editing {
    /// One accepted mint's own price per Mbit, in `unit`.
    MintPrice { mint: String, unit: String },
    /// The price every mint without its own pays, in `unit`.
    DefaultPrice { unit: String },
    /// An amount to top the wallet up by.
    TopUp,
}

/// Everything the display is currently doing, as opposed to what the merchant
/// is.
struct App {
    socket: PathBuf,
    snapshot: Snapshot,
    /// Why the last refresh failed, if it did. The previous snapshot stays on
    /// screen: a merchant that is restarting should not erase what it was doing.
    error: Option<String>,
    /// Something the merchant refused, or said, kept until the operator does
    /// something else. It cannot share `error`, which is recomputed every
    /// refresh and would wipe this before it was read.
    notice: Option<String>,
    tab: Tab,
    /// Which accepted mint the cursor is on, over on the pricing tab.
    mints: TableState,
    /// `Some` while a number is being typed — a price, or an amount to top up
    /// by — with what it is for and the text so far. Editing is modal because a
    /// keystroke that means "quit" in one mode and "9" in the other has to be
    /// told which it is.
    editing: Option<(Editing, String)>,
    /// An invoice waiting to be paid, held on screen until it is or the
    /// operator dismisses it. The merchant collects what it bought by itself.
    invoice: Option<TopUp>,
}

impl App {
    fn selected_mint(&self) -> Option<&Accept> {
        self.mints
            .selected()
            .and_then(|i| self.snapshot.accepts.get(i))
    }

    /// Keep the pricing cursor on a row that exists: a mint stops being
    /// accepted when the merchant restarts with a different file.
    fn clamp_mints(&mut self) {
        clamp(&mut self.mints, self.snapshot.accepts.len());
    }

    fn move_mint(&mut self, delta: isize) {
        move_cursor(&mut self.mints, self.snapshot.accepts.len(), delta);
    }

    /// The price a mint is actually sold at: its own, or else the default.
    fn price_of<'a>(&'a self, accept: &'a Accept) -> Option<&'a Price> {
        accept.price.as_ref().or(self.snapshot.price.as_ref())
    }

    /// What one unit of a mint's tokens buys right now, if it can be priced.
    ///
    /// Looked up rather than computed here: the merchant's table is what the
    /// market actually sells by, and a display that did its own arithmetic
    /// could disagree with it.
    fn table_row(&self, accept: &Accept) -> Option<&Accepted> {
        self.snapshot.table.iter().find(|row| {
            row.mint.trim_end_matches('/') == accept.mint.trim_end_matches('/')
                && row.unit == accept.unit
        })
    }

    /// Start editing the price of the mint under the cursor.
    ///
    /// In the unit it is already priced in, so that editing a number does not
    /// quietly change what it is a number of. A mint on the default starts from
    /// the default, which is what it was being sold at.
    fn edit_mint_price(&mut self) {
        let Some(accept) = self.selected_mint() else {
            return;
        };
        let current = self.price_of(accept);
        let unit = current
            .map(|p| p.unit.clone())
            .unwrap_or_else(|| accept.unit.clone());
        let text = current.map(|p| p.per_mbit.to_string()).unwrap_or_default();
        self.editing = Some((
            Editing::MintPrice {
                mint: accept.mint.clone(),
                unit,
            },
            text,
        ));
        self.notice = None;
    }

    /// Start editing the default price, in its own unit, or sats if there is
    /// none yet.
    fn edit_default_price(&mut self) {
        let (unit, text) = match &self.snapshot.price {
            Some(p) => (p.unit.clone(), p.per_mbit.to_string()),
            None => ("sat".to_string(), String::new()),
        };
        self.editing = Some((Editing::DefaultPrice { unit }, text));
        self.notice = None;
    }

    /// Every place money is held, plus the mint the wallet is topped up at
    /// whether or not anything is held there.
    ///
    /// A balance is not the same question as a place to top up. A wallet that
    /// has spent everything still tops up where it always did, and showing only
    /// what is held would mean the one moment an operator needs to add money is
    /// the one moment there is nothing on screen to say where it goes.
    fn money(&self) -> Vec<Holding> {
        let mut rows: Vec<Holding> = self
            .snapshot
            .holdings
            .iter()
            .filter(|h| h.kind == Kind::Money)
            .cloned()
            .collect();

        if let Some(money) = &self.snapshot.money_mint {
            let known = rows.iter().any(|h| {
                h.mint.trim_end_matches('/') == money.mint.trim_end_matches('/')
                    && h.unit == money.unit
            });
            if !known {
                rows.push(Holding {
                    mint: money.mint.clone(),
                    unit: money.unit.clone(),
                    amount: 0,
                    kind: Kind::Money,
                });
            }
        }
        rows
    }
}

/// Point a cursor at a row that exists, or at nothing when there are none.
fn clamp(state: &mut TableState, count: usize) {
    match (count, state.selected()) {
        (0, _) => state.select(None),
        (_, None) => state.select(Some(0)),
        (n, Some(i)) if i >= n => state.select(Some(n - 1)),
        _ => {}
    }
}

/// Move a cursor, stopping at either end rather than wrapping.
///
/// Wrapping in a list this short reads as the cursor jumping rather than as
/// reaching the end.
fn move_cursor(state: &mut TableState, count: usize, delta: isize) {
    if count == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    state.select(Some((current + delta).clamp(0, count as isize - 1) as usize));
}

fn main() -> Result<()> {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(merchant::default_control_path);
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
        notice: None,
        tab: Tab::Pricing,
        mints: TableState::default(),
        editing: None,
        invoice: None,
    };

    loop {
        match runtime.block_on(fetch(&app.socket)) {
            Ok(fresh) => {
                app.snapshot = fresh;
                app.error = None;
            }
            Err(e) => app.error = Some(format!("{e:#}")),
        }
        app.clamp_mints();

        terminal.draw(|frame| draw(frame, &mut app))?;

        if event::poll(interval)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Editing takes every key it can use before anything else looks at
            // one: while a price is half-typed, `q` is not "quit".
            if let Some((what, text)) = app.editing.as_mut() {
                match key.code {
                    KeyCode::Esc => app.editing = None,
                    KeyCode::Enter => {
                        let (what, text) = (what.clone(), text.trim().to_string());
                        app.notice = submit(&runtime, &mut app, what, &text);
                        app.editing = None;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => text.push(c),
                    // A price is a decimal — a thousandth of a cent a Mbit is
                    // an ordinary one — and an amount of sats is not.
                    KeyCode::Char('.') if *what != Editing::TopUp && !text.contains('.') => {
                        text.push('.')
                    }
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Right | KeyCode::Left => {
                    app.tab = app.tab.other();
                    app.notice = None;
                }
                // Esc backs out of whatever is open, and quits from the top.
                KeyCode::Esc if app.invoice.is_some() => app.invoice = None,
                KeyCode::Esc => break,

                KeyCode::Up | KeyCode::Char('k') if app.tab == Tab::Pricing => app.move_mint(-1),
                KeyCode::Down | KeyCode::Char('j') if app.tab == Tab::Pricing => app.move_mint(1),

                // Editing the mint under the cursor is the thing to do on the
                // pricing tab, so Enter starts it as well as `e`.
                KeyCode::Enter | KeyCode::Char('e') if app.tab == Tab::Pricing => {
                    app.edit_mint_price()
                }
                KeyCode::Char('d') if app.tab == Tab::Pricing => app.edit_default_price(),

                // Topping up is the only thing to do on the wallet tab, so
                // Enter starts it as well as `t`.
                KeyCode::Enter | KeyCode::Char('t') if app.tab == Tab::Wallet => {
                    app.editing = Some((Editing::TopUp, String::new()));
                    app.invoice = None;
                    app.notice = None;
                }

                // Collect an invoice that was paid while nothing was watching
                // for it — a merchant restarted mid-wait, or a mint that
                // rate-limited the status polls until the waiter gave up.
                KeyCode::Char('c') if app.tab == Tab::Wallet => {
                    app.notice = match runtime.block_on(claim(&app.socket)) {
                        Ok(0) => Some("nothing paid for and unclaimed".into()),
                        Ok(claimed) => Some(format!("claimed {claimed}")),
                        Err(e) => Some(format!("{e:#}")),
                    };
                    app.invoice = None;
                }
                _ => {}
            }
        }
    }

    ratatui::restore();
    Ok(())
}

/// Carry out what was typed, and say what went wrong if anything did.
///
/// Nothing typed is a change of mind, not a zero: stopping selling for a mint
/// is worth typing a `0` for.
fn submit(
    runtime: &tokio::runtime::Runtime,
    app: &mut App,
    what: Editing,
    text: &str,
) -> Option<String> {
    let (mint, unit) = match what {
        Editing::TopUp => {
            let Ok(amount) = text.parse::<u64>() else {
                return None;
            };
            return match runtime.block_on(top_up(&app.socket, amount)) {
                Ok(invoice) => {
                    app.invoice = Some(invoice);
                    None
                }
                Err(e) => Some(format!("{e:#}")),
            };
        }
        Editing::MintPrice { mint, unit } => (Some(mint), unit),
        Editing::DefaultPrice { unit } => (None, unit),
    };

    let per_mbit = match text.parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => n,
        _ => return None,
    };
    match runtime.block_on(set_price(&app.socket, mint, Price { unit, per_mbit })) {
        // The answer is the repriced snapshot, so the new table shows now
        // rather than on the next refresh.
        Ok(fresh) => {
            app.snapshot = fresh;
            None
        }
        Err(e) => Some(format!("{e:#}")),
    }
}

/// Ask for, and unwrap, whatever the merchant answers to `request`.
async fn ask(socket: &std::path::Path, request: &Request) -> Result<serde_json::Value> {
    match merchant::send(socket, request).await? {
        Response::Ok { data } => Ok(data),
        Response::Error { message } => Err(anyhow::anyhow!(message)),
    }
}

/// Read one snapshot from the merchant.
async fn fetch(socket: &std::path::Path) -> Result<Snapshot> {
    Ok(serde_json::from_value(
        ask(socket, &Request::Snapshot).await?,
    )?)
}

/// Ask the merchant to buy money, and hand back the invoice that pays for it.
///
/// Always at the mint the wallet is configured to top up at: that is the one
/// place it knows how to buy.
async fn top_up(socket: &std::path::Path, amount: u64) -> Result<TopUp> {
    Ok(serde_json::from_value(
        ask(socket, &Request::TopUp { amount }).await?,
    )?)
}

/// Ask the merchant to collect anything paid for and not yet claimed.
async fn claim(socket: &std::path::Path) -> Result<u64> {
    let data = ask(socket, &Request::Claim).await?;
    Ok(data
        .get("claimed")
        .and_then(|c| c.as_u64())
        .unwrap_or_default())
}

/// Change the default price, or one mint's, and fail loudly if the merchant
/// will not.
async fn set_price(
    socket: &std::path::Path,
    mint: Option<String>,
    price: Price,
) -> Result<Snapshot> {
    Ok(serde_json::from_value(
        ask(socket, &Request::SetPrice { mint, price }).await?,
    )?)
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [tabs, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    frame.render_widget(tab_bar(app), tabs);
    match app.tab {
        Tab::Pricing => pricing_tab(frame, app, body),
        Tab::Wallet => wallet_tab(frame, app, body),
    }
    frame.render_widget(status(app), footer);
}

/// The tabs, and nothing else. What the merchant charges belongs in the status
/// bar, where it stays visible whichever tab is open.
fn tab_bar(app: &App) -> Paragraph<'_> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
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

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" merchanttop "),
    )
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

/// What this node charges, and what a unit of each accepted mint's tokens buys.
///
/// The default price and the rates it goes through first, then one row per
/// mint. A list rather than a number, because the price of capacity is not one
/// price: a sat from a mint you expect to honour its tokens is worth more than a
/// sat from one you do not, and pricing mints is how an operator says so.
fn pricing_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    let [head, rest] = Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).areas(area);
    frame.render_widget(pricing_head(app), head);

    if app.snapshot.accepts.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled(
                "This node takes no tokens as payment.",
                Style::default().fg(Color::Red),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Nobody can buy its vouchers here, which is a working setting for a",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "node whose capacity is sold somewhere else. Add mints under",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "`accepts` in merchant.yaml to take payment.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" accepted "));
        frame.render_widget(empty, rest);
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let rows: Vec<Row> = app
        .snapshot
        .accepts
        .iter()
        .map(|a| {
            // A mint on the default says so, rather than repeating the default
            // as if it were a price of its own: changing the default moves it.
            let price = match &a.price {
                Some(p) => Span::raw(per_mbit(p)),
                None => Span::styled("default", dim),
            };
            // No row in the table is a mint that cannot be priced right now —
            // no price at all, or a rate never fetched. It is not sold at.
            let (bytes, gigabyte) = match app.table_row(a) {
                Some(row) => (
                    Span::raw(row.bytes_per_unit.to_string()),
                    Span::raw(costs(1_000_000_000, row)),
                ),
                None => (
                    Span::styled("not sold", Style::default().fg(Color::Red)),
                    Span::styled("—", dim),
                ),
            };
            Row::new(vec![
                Cell::from(a.mint.clone()),
                Cell::from(a.unit.clone()),
                Cell::from(price),
                Cell::from(bytes),
                Cell::from(gigabyte),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(24),    // mint
            Constraint::Length(6),  // unit
            Constraint::Length(20), // price
            Constraint::Length(15), // bytes per unit
            Constraint::Length(14), // a gigabyte
        ],
    )
    .header(header_row(&[
        "mint",
        "unit",
        "price",
        "bytes per unit",
        "a gigabyte",
    ]))
    .row_highlight_style(SELECTED)
    .highlight_symbol(CURSOR)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" accepted — what this node takes, and what it gives for it "),
    );

    frame.render_stateful_widget(table, rest, &mut app.mints);
}

/// The default price, and the BTC rates every cross-currency price goes
/// through.
fn pricing_head(app: &App) -> Paragraph<'_> {
    let dim = Style::default().fg(Color::DarkGray);
    let default = match &app.snapshot.price {
        Some(p) => Span::styled(per_mbit(p), Style::default().add_modifier(Modifier::BOLD)),
        None => Span::styled(
            "none — only mints with a price of their own are sold at",
            Style::default().fg(Color::Yellow),
        ),
    };
    let rates = if app.snapshot.rates.is_empty() {
        // Not necessarily a fault: a sat price paid in sats needs no rate.
        Span::styled("none fetched", dim)
    } else {
        Span::raw(
            app.snapshot
                .rates
                .iter()
                .map(|(currency, btc)| format!("{btc:.0} {currency}"))
                .collect::<Vec<_>>()
                .join("   "),
        )
    };

    Paragraph::new(vec![
        Line::from(vec![
            Span::styled(format!("{:<16}", "default price"), dim),
            default,
        ]),
        Line::from(vec![Span::styled(format!("{:<16}", "1 BTC"), dim), rates]),
    ])
    .block(Block::default().borders(Borders::ALL).title(" pricing "))
}

/// A price as it is set: so much of its unit per Mbit.
///
/// Printed as typed rather than rounded — `0.00001 usd` is an ordinary price,
/// and rounding it to two places would show a free one.
fn per_mbit(price: &Price) -> String {
    format!("{} {}/Mbit", price.per_mbit, price.unit)
}

/// What this node is holding.
///
/// Money first, then other people's vouchers, because they are not the same
/// kind of thing: money is what this node can pay with, and a peer's vouchers
/// are what it has been paid — a claim on that peer's capacity that nobody but
/// that peer will honour.
fn wallet_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    // An invoice takes the whole tab while it is unpaid: it is the one thing on
    // screen the operator has to act on, and a QR wants the room.
    if let Some(invoice) = &app.invoice {
        invoice_panel(frame, invoice, area);
        return;
    }

    // The money held, and the mint it is topped up at whether or not it holds
    // any — an empty row is where a top-up goes.
    let money = app.money();
    let transit: Vec<Holding> = app
        .snapshot
        .holdings
        .iter()
        .filter(|h| h.kind == Kind::PrepaidTransit)
        .cloned()
        .collect();

    if money.is_empty() && transit.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled(
                "This node holds nothing, and has nowhere to buy.",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "It can still sell — it is paid in money it can hold — but it cannot buy",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "transit from an upstream until `wallet.mint` names a mint to buy at.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" wallet "));
        frame.render_widget(empty, area);
        return;
    }

    // Two tables rather than one with a column saying which is which. Both are
    // spending power; the difference is who will take it. Money buys from
    // whoever accepts its mint, and a byte-denominated holding is capacity
    // already bought from one upstream — good there and nowhere else.
    let [top, bottom] = Layout::vertical([
        Constraint::Length(money.len().max(1) as u16 + 3),
        Constraint::Min(3),
    ])
    .areas(area);

    frame.render_widget(
        holdings_table(
            &money,
            " money — buys from any peer that accepts the mint ",
            Color::Green,
        ),
        top,
    );
    frame.render_widget(
        holdings_table(
            &transit,
            " prepaid transit — bought from an upstream, spendable only there ",
            Color::Cyan,
        ),
        bottom,
    );
}

fn holdings_table<'a>(holdings: &[Holding], title: &'a str, colour: Color) -> Table<'a> {
    let rows: Vec<Row> = if holdings.is_empty() {
        vec![Row::new(vec![Cell::from(Span::styled(
            "—",
            Style::default().fg(Color::DarkGray),
        ))])]
    } else {
        holdings
            .iter()
            .map(|h| {
                // A mint with nothing in it is a place to put money rather than
                // money, so it is dimmed instead of coloured.
                let amount = if h.amount == 0 {
                    Span::styled("—", Style::default().fg(Color::DarkGray))
                } else {
                    Span::styled(
                        held(h),
                        Style::default().fg(colour).add_modifier(Modifier::BOLD),
                    )
                };
                Row::new(vec![
                    Cell::from(h.mint.clone()),
                    Cell::from(h.unit.clone()),
                    Cell::from(amount),
                ])
            })
            .collect()
    };

    Table::new(
        rows,
        [
            Constraint::Min(24),    // issuer
            Constraint::Length(6),  // unit
            Constraint::Length(16), // held
        ],
    )
    .header(header_row(&["issuer", "unit", "held"]))
    .block(Block::default().borders(Borders::ALL).title(title))
}

/// A holding, in the units its own kind is read in.
///
/// Money is counted: 900 sat is 900 sat, and rounding it to "0.9k" hides the
/// thing being counted. Capacity is measured, so it reads as bytes do
/// everywhere else.
fn held(holding: &Holding) -> String {
    match holding.kind {
        Kind::Money => format!("{} {}", holding.amount, holding.unit),
        Kind::PrepaidTransit => units(holding.amount),
    }
}

/// The invoice for a top-up, and nothing else on screen.
///
/// A QR beside the string, because the thing paying is a phone. The string
/// stays: a QR is no use over ssh into a terminal being read through a scroll
/// buffer, and it is what an operator copies into a wallet on the same machine.
fn invoice_panel(frame: &mut Frame, invoice: &TopUp, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" top up — esc to dismiss ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(Color::DarkGray);
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("buying ", dim),
            Span::styled(
                format!("{} {}", invoice.amount, invoice.unit),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" at ", dim),
            Span::raw(invoice.mint.clone()),
        ]),
        Line::from(Span::styled(
            "Scan or paste. The merchant collects what it buys by itself, and the \
             balance appears when it lands — there is nothing to confirm.",
            dim,
        )),
    ])
    .wrap(Wrap { trim: false });

    let qr = qr_lines(&invoice.request);
    // Height first: a QR that does not fit whole is not a QR, so if the pane is
    // too short the string gets the room instead.
    let qr_height = qr.len() as u16;
    let qr_width = qr.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let fits = qr_height + 3 <= inner.height && qr_width + 20 <= inner.width;

    let [head, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(inner);
    frame.render_widget(header, head);

    if !fits {
        let mut lines = vec![Line::from(Span::styled(
            invoice.request.clone(),
            Style::default().fg(Color::Yellow),
        ))];
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("(a {qr_width}×{qr_height} QR needs a taller window)"),
            dim,
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        return;
    }

    let [left, right] =
        Layout::horizontal([Constraint::Length(qr_width + 2), Constraint::Min(20)]).areas(body);

    // Black on white, whatever the terminal's own colours are: a scanner needs
    // dark modules on a light field, and a QR drawn in the terminal's
    // foreground colour on its background is as likely to be the negative.
    frame.render_widget(
        Paragraph::new(
            qr.into_iter()
                .map(|line| Line::from(Span::raw(line)))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(Color::Black).bg(Color::White)),
        left,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            invoice.request.clone(),
            Style::default().fg(Color::Yellow),
        ))
        .wrap(Wrap { trim: false }),
        right,
    );
}

/// A bolt11 invoice as terminal-sized QR rows.
///
/// Uppercased first. A bech32 invoice is case-insensitive, and uppercase is
/// what lets the encoder use alphanumeric mode instead of binary — roughly a
/// third fewer modules, which is the difference between a QR that fits an
/// ordinary window and one that does not. Two module rows per line of text,
/// for the same reason.
fn qr_lines(invoice: &str) -> Vec<String> {
    use qrcode::QrCode;
    use qrcode::render::unicode;

    let Ok(code) = QrCode::new(invoice.to_uppercase().as_bytes()) else {
        return Vec::new();
    };
    code.render::<unicode::Dense1x2>()
        .quiet_zone(true)
        .build()
        .lines()
        .map(str::to_string)
        .collect()
}

/// What a quantity costs in one mint's tokens.
fn costs(bytes: u64, row: &Accepted) -> String {
    if row.bytes_per_unit == 0 {
        return "—".into();
    }
    let units = bytes as f64 / row.bytes_per_unit as f64;
    if units >= 1.0 {
        format!("{units:.0} {}", row.unit)
    } else {
        format!("{units:.3} {}", row.unit)
    }
}

fn status(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);

    // Editing wins over everything: the operator is mid-keystroke, and what
    // they need to see is what they have typed so far.
    if let Some((what, text)) = &app.editing {
        // What a number means depends on what asked for it.
        let (prompt, help) = match what {
            Editing::TopUp => (
                "top up by (sat): ".to_string(),
                "   enter to buy, esc to cancel",
            ),
            Editing::MintPrice { unit, .. } => (
                format!("{unit} per Mbit at this mint: "),
                "   enter to set, esc to cancel, 0 to stop selling for this mint",
            ),
            Editing::DefaultPrice { unit } => (
                format!("default {unit} per Mbit: "),
                "   enter to set, esc to cancel",
            ),
        };
        return Paragraph::new(Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{text}_"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(help, dim),
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

    let hints = match (app.tab, app.invoice.is_some()) {
        (Tab::Pricing, _) => {
            "↑↓ selects   e prices this mint   d sets the default   tab switches   q quits"
        }
        (Tab::Wallet, true) => "esc dismisses   c claims a paid invoice   q quits",
        (Tab::Wallet, false) => "t tops up   c claims a paid invoice   tab switches   q quits",
    };

    // What the merchant charges and holds stays visible on every tab: the tab
    // bar above says what is being looked at, not what it adds up to.
    let money: u64 = app
        .snapshot
        .holdings
        .iter()
        .filter(|h| h.kind == Kind::Money && h.unit == "sat")
        .map(|h| h.amount)
        .sum();
    Paragraph::new(Line::from(vec![
        Span::styled("default ", dim),
        Span::raw(
            app.snapshot
                .price
                .as_ref()
                .map(per_mbit)
                .unwrap_or_else(|| "none".into()),
        ),
        Span::styled("  takes ", dim),
        Span::raw(match app.snapshot.accepts.len() {
            0 => "nothing".to_string(),
            1 => "1 mint".to_string(),
            n => format!("{n} mints"),
        }),
        Span::styled("  holds ", dim),
        Span::styled(format!("{money} sat"), Style::default().fg(Color::Green)),
        Span::styled(format!("   {hints}"), dim),
    ]))
    .block(Block::default().borders(Borders::ALL))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A real bolt11 invoice, from the fake mint the topologies run.
    const INVOICE: &str = "lnbc2500n1p48t9kydqqpp5atkdtq5tetqkr3acy4cxhgzc0pn9lw7pyahumh70g43qy9qt2r9qsp59g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4q9qrsgq";

    #[test]
    fn an_invoice_becomes_a_square_qr() {
        let lines = qr_lines(INVOICE);
        assert!(!lines.is_empty(), "an invoice should encode");

        // Two module rows per line of text, so the drawing is half as tall as
        // it is wide — give or take the odd row.
        let width = lines.iter().map(|l| l.chars().count()).max().unwrap();
        let height = lines.len();
        assert!(
            height * 2 >= width - 2 && height * 2 <= width + 2,
            "{width}×{height} is not a square QR"
        );
        assert!(
            width < 80,
            "{width} columns will not fit an ordinary window"
        );
    }

    #[test]
    fn uppercasing_the_invoice_makes_the_qr_smaller() {
        // Bech32 is case-insensitive, and uppercase is what lets the encoder
        // use alphanumeric mode instead of binary. It is the difference between
        // a QR that fits a window and one that does not.
        use qrcode::QrCode;
        let binary = QrCode::new(INVOICE.as_bytes()).expect("encode").width();
        let alphanumeric = QrCode::new(INVOICE.to_uppercase().as_bytes())
            .expect("encode")
            .width();
        assert!(
            alphanumeric < binary,
            "uppercase {alphanumeric} should beat mixed case {binary}"
        );
    }

    #[test]
    fn something_that_will_not_encode_is_not_a_panic() {
        // 4 kB is past what any QR version holds. A display that fell over
        // because a mint wrote a long invoice would be worse than one showing
        // the string alone.
        assert!(qr_lines(&"x".repeat(4096)).is_empty());
    }

    #[test]
    fn a_small_price_is_shown_as_set_and_not_rounded_away() {
        let price = Price {
            unit: "usd".into(),
            per_mbit: 0.00001,
        };
        assert_eq!(per_mbit(&price), "0.00001 usd/Mbit");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_with_two_mints() -> App {
        let mut app = App {
            socket: PathBuf::from("/tmp/x.sock"),
            snapshot: Snapshot {
                price: Some(Price {
                    unit: "usd".into(),
                    per_mbit: 0.001,
                }),
                accepts: vec![
                    Accept {
                        mint: "https://one.example".into(),
                        unit: "sat".into(),
                        price: Some(Price {
                            unit: "sat".into(),
                            per_mbit: 0.8,
                        }),
                    },
                    Accept {
                        mint: "https://two.example".into(),
                        unit: "sat".into(),
                        price: None,
                    },
                ],
                table: vec![
                    Accepted {
                        mint: "https://one.example".into(),
                        unit: "sat".into(),
                        bytes_per_unit: 156_250,
                    },
                    Accepted {
                        mint: "https://two.example".into(),
                        unit: "sat".into(),
                        bytes_per_unit: 125_000,
                    },
                ],
                rates: [("usd".to_string(), 100_000.0)].into_iter().collect(),
                holdings: vec![
                    Holding {
                        mint: "https://one.example".into(),
                        unit: "sat".into(),
                        amount: 900,
                        kind: Kind::Money,
                    },
                    // Bought from an upstream and not yet spent — spending
                    // power, but only at the node that issued it.
                    Holding {
                        mint: "https://upstream.example".into(),
                        unit: "byte".into(),
                        amount: 4096,
                        kind: Kind::PrepaidTransit,
                    },
                ],
                money_mint: None,
            },
            error: None,
            notice: None,
            tab: Tab::Pricing,
            mints: TableState::default(),
            editing: None,
            invoice: None,
        };
        app.clamp_mints();
        app
    }

    fn render(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(110, 20)).expect("terminal");
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
        let mut app = app_with_two_mints();
        let screen = render(&mut app);

        let marked = cursored(&screen);
        assert_eq!(marked.len(), 1, "exactly one row carries the cursor");
        assert!(marked[0].contains("one.example"), "{:?}", marked);

        let header = screen
            .iter()
            .find(|l| l.contains("bytes per unit"))
            .expect("a header");
        assert!(
            !header.contains(CURSOR.trim()),
            "the header is a label, not a selection: {header:?}"
        );
    }

    #[test]
    fn the_cursor_moves_between_mints_and_stops_at_the_ends() {
        let mut app = app_with_two_mints();

        app.move_mint(1);
        assert!(cursored(&render(&mut app))[0].contains("two.example"));

        // Past the end stays at the end rather than wrapping, which in a list
        // this short reads as the cursor jumping.
        app.move_mint(1);
        assert!(cursored(&render(&mut app))[0].contains("two.example"));

        app.move_mint(-5);
        assert!(cursored(&render(&mut app))[0].contains("one.example"));
    }

    #[test]
    fn a_mint_on_the_default_says_so_and_one_that_cannot_be_priced_is_not_sold() {
        let mut app = app_with_two_mints();
        // The rate went away before the table was recomputed without it.
        app.snapshot.table.pop();
        let screen = render(&mut app).join("\n");

        assert!(screen.contains("0.8 sat/Mbit"), "its own price: {screen}");
        assert!(screen.contains("default"), "{screen}");
        assert!(screen.contains("0.001 usd/Mbit"), "the default: {screen}");
        assert!(screen.contains("100000 usd"), "the rate: {screen}");
        assert!(screen.contains("not sold"), "{screen}");
        // 1 GB at 156 250 bytes a sat.
        assert!(screen.contains("6400 sat"), "{screen}");
    }

    #[test]
    fn editing_a_price_starts_from_what_it_is_sold_at_in_its_own_unit() {
        // A mint on the default is being sold at the default, so that is the
        // number to start from — and the unit, so that editing a number does
        // not quietly change what it is a number of.
        let mut app = app_with_two_mints();
        app.move_mint(1);
        app.edit_mint_price();
        assert_eq!(
            app.editing,
            Some((
                Editing::MintPrice {
                    mint: "https://two.example".into(),
                    unit: "usd".into(),
                },
                "0.001".into()
            ))
        );

        app.edit_default_price();
        assert_eq!(
            app.editing,
            Some((Editing::DefaultPrice { unit: "usd".into() }, "0.001".into()))
        );
    }

    #[test]
    fn the_mint_money_is_bought_at_gets_a_row_even_when_empty() {
        // The one moment an operator needs to add money is the moment there is
        // none — so the mint a top-up goes to still has to be on screen.
        let mut app = app_with_two_mints();
        app.tab = Tab::Wallet;
        app.snapshot.money_mint = Some(Accepted {
            mint: "https://money.example".into(),
            unit: "sat".into(),
            bytes_per_unit: 0,
        });

        let screen = render(&mut app).join("\n");
        assert!(screen.contains("one.example"), "the held one: {screen}");
        assert!(screen.contains("money.example"), "the empty one: {screen}");
        assert_eq!(app.money().len(), 2);
    }

    #[test]
    fn the_two_halves_say_who_will_take_them() {
        // The labels are the point. Both halves are spending power; what
        // differs is whether it is general or good at exactly one node — and
        // neither of them is "what peers have paid us", because a peer pays in
        // *this* node's paper, which redeeming cancels rather than banks.
        let mut app = app_with_two_mints();
        app.tab = Tab::Wallet;
        let screen = render(&mut app).join("\n");

        assert!(screen.contains("buys from any peer"), "{screen}");
        assert!(screen.contains("spendable only there"), "{screen}");
    }
}
