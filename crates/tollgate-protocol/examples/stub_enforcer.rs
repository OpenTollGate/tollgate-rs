//! A stub enforcer: the smallest program that speaks the enforcer protocol.
//!
//! It owns no traffic. It listens on a Unix socket, says `hello`, keeps the
//! bindings and rates `tollgated` sends, refuses a subject another payer
//! already holds, and — with `--carry` — pretends every open payer moved as
//! many units as its rate allows each second, so the counters it reports have
//! something in them. With `--state` it writes what it holds as JSON after
//! every change, which is how the `testing/external` topology sees the
//! enforcer's side.
//!
//! It is also the reference for an enforcer written in Rust: all it needs of
//! TollGate is this crate — `tollgate_protocol::enforcer` and the frame reader
//! — and the standard library.
//!
//! ```text
//! cargo run -p tollgate-protocol --example stub_enforcer -- \
//!     --socket /run/tollgate-ip/enforcer.sock --identity address --unit byte --carry
//! ```

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tollgate_protocol::enforcer::{
    self, Binding, Conflict, Counters, EnforcerMessage, Hello, Identity, PROTOCOL_VERSION, Subject,
};
use tollgate_protocol::{FrameReader, PubKey};

type Result<T> = std::result::Result<T, String>;

struct Args {
    socket: PathBuf,
    hello: Hello,
    carry: bool,
    unshaped: u64,
    state: Option<PathBuf>,
}

const USAGE: &str = "usage: stub_enforcer --socket PATH --identity pubkey|address \
[--unit UNIT] [--delegated] [--version N] [--carry] [--unshaped UNITS_PER_S] [--state PATH]";

fn parse_args() -> Result<Args> {
    let mut socket = None;
    let mut identity = None;
    let mut unit = String::from("byte");
    let mut delegated = false;
    let mut version = PROTOCOL_VERSION;
    let mut carry = false;
    let mut unshaped = 1_000_000;
    let mut state = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            // Built for one identity: it is only stated, as a check.
            "--identity" => {
                let name = value()?;
                identity = Some(Identity::from_name(&name).ok_or(format!("no identity {name:?}"))?);
            }
            "--unit" => unit = value()?,
            "--delegated" => delegated = true,
            "--version" => version = value()?.parse().map_err(|e| format!("--version: {e}"))?,
            "--carry" => carry = true,
            "--unshaped" => {
                unshaped = value()?.parse().map_err(|e| format!("--unshaped: {e}"))?;
            }
            "--state" => state = Some(PathBuf::from(value()?)),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(Args {
        socket: socket.ok_or(USAGE)?,
        hello: Hello {
            version,
            identity: identity.ok_or(USAGE)?,
            delegated,
            unit,
        },
        carry,
        unshaped,
        state,
    })
}

/// One payer, as the enforcer holds it.
#[derive(Debug, Default)]
struct Payer {
    subjects: Vec<Binding>,
    /// `None` until a `set` arrives: closed. Inside, `None` is unshaped.
    rate: Option<Option<u64>>,
    delivered: u64,
    received: u64,
    /// The counts changed since they were last reported.
    dirty: bool,
}

#[derive(Debug, Default)]
struct Enforcer {
    /// Which connection is current; an older one stops when it sees this move.
    generation: u64,
    connected: bool,
    connections: u64,
    payers: HashMap<PubKey, Payer>,
    conflicts: Vec<(PubKey, Subject)>,
}

impl Enforcer {
    /// Back to where an enforcer starts: every subject closed.
    fn reset(&mut self) {
        self.connected = false;
        self.payers.clear();
        self.conflicts.clear();
    }

    fn holder(&self, subject: &Subject) -> Option<PubKey> {
        self.payers
            .iter()
            .find(|(_, p)| p.subjects.iter().any(|b| &b.subject == subject))
            .map(|(k, _)| *k)
    }

    /// What the enforcer holds, as JSON. Written by hand: this crate is `no_std`
    /// and carries no JSON library, and the shape is small.
    fn json(&self) -> String {
        let hex = |k: &PubKey| k.0.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let payers: Vec<String> = self
            .payers
            .iter()
            .map(|(key, p)| {
                let rate = match p.rate {
                    None | Some(Some(0)) => "0".to_string(),
                    Some(Some(n)) => n.to_string(),
                    Some(None) => "\"unshaped\"".to_string(),
                };
                let subjects: Vec<String> = p
                    .subjects
                    .iter()
                    .map(|b| {
                        format!(
                            "{{\"subject\":\"{}\",\"delegated\":{}}}",
                            b.subject, b.delegated
                        )
                    })
                    .collect();
                format!(
                    "\"{}\":{{\"subjects\":[{}],\"rate\":{rate},\"delivered\":{},\"received\":{}}}",
                    hex(key),
                    subjects.join(","),
                    p.delivered,
                    p.received
                )
            })
            .collect();
        let conflicts: Vec<String> = self
            .conflicts
            .iter()
            .map(|(k, s)| format!("{{\"payer\":\"{}\",\"subject\":\"{s}\"}}", hex(k)))
            .collect();
        format!(
            "{{\"connected\":{},\"connections\":{},\"payers\":{{{}}},\"conflicts\":[{}]}}",
            self.connected,
            self.connections,
            payers.join(","),
            conflicts.join(",")
        )
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("stub_enforcer: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Arc::new(parse_args()?);
    let _ = std::fs::remove_file(&args.socket);
    let listener = UnixListener::bind(&args.socket)
        .map_err(|e| format!("listen on {}: {e}", args.socket.display()))?;
    // Reaching this socket is the power to open what the enforcer controls.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("restrict {}: {e}", args.socket.display()))?;
    }
    eprintln!(
        "stub_enforcer: listening on {}, {:?}",
        args.socket.display(),
        args.hello
    );

    let state = Arc::new(Mutex::new(Enforcer::default()));
    write_state(&args, &state);

    let mut current: Option<UnixStream> = None;
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| format!("accept: {e}"))?;
        // One connection at a time: a second replaces the first, and the
        // enforcer resets to closed as if the first had dropped.
        if let Some(old) = current.take() {
            eprintln!("stub_enforcer: a new connection replaces the old one");
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        current = stream.try_clone().ok();
        let generation = {
            let mut g = state.lock().expect("not poisoned");
            g.reset();
            g.generation += 1;
            g.connected = true;
            g.connections += 1;
            g.generation
        };
        write_state(&args, &state);

        let (args, state) = (Arc::clone(&args), Arc::clone(&state));
        std::thread::spawn(move || {
            if let Err(e) = serve(stream, &args, &state, generation) {
                eprintln!("stub_enforcer: closing the connection: {e}");
            }
            // Losing the connection closes everything too.
            let mut g = state.lock().expect("not poisoned");
            if g.generation == generation {
                g.reset();
                drop(g);
                write_state(&args, &state);
            }
        });
    }
    Ok(())
}

fn serve(
    mut stream: UnixStream,
    args: &Args,
    state: &Mutex<Enforcer>,
    generation: u64,
) -> Result<()> {
    send(&mut stream, &[EnforcerMessage::Hello(args.hello.clone())])?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 8192];
    let mut last_tick = Instant::now();
    loop {
        if state.lock().expect("not poisoned").generation != generation {
            return Err("replaced by a newer connection".into());
        }
        match stream.read(&mut buf) {
            Ok(0) => return Err("tollgated closed the connection".into()),
            Ok(n) => {
                reader.push(&buf[..n]);
                let mut replies = Vec::new();
                while let Some(msg) = reader.next_enforcer_message() {
                    let msg = msg.map_err(|e| format!("malformed message: {e}"))?;
                    eprintln!("stub_enforcer: <- {msg:?}");
                    replies.extend(handle(msg, &args.hello, state)?);
                }
                write_state(args, state);
                send(&mut stream, &replies)?;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            let reports = report(args, state);
            if !reports.is_empty() {
                write_state(args, state);
                send(&mut stream, &reports)?;
            }
        }
    }
}

fn send(stream: &mut UnixStream, msgs: &[EnforcerMessage]) -> Result<()> {
    let mut out = Vec::new();
    for msg in msgs {
        enforcer::encode_frame(msg, &mut out).map_err(|e| e.to_string())?;
    }
    stream.write_all(&out).map_err(|e| e.to_string())
}

/// Apply one message from `tollgated`, returning anything to say back.
///
/// A subject of the wrong length for the identity, a delegated one when this
/// enforcer refuses them, or a message only an enforcer sends is a protocol
/// error, and the caller closes the connection over it.
fn handle(
    msg: EnforcerMessage,
    hello: &Hello,
    state: &Mutex<Enforcer>,
) -> Result<Vec<EnforcerMessage>> {
    let mut g = state.lock().expect("not poisoned");
    let mut replies = Vec::new();
    match msg {
        EnforcerMessage::Bind(bind) => {
            for b in &bind.bindings {
                hello
                    .check(b)
                    .map_err(|e| format!("a binding this enforcer cannot take: {e}"))?;
            }
            let mut accepted = Vec::new();
            for binding in bind.bindings {
                // Never last-wins: the first payer keeps the subject.
                match g.holder(&binding.subject) {
                    Some(holder) if holder != bind.peer => {
                        eprintln!("stub_enforcer: conflict over {}", binding.subject);
                        g.conflicts.push((bind.peer, binding.subject.clone()));
                        replies.push(EnforcerMessage::Conflict(Conflict {
                            peer: bind.peer,
                            subject: binding.subject,
                        }));
                    }
                    _ => accepted.push(binding),
                }
            }
            g.payers.entry(bind.peer).or_default().subjects = accepted;
        }
        EnforcerMessage::Set(set) => g.payers.entry(set.peer).or_default().rate = Some(set.rate),
        EnforcerMessage::Remove(remove) => {
            g.payers.remove(&remove.peer);
        }
        other => {
            return Err(format!(
                "tollgated sent {:?}, which only an enforcer sends",
                other.msg_type()
            ));
        }
    }
    Ok(replies)
}

/// Pretend a second passed, and report whatever changed.
fn report(args: &Args, state: &Mutex<Enforcer>) -> Vec<EnforcerMessage> {
    let mut g = state.lock().expect("not poisoned");
    let mut reports = Vec::new();
    for (key, payer) in g.payers.iter_mut() {
        if args.carry && !payer.subjects.is_empty() {
            let moved = match payer.rate {
                None | Some(Some(0)) => 0,
                Some(Some(n)) => n,
                Some(None) => args.unshaped,
            };
            if moved > 0 {
                payer.delivered = payer.delivered.saturating_add(moved);
                payer.received = payer.received.saturating_add(moved / 20);
                payer.dirty = true;
            }
        }
        if payer.dirty {
            payer.dirty = false;
            reports.push(EnforcerMessage::Counters(Counters {
                peer: *key,
                delivered: payer.delivered,
                received: payer.received,
            }));
        }
    }
    reports
}

fn write_state(args: &Args, state: &Mutex<Enforcer>) {
    let Some(path) = &args.state else {
        return;
    };
    let json = state.lock().expect("not poisoned").json();
    // Written aside and renamed, so a reader never sees half of it.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
