//! A buyer acquiring a node's vouchers by minting them at the node's mint.
//!
//! Until there is a market, this is the only way a peer comes to hold anything
//! it can fund a channel with: a NUT-04 quote at the seller's mint, paid the
//! moment it exists, minted at once. These run a real mint over real HTTP and
//! a real cdk wallet against it, so what they show is that an ordinary Cashu
//! client can do it, not only this crate's.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tollgate_net::Identity;
use tollgate_net::channel::{
    ChannelBackend, FundedChannel, SpilmanChannels, SpilmanConfig, VerifiedChannel,
};
use tollgate_net::mint::{self, IssueLimit, MintConfig};
use tollgate_net::wallet::Wallet;

const UNIT: &str = "byte";

/// A seller's mint, serving on a port of its own.
struct Seller {
    mint: Arc<cdk::mint::Mint>,
    url: String,
}

/// Bring a mint up and serve it, returning once it answers.
async fn seller(auto_accept: bool, max_amount: u64) -> Seller {
    serve_seller(auto_accept, max_amount, IssueLimit::default()).await
}

/// An auto-accepting seller issuing under `limit`.
async fn seller_limited(max_amount: u64, limit: IssueLimit) -> Seller {
    serve_seller(true, max_amount, limit).await
}

async fn serve_seller(auto_accept: bool, max_amount: u64, issue_limit: IssueLimit) -> Seller {
    // Bound and released to learn a free port. Something else could take it in
    // between, but on a test machine that is not worth more machinery.
    let listen: SocketAddr = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("find a free port");
        probe.local_addr().expect("its address")
    };
    let url = format!("http://{listen}");

    let mint = Arc::new(
        mint::build(&MintConfig {
            url: url.clone(),
            unit: UNIT.into(),
            seed: vec![9; 32],
            max_amount,
            auto_accept,
            issue_limit,
        })
        .await
        .expect("build the mint"),
    );

    {
        let mint = Arc::clone(&mint);
        tokio::spawn(async move {
            let _ = mint::serve(mint, axum::Router::new(), listen, std::future::pending()).await;
        });
    }

    // Serving is spawned, so wait until it is actually listening rather than
    // racing the first request against the bind.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(listen).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Seller { mint, url }
}

/// A directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tollgate-minting-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn buyer_wallet(dir: &TempDir, seed: u8) -> Wallet {
    Wallet::open(dir.path().join("wallet.sqlite"), [seed; 64], UNIT)
        .await
        .expect("open the buyer's wallet")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_buyer_mints_capacity_at_a_mint_that_auto_accepts() {
    let seller = seller(true, 1 << 30).await;
    let dir = TempDir::new();
    let wallet = buyer_wallet(&dir, 1).await;

    let minted = wallet
        .issue(&seller.url, UNIT, 1_000_000)
        .await
        .expect("an auto-accepting mint issues for the asking");
    assert_eq!(minted, 1_000_000);
    assert_eq!(wallet.balance_of(&seller.url, UNIT).await, 1_000_000);

    // And the paper is good: spending it swaps at the issuer, which checks
    // every signature.
    wallet
        .spend(&seller.url, UNIT, 300_000)
        .await
        .expect("spend what was minted");
    assert_eq!(wallet.balance_of(&seller.url, UNIT).await, 700_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn more_than_one_quote_allows_is_asked_for_in_several() {
    // A buyer's channel is sized by the buyer, and the seller caps one quote by
    // its own setting, so the two need not agree.
    let seller = seller(true, 4096).await;
    let dir = TempDir::new();
    let wallet = buyer_wallet(&dir, 2).await;

    let minted = wallet
        .issue(&seller.url, UNIT, 10_000)
        .await
        .expect("mint across several quotes");
    assert_eq!(minted, 10_000);
    assert_eq!(wallet.balance_of(&seller.url, UNIT).await, 10_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mint_that_does_not_auto_accept_issues_nothing() {
    let seller = seller(false, 1 << 30).await;
    let dir = TempDir::new();
    let wallet = buyer_wallet(&dir, 3).await;

    assert!(
        wallet.issue(&seller.url, UNIT, 1_000_000).await.is_err(),
        "a mint with auto-accept off serves no mint quotes"
    );
    assert_eq!(wallet.balance_of(&seller.url, UNIT).await, 0);

    // Not only refused by our wallet for want of an advertised method: the
    // endpoint itself is not there to be asked.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/mint/quote/bolt11", seller.url))
        .json(&serde_json::json!({ "amount": 1000, "unit": UNIT }))
        .send()
        .await
        .expect("reach the mint");
    assert!(
        !response.status().is_success(),
        "a quote was served: {}",
        response.status()
    );
}

/// Fund a channel of `capacity` to `seller` out of `held`, and have the seller
/// verify it.
async fn fund_and_verify(
    seller: &Seller,
    held: Wallet,
    capacity: u64,
) -> (FundedChannel, VerifiedChannel) {
    let (buyer_id, seller_id) = (Identity::generate(), Identity::generate());
    let seller_dir = TempDir::new();

    // The buyer's own mint only matters for channels paid *to* it; here it is
    // simply somewhere to point the config.
    let buyer_mint = mint::build(&MintConfig {
        url: "http://127.0.0.1:1".into(),
        unit: UNIT.into(),
        seed: vec![4; 32],
        max_amount: 1 << 30,
        auto_accept: false,
        issue_limit: IssueLimit::default(),
    })
    .await
    .expect("build the buyer's mint");

    let buyer = SpilmanChannels::new(SpilmanConfig {
        mint: Arc::new(buyer_mint),
        mint_url: "http://127.0.0.1:1".into(),
        unit: UNIT.into(),
        accepted_mints: vec!["http://127.0.0.1:1".into()],
        secret_key_hex: buyer_id.secret_hex(),
        wallet: held,
    })
    .expect("the buyer's channels");

    let sellers = SpilmanChannels::new(SpilmanConfig {
        mint: Arc::clone(&seller.mint),
        mint_url: seller.url.clone(),
        unit: UNIT.into(),
        accepted_mints: vec![seller.url.clone()],
        secret_key_hex: seller_id.secret_hex(),
        wallet: buyer_wallet(&seller_dir, 5).await,
    })
    .expect("the seller's channels");

    // The node drives the backend from a blocking thread, and so does this.
    let (url, peer, buyer_key) = (seller.url.clone(), seller_id.pubkey(), buyer_id.pubkey());
    tokio::task::spawn_blocking(move || {
        let funded = buyer.fund(peer, &url, capacity).expect("fund a channel");
        let verified = sellers
            .verify(buyer_key, &funded.funding)
            .expect("the seller accepts funding minted at its own mint");
        (funded, verified)
    })
    .await
    .expect("the funding thread")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_buyer_funds_a_channel_the_seller_verifies_from_minted_vouchers() {
    let seller = seller(true, 1 << 30).await;
    let buyer_dir = TempDir::new();

    // Vouchers left over from an earlier purchase that minted and then failed:
    // funding spends them first and mints only the rest.
    let held = buyer_wallet(&buyer_dir, 4).await;
    held.issue(&seller.url, UNIT, 500_000)
        .await
        .expect("mint the leftovers");

    let (funded, verified) = fund_and_verify(&seller, held.clone(), 2_000_000).await;

    assert_eq!(funded.capacity, 2_000_000);
    assert_eq!(verified.channel_id, funded.channel_id);
    assert_eq!(verified.capacity, funded.capacity);
    assert_eq!(
        verified.mint_url, seller.url,
        "funded in the seller's own mint"
    );
    assert_eq!(
        held.balance_of(&seller.url, UNIT).await,
        0,
        "the leftovers went into the channel and nothing more was minted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn funding_is_served_and_a_flood_past_the_issue_limit_is_refused() {
    // A burst of one channel and a rate so slow that nothing refills while
    // the test runs: the channel is funded, and what comes after it is not.
    let seller = seller_limited(
        2_000_000,
        IssueLimit {
            bytes_per_sec: 1,
            burst_bytes: 2_000_000,
            quotes_per_minute: 0,
        },
    )
    .await;

    let buyer_dir = TempDir::new();
    let (funded, verified) =
        fund_and_verify(&seller, buyer_wallet(&buyer_dir, 6).await, 2_000_000).await;
    assert_eq!(funded.capacity, 2_000_000);
    assert_eq!(verified.capacity, 2_000_000);

    // Somebody else asking for more, straight after, is turned away before
    // the mint signs or stores anything.
    let flood_dir = TempDir::new();
    let flooder = buyer_wallet(&flood_dir, 7).await;
    let refused = flooder
        .issue(&seller.url, UNIT, 1_000)
        .await
        .expect_err("the issue limit is spent");
    // cdk hands a wallet any refusal from the payment processor as a generic
    // "Invalid payment request", so what shows here is that the quote itself
    // was refused, rather than the mint after it.
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("ask ") && refused.contains("Invalid payment request"),
        "refused when the quote was asked for: {refused}"
    );
    assert_eq!(flooder.balance_of(&seller.url, UNIT).await, 0);
}
