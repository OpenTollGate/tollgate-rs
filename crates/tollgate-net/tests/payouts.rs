//! Taking money out of a node's wallet: as a token, and paid over Lightning.
//!
//! Runs a real sat mint, with cdk's fake Lightning backend, over real HTTP,
//! and the node's wallet against it, so what it shows is what an operator
//! with sats at an ordinary mint gets.

use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cdk::mint::{MintBuilder, MintMeltLimits};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk_common::common::FeeReserve;
use tollgate_net::wallet::{Destination, Size, Wallet};

/// A sat mint whose Lightning is fake: invoices it issues are paid by
/// themselves, and invoices it pays succeed.
async fn sat_mint() -> String {
    let db = Arc::new(cdk_sqlite::mint::memory::empty().await.expect("db"));
    let mut builder = MintBuilder::new(db.clone());
    let lightning = cdk_fake_wallet::FakeWallet::new(
        FeeReserve {
            min_fee_reserve: 1.into(),
            percent_fee_reserve: 0.01,
        },
        HashMap::default(),
        HashSet::default(),
        0,
        CurrencyUnit::Sat,
    );
    builder
        .add_payment_processor(
            CurrencyUnit::Sat,
            PaymentMethod::Known(KnownMethod::Bolt11),
            MintMeltLimits::new(1, 1_000_000),
            Arc::new(lightning),
        )
        .await
        .expect("bolt11");

    let probe = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let addr = probe.local_addr().expect("its address");
    drop(probe);
    let url = format!("http://{addr}");

    let mint = Arc::new(
        builder
            .with_urls(vec![url.clone()])
            .build_with_seed(db, &[9u8; 64])
            .await
            .expect("mint"),
    );
    mint.start().await.expect("start");
    let router = cdk_axum::create_mint_router(Arc::clone(&mint), vec!["bolt11".into()])
        .await
        .expect("router");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    tokio::spawn(async move {
        let _keep = mint;
        axum::serve(listener, router).await.expect("serve");
    });
    url
}

/// A directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tollgate-payouts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
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

/// `amount` sats bought at `mint` and held: a top-up, collected once the
/// fake Lightning has paid it.
async fn funded(wallet: &Wallet, mint: &str, amount: u64) {
    let top_up = wallet.top_up(mint, "sat", amount).await.expect("top up");
    for _ in 0..50 {
        if wallet.collect(&top_up).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the top-up was never paid");
}

#[tokio::test(flavor = "multi_thread")]
async fn money_held_at_a_mint_is_listed_after_a_restart() {
    let mint = sat_mint().await;
    let dir = TempDir::new();
    let path = dir.path().join("wallet.sqlite");
    {
        let wallet = Wallet::open(&path, [1; 64], "byte").await.expect("open");
        funded(&wallet, &mint, 2_000).await;
    }

    // A new process: nothing has touched the mint yet.
    let wallet = Wallet::open(&path, [1; 64], "byte").await.expect("reopen");
    let held = wallet.balances().await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].mint, mint);
    assert_eq!(held[0].amount, 2_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn money_comes_out_as_a_token_another_wallet_redeems() {
    let mint = sat_mint().await;
    let dir = TempDir::new();
    let node = Wallet::open(dir.path().join("node.sqlite"), [2; 64], "byte")
        .await
        .expect("open");
    funded(&node, &mint, 3_000).await;

    let part = node
        .export(&mint, "sat", Some(1_000))
        .await
        .expect("export");
    assert_eq!(part.worth, 1_000);
    assert_eq!(node.balance_of(&mint, "sat").await, 2_000 - part.fee);

    let elsewhere = Wallet::open(dir.path().join("elsewhere.sqlite"), [3; 64], "byte")
        .await
        .expect("open");
    let got = elsewhere.deposit(&part.token).await.expect("redeem");
    assert_eq!(got.amount, 1_000);

    let rest = node
        .export(&mint, "sat", None)
        .await
        .expect("export the rest");
    assert_eq!(node.balance_of(&mint, "sat").await, 0);
    assert!(rest.worth > 0);
    assert!(
        node.export(&mint, "sat", None).await.is_err(),
        "nothing left to export"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn money_is_paid_out_to_an_invoice() {
    let mint = sat_mint().await;
    let dir = TempDir::new();
    let node = Wallet::open(dir.path().join("node.sqlite"), [4; 64], "byte")
        .await
        .expect("open");
    funded(&node, &mint, 5_000).await;

    // Somebody else's invoice, for 1 000 sats: the fake Lightning pays it.
    let invoice = cdk_fake_wallet::create_fake_invoice(1_000_000, "a payee".into());
    let to: Destination = invoice.to_string().parse().expect("an invoice");

    assert!(
        node.pay_out(&mint, "sat", &to, Size::Exactly(999))
            .await
            .is_err(),
        "an invoice says its own amount"
    );
    let payout = node
        .pay_out(&mint, "sat", &to, Size::Exactly(1_000))
        .await
        .expect("pay");
    assert_eq!(payout.paid, 1_000);
    let left = node.balance_of(&mint, "sat").await;
    assert_eq!(
        left,
        5_000 - 1_000 - payout.fee,
        "only the amount and the fee left"
    );

    // And everything that is left, within its fees, to a second invoice
    // bigger than it: refused, the balance untouched.
    let too_big = cdk_fake_wallet::create_fake_invoice(10_000_000, "a payee".into());
    let to: Destination = too_big.to_string().parse().expect("an invoice");
    assert!(
        node.pay_out(&mint, "sat", &to, Size::Within(u64::MAX))
            .await
            .is_err()
    );
    assert_eq!(node.balance_of(&mint, "sat").await, left);
}
