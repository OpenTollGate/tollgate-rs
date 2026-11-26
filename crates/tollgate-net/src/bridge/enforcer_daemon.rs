//! A minimal enforcer daemon, so the bridge's admission path can be exercised
//! against a real socket instead of only against `Loopback`.
//!
//! This is **not** a shipping enforcer. It admits, shapes and forgets exactly as
//! asked and carries no traffic — there is no kernel, no `tc`, no mesh. Its purpose
//! is to be the other end of the enforcer protocol so that
//! [`External::connect`](crate::enforcer::External) has something to talk to, and so
//! that a test can assert what a real connection carried.
//!
//! That distinction matters: a test against `Loopback` proves the bridge calls the
//! trait correctly. A test against this proves the calls survive encoding, framing
//! and a second process — which is the part `Loopback` cannot reach.

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tollgate_protocol::FrameReader;
use tollgate_protocol::enforcer::{
    Bind, EnforcerMessage, Hello, Identity, PROTOCOL_VERSION, Remove, Set, Subject, encode_frame,
};

/// What the daemon was told, so a test can assert on it.
#[derive(Debug, Default)]
pub struct Recorded {
    /// Every subject bound, in arrival order.
    pub bindings: Vec<Vec<u8>>,
    /// The current rate for each peer, by key bytes.
    pub rates: BTreeMap<Vec<u8>, u64>,
    /// Peers still tracked.
    pub tracked: Vec<Vec<u8>>,
}

/// A running enforcer daemon.
pub struct Daemon {
    /// What it has been told.
    pub recorded: Arc<Mutex<Recorded>>,
}

/// Serve the enforcer protocol on `path` until the process ends.
///
/// One connection at a time is enough here: `tollgated` holds one and reconnects.
/// A second connection replaces the first, which is what the protocol expects.
pub fn serve(path: &Path, unit: &str) -> Result<Daemon> {
    let unit = unit.to_owned();
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind the enforcer socket at {}", path.display()))?;
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let shared = Arc::clone(&recorded);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let shared = Arc::clone(&shared);
            let unit = unit.to_owned();
            std::thread::spawn(move || {
                if let Err(e) = handle(stream, &unit, &shared) {
                    eprintln!("enforcer daemon: {e:#}");
                }
            });
        }
    });

    Ok(Daemon { recorded })
}

fn handle(stream: UnixStream, unit: &str, recorded: &Arc<Mutex<Recorded>>) -> Result<()> {
    let mut writer = stream.try_clone().context("clone the connection")?;
    let mut source = BufReader::new(stream);
    let mut reader = FrameReader::new();
    let mut chunk = [0u8; 4096];

    // First message: our hello, so the node can check version, identity and unit.
    let hello = EnforcerMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        identity: Identity::Address,
        delegated: false,
        unit: unit.to_owned(),
    });
    write_message(&mut writer, &hello)?;

    loop {
        // Drain every complete message the buffer holds before reading more: one
        // read can carry several, and leaving them buffered would stall the loop.
        while let Some(decoded) = reader.next_enforcer_message() {
            let message = decoded.context("decode a message")?;
            record(message, recorded);
        }
        let read = {
            use std::io::Read;
            source.read(&mut chunk).context("read the connection")?
        };
        if read == 0 {
            return Ok(());
        }
        reader.push(&chunk[..read]);
    }
}

fn record(message: EnforcerMessage, recorded: &Arc<Mutex<Recorded>>) {
    match message {
        EnforcerMessage::Bind(Bind { peer, bindings }) => {
            let mut rec = recorded.lock().expect("not poisoned");
            for binding in bindings {
                rec.bindings.push(binding.subject.as_bytes().to_vec());
            }
            if !rec
                .tracked
                .iter()
                .any(|k| k.as_slice() == peer.0.as_slice())
            {
                rec.tracked.push(peer.0.to_vec());
            }
        }
        EnforcerMessage::Set(Set { peer, rate }) => {
            let mut rec = recorded.lock().expect("not poisoned");
            rec.rates.insert(peer.0.to_vec(), rate.unwrap_or(u64::MAX));
            if !rec
                .tracked
                .iter()
                .any(|k| k.as_slice() == peer.0.as_slice())
            {
                rec.tracked.push(peer.0.to_vec());
            }
        }
        EnforcerMessage::Remove(Remove { peer }) => {
            let mut rec = recorded.lock().expect("not poisoned");
            rec.tracked.retain(|k| k.as_slice() != peer.0.as_slice());
            rec.rates.remove(peer.0.as_slice());
        }
        EnforcerMessage::Counters(_) => {
            // Nothing is measured here, so counters stay absent rather than
            // invented: a zero reading would look like a real idle peer.
        }
        other => {
            eprintln!("enforcer daemon: ignoring {:?}", other.msg_type());
        }
    }
}

fn write_message(writer: &mut UnixStream, message: &EnforcerMessage) -> Result<()> {
    let mut out = Vec::new();
    encode_frame(message, &mut out).context("frame a message")?;
    writer.write_all(&out).context("write a message")?;
    writer.flush().context("flush")?;
    Ok(())
}

/// A subject built from raw bytes, so a test can compare what arrived with what
/// was sent without re-deriving the encoding.
pub fn subject_of(bytes: &[u8]) -> Subject {
    Subject::new(bytes.to_vec()).expect("a subject within the length bound")
}
