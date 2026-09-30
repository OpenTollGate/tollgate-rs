//! A stub gate: the smallest program that speaks the gate protocol.
//!
//! It owns no data plane. It listens on a Unix socket, says `hello`, keeps the
//! bindings and rates `tollgated` sends, refuses a subject another payer
//! already holds, and — with `--carry` — pretends every open payer moved as
//! much as its rate allows each second, so the counters it reports have
//! something in them. With `--state` it writes what it holds as JSON after
//! every change, which is how the `testing/external` topology sees the gate's
//! side.
//!
//! It is also the reference for a gate written in Rust: all it needs of
//! TollGate is this crate — `tollgate_protocol::gate` and the frame reader —
//! and the standard library.
//!
//! ```text
//! cargo run -p tollgate-protocol --example stub_gate -- \
//!     --socket /tmp/gate.sock --kinds ipv4,ipv6 --identify claimed --carry
//! ```

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tollgate_protocol::gate::{
    self, Binding, Conflict, Counters, GATE_PROTOCOL_VERSION, GateMessage, Hello, Subject,
    SubjectKind,
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

const USAGE: &str = "usage: stub_gate --socket PATH [--kinds ipv4,ipv6,mac,pubkey,opaque] \
[--identify claimed|fips] [--delegated] [--version N] [--carry] [--unshaped BYTES_PER_S] \
[--state PATH]";

fn parse_args() -> Result<Args> {
    let mut socket = None;
    let mut kinds = vec![SubjectKind::Ipv4, SubjectKind::Ipv6];
    let mut identify = gate::Identify::Claimed;
    let mut delegated = false;
    let mut version = GATE_PROTOCOL_VERSION;
    let mut carry = false;
    let mut unshaped = 1_000_000;
    let mut state = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--kinds" => {
                kinds = value()?
                    .split(',')
                    .map(|k| match k {
                        "ipv4" => Ok(SubjectKind::Ipv4),
                        "ipv6" => Ok(SubjectKind::Ipv6),
                        "mac" => Ok(SubjectKind::Mac),
                        "pubkey" => Ok(SubjectKind::Pubkey),
                        "opaque" => Ok(SubjectKind::Opaque),
                        other => Err(format!("no subject kind {other:?}")),
                    })
                    .collect::<Result<_>>()?;
            }
            "--identify" => {
                identify = match value()?.as_str() {
                    "claimed" => gate::Identify::Claimed,
                    "fips" => gate::Identify::Fips,
                    other => return Err(format!("no identify mode {other:?}")),
                }
            }
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
            kinds,
            identify,
            delegated,
            opaque_kinds: vec![],
        },
        carry,
        unshaped,
        state,
    })
}

/// One payer, as the gate holds it.
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
struct Gate {
    /// Which connection is current; an older one stops when it sees this move.
    generation: u64,
    connected: bool,
    connections: u64,
    payers: HashMap<PubKey, Payer>,
    conflicts: Vec<(PubKey, Subject)>,
}

impl Gate {
    /// Back to where a gate starts: every subject closed.
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

    /// What the gate holds, as JSON. Written by hand: this crate is `no_std`
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
        eprintln!("stub_gate: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Arc::new(parse_args()?);
    let _ = std::fs::remove_file(&args.socket);
    let listener = UnixListener::bind(&args.socket)
        .map_err(|e| format!("listen on {}: {e}", args.socket.display()))?;
    // Reaching this socket is the power to open the gate.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("restrict {}: {e}", args.socket.display()))?;
    }
    eprintln!(
        "stub_gate: listening on {}, {:?}",
        args.socket.display(),
        args.hello
    );

    let gate = Arc::new(Mutex::new(Gate::default()));
    write_state(&args, &gate);

    let mut current: Option<UnixStream> = None;
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| format!("accept: {e}"))?;
        // One connection at a time: a second replaces the first, and the gate
        // resets to closed as if the first had dropped.
        if let Some(old) = current.take() {
            eprintln!("stub_gate: a new connection replaces the old one");
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        current = stream.try_clone().ok();
        let generation = {
            let mut g = gate.lock().expect("not poisoned");
            g.reset();
            g.generation += 1;
            g.connected = true;
            g.connections += 1;
            g.generation
        };
        write_state(&args, &gate);

        let (args, gate) = (Arc::clone(&args), Arc::clone(&gate));
        std::thread::spawn(move || {
            if let Err(e) = serve(stream, &args, &gate, generation) {
                eprintln!("stub_gate: closing the connection: {e}");
            }
            // Losing the connection closes the gate too.
            let mut g = gate.lock().expect("not poisoned");
            if g.generation == generation {
                g.reset();
                drop(g);
                write_state(&args, &gate);
            }
        });
    }
    Ok(())
}

fn serve(mut stream: UnixStream, args: &Args, gate: &Mutex<Gate>, generation: u64) -> Result<()> {
    send(&mut stream, &[GateMessage::Hello(args.hello.clone())])?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 8192];
    let mut last_tick = Instant::now();
    loop {
        if gate.lock().expect("not poisoned").generation != generation {
            return Err("replaced by a newer connection".into());
        }
        match stream.read(&mut buf) {
            Ok(0) => return Err("tollgated closed the connection".into()),
            Ok(n) => {
                reader.push(&buf[..n]);
                let mut replies = Vec::new();
                while let Some(msg) = reader.next_gate_message() {
                    let msg = msg.map_err(|e| format!("malformed message: {e}"))?;
                    eprintln!("stub_gate: <- {msg:?}");
                    replies.extend(handle(msg, &args.hello, gate)?);
                }
                write_state(args, gate);
                send(&mut stream, &replies)?;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            let reports = report(args, gate);
            if !reports.is_empty() {
                write_state(args, gate);
                send(&mut stream, &reports)?;
            }
        }
    }
}

fn send(stream: &mut UnixStream, msgs: &[GateMessage]) -> Result<()> {
    let mut out = Vec::new();
    for msg in msgs {
        gate::encode_frame(msg, &mut out).map_err(|e| e.to_string())?;
    }
    stream.write_all(&out).map_err(|e| e.to_string())
}

/// Apply one message from `tollgated`, returning anything to say back.
///
/// Anything this gate did not ask for is a protocol error, and the caller
/// closes the connection over it.
fn handle(msg: GateMessage, hello: &Hello, gate: &Mutex<Gate>) -> Result<Vec<GateMessage>> {
    let mut g = gate.lock().expect("not poisoned");
    let mut replies = Vec::new();
    match msg {
        GateMessage::Bind(bind) => {
            if let Some(b) = bind.bindings.iter().find(|b| !hello.accepts(b)) {
                return Err(format!("a binding this gate did not ask for: {b:?}"));
            }
            let mut accepted = Vec::new();
            for binding in bind.bindings {
                // Never last-wins: the first payer keeps the subject.
                match g.holder(&binding.subject) {
                    Some(holder) if holder != bind.peer => {
                        eprintln!("stub_gate: conflict over {}", binding.subject);
                        g.conflicts.push((bind.peer, binding.subject.clone()));
                        replies.push(GateMessage::Conflict(Conflict {
                            peer: bind.peer,
                            subject: binding.subject,
                        }));
                    }
                    _ => accepted.push(binding),
                }
            }
            g.payers.entry(bind.peer).or_default().subjects = accepted;
        }
        GateMessage::Set(set) => g.payers.entry(set.peer).or_default().rate = Some(set.rate),
        GateMessage::Remove(remove) => {
            g.payers.remove(&remove.peer);
        }
        other => {
            return Err(format!(
                "tollgated sent {:?}, which only a gate sends",
                other.msg_type()
            ));
        }
    }
    Ok(replies)
}

/// Pretend a second passed, and report whatever changed.
fn report(args: &Args, gate: &Mutex<Gate>) -> Vec<GateMessage> {
    let mut g = gate.lock().expect("not poisoned");
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
            reports.push(GateMessage::Counters(Counters {
                peer: *key,
                delivered: payer.delivered,
                received: payer.received,
            }));
        }
    }
    reports
}

fn write_state(args: &Args, gate: &Mutex<Gate>) {
    let Some(path) = &args.state else {
        return;
    };
    let json = gate.lock().expect("not poisoned").json();
    // Written aside and renamed, so a reader never sees half of it.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
