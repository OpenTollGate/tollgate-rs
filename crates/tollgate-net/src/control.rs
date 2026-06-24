//! Control-socket client: query a running node's status over its Unix socket.
//!
//! The protocol is line-oriented: write a command line (`status`), read one line
//! of JSON back. This mirrors FIPS's control socket — local-only, no TCP port.
//! The node side (the server) lives in the `tollgate` binary; this client half is
//! shared so `tolltop` (and any other tool) can read status the same way.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Context;

use crate::status::NodeStatus;

/// Connect to a node's control socket and fetch its current [`NodeStatus`].
/// Blocking — callers are short-lived tools, not the async node.
pub fn query(socket: &Path) -> anyhow::Result<NodeStatus> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to control socket {}", socket.display()))?;
    stream
        .write_all(b"status\n")
        .context("sending status request")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("reading status response")?;
    let line = line.trim();
    if line.is_empty() {
        anyhow::bail!("empty response from control socket");
    }
    serde_json::from_str(line).context("parsing status JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::NodeStatus;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn query_round_trips_over_a_unix_socket() {
        let path = std::env::temp_dir().join(format!("tolltop-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");

        // A minimal server: read the "status" request, reply with one JSON line.
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut req = String::new();
            reader.read_line(&mut req).expect("read req");
            assert_eq!(req.trim(), "status");

            let status = NodeStatus {
                pubkey: "02ab".to_string(),
                unit: "bytes".to_string(),
                peers: Vec::new(),
                pricing: Default::default(),
            };
            let mut w = stream;
            w.write_all(serde_json::to_string(&status).unwrap().as_bytes())
                .unwrap();
            w.write_all(b"\n").unwrap();
        });

        let status = query(&path).expect("query");
        assert_eq!(status.pubkey, "02ab");
        assert_eq!(status.unit, "bytes");
        assert!(status.peers.is_empty());

        handle.join().unwrap();
        std::fs::remove_file(&path).ok();
    }
}
