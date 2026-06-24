//! FIPS-style Unix control socket: serves the node status snapshot on request.
//!
//! Line protocol: read a command line, write one JSON line back. Only `status`
//! is implemented today (returns a [`NodeStatus`](tollgate_net::status::NodeStatus));
//! unknown commands get a JSON error. The client half lives in the shared lib
//! ([`tollgate_net::control`]).

use std::path::Path;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::driver::Driver;

/// Bind `path` and serve status to every connection until the process exits.
/// Removes any stale socket file first. Errors are returned for the caller to log.
pub async fn serve(path: &Path, driver: Driver) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding control socket {}", path.display()))?;
    tracing::info!(socket = %path.display(), "control socket listening");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let driver = driver.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, driver).await {
                        tracing::debug!(err = %e, "control connection ended");
                    }
                });
            }
            Err(e) => tracing::warn!(err = %e, "control socket accept failed"),
        }
    }
}

async fn handle(stream: UnixStream, driver: Driver) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        match line.trim() {
            "status" => {
                let status = driver.status().await;
                let json = serde_json::to_string(&status)?;
                write.write_all(json.as_bytes()).await?;
                write.write_all(b"\n").await?;
            }
            "" => {}
            other => {
                let err = serde_json::json!({ "error": format!("unknown command: {other}") });
                write.write_all(err.to_string().as_bytes()).await?;
                write.write_all(b"\n").await?;
            }
        }
    }
    Ok(())
}
