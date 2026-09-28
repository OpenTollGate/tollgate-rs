//! The mint daemon.
//!
//! Issues this node's vouchers and redeems them, and does nothing else: the
//! standard Cashu API on a public listener for peers and wallets, and on a
//! private one where mint quotes are paid on creation, for `merchantd`. Reads
//! `mint.yaml`; see `docs/design/core/tollgate-daemons.md`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use tollgate_net::mint::{self, Stats};
use tollgate_net::mintd::{MintdFile, Snapshot};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "mintd", about = "A TollGate node's mint")]
struct Args {
    /// Configuration file. Defaults are used if absent.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Where to serve the control socket that `minttop` reads. Overrides the
    /// file.
    #[arg(long)]
    control_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let args = Args::parse();
    let file = match &args.config {
        Some(path) => MintdFile::load(path)?,
        None => serde_yaml::from_str("{}").expect("the empty document is valid"),
    };
    let listeners = file.listeners().context("resolve the listeners")?;

    let seed_path = file.seed_path();
    let seed = mint::load_or_create_seed(&seed_path)?;
    let mint = Arc::new(
        mint::build(&file.mint_config(seed))
            .await
            .context("bring up the mint")?,
    );
    let keyset = mint
        .get_active_keysets()
        .get(&mint::currency_unit(&file.unit))
        .map(|id| id.to_string())
        .unwrap_or_default();

    info!(
        url = %file.url,
        unit = %file.unit,
        %keyset,
        public = %listeners.public,
        private = %listeners.private,
        db = %file.db_path().display(),
        seed = %seed_path.display(),
        "mint serving"
    );
    if listeners.auto_accept {
        // Said once at startup because it is the most consequential setting in
        // the file: it makes service free to anyone who can reach the mint.
        tracing::warn!("the public listener gives vouchers to anyone who asks, so service is free");
    }

    let stats = Arc::new(Stats::default());
    {
        let snapshot = SnapshotSource {
            base: Snapshot {
                url: file.url.clone(),
                unit: file.unit.clone(),
                keyset,
                public: listeners.public.to_string(),
                private: listeners.private.to_string(),
                auto_accept: listeners.auto_accept,
                issue_quotes_per_minute: listeners.issue_limit.quotes_per_minute,
                ..Snapshot::default()
            },
            stats: Arc::clone(&stats),
            started: Instant::now(),
        };
        let path = args
            .control_socket
            .clone()
            .unwrap_or_else(|| file.control_path());
        tokio::spawn(async move {
            if let Err(e) = serve_control(&path, snapshot).await {
                tracing::warn!(error = %format!("{e:#}"), "the control socket stopped");
            }
        });
    }

    mint::serve(mint, listeners, stats, interrupted()).await
}

/// What the control socket publishes.
#[derive(Clone)]
struct SnapshotSource {
    base: Snapshot,
    stats: Arc<Stats>,
    started: Instant,
}

impl SnapshotSource {
    fn now(&self) -> Snapshot {
        Snapshot {
            public_quotes: self.stats.public_quotes.load(Ordering::Relaxed),
            private_quotes: self.stats.private_quotes.load(Ordering::Relaxed),
            refused_quotes: self.stats.refused_quotes.load(Ordering::Relaxed),
            uptime_secs: self.started.elapsed().as_secs(),
            ..self.base.clone()
        }
    }
}

/// Serve one JSON snapshot per connection on a Unix socket.
///
/// A socket rather than a port: it is operational state for a local tool.
#[cfg(unix)]
async fn serve_control(path: &std::path::Path, source: SnapshotSource) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("bind the control socket at {}", path.display()))?;
    info!(path = %path.display(), "control socket");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let body = serde_json::to_vec(&source.now())?;
        tokio::spawn(async move {
            let _ = stream.write_all(&body).await;
            let _ = stream.shutdown().await;
        });
    }
}

#[cfg(not(unix))]
async fn serve_control(_path: &std::path::Path, _source: SnapshotSource) -> Result<()> {
    Ok(())
}

/// Resolves on the first interrupt or termination signal.
async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
