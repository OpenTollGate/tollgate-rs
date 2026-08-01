//! This node's own mint.
//!
//! Every node issues vouchers against its own capacity and redeems them on
//! delivery. Redemption is a local spent-proof check — the node is the
//! authority on its own paper — which is why payment liveness and service
//! liveness fail together rather than separately.
//!
//! The keyset unit is the **byte**, not the sat. That is one of the three
//! things the voucher model changes, and it is what makes a proof a claim on
//! one unit of capacity rather than on money. A `1024` proof is a 1 KiB claim;
//! two of them make 2 KiB; they split and combine like any other Cashu token.
//!
//! # What is simulated
//!
//! The payment backend behind the mint is a fake wallet that auto-pays its own
//! quotes, so vouchers cost nothing to acquire. That is the *market* being
//! simulated, not the mint: the keysets, blind signatures, DLEQ proofs and
//! spent-proof set are all real. Selling vouchers for money is a separate
//! concern the protocol never sees.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use cdk::mint::{Mint, MintBuilder, MintMeltLimits};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk::types::FeeReserve;
use cdk_common::common::QuoteTTL;
use cdk_fake_wallet::FakeWallet;

/// How this node's mint is set up.
#[derive(Debug, Clone)]
pub struct MintConfig {
    /// URL peers reach it on, as advertised in our Offer.
    pub url: String,
    /// Quantity unit. `"byte"` for network forwarding.
    pub unit: String,
    /// Seed for the keyset, so a restart keeps issuing against the same keys.
    pub seed: Vec<u8>,
    /// Largest amount a single mint operation may create.
    pub max_amount: u64,
}

/// The unit a keyset denominates in.
///
/// The resource fixes the unit, and every node selling the same resource
/// denominates the same way — that is what makes one issuer's vouchers
/// comparable to another's. `"sat"` is still spelled the Cashu way rather than
/// as a custom unit, so a sat-denominated node stays interoperable with
/// ordinary wallets.
pub fn currency_unit(unit: &str) -> CurrencyUnit {
    match unit {
        "sat" => CurrencyUnit::Sat,
        "msat" => CurrencyUnit::Msat,
        other => CurrencyUnit::Custom(other.to_string()),
    }
}

/// Build and start this node's mint.
pub async fn build(config: &MintConfig) -> Result<Mint> {
    let unit = currency_unit(&config.unit);
    let db = Arc::new(
        cdk_sqlite::mint::memory::empty()
            .await
            .context("open the mint database")?,
    );

    let mut builder = MintBuilder::new(db.clone())
        .with_name(format!("TollGate node at {}", config.url))
        .with_description("Vouchers: claims on this node's capacity".to_string())
        .with_urls(vec![config.url.clone()]);

    // The backend that would take money for vouchers. Nothing about the
    // protocol depends on it: a node that sells its vouchers some other way,
    // or gives them away, runs the same mint.
    let backend = FakeWallet::new(
        FeeReserve {
            min_fee_reserve: 0.into(),
            percent_fee_reserve: 0.0,
        },
        HashMap::default(),
        HashSet::default(),
        0,
        unit.clone(),
    );

    builder
        .add_payment_processor(
            unit.clone(),
            PaymentMethod::Known(KnownMethod::Bolt11),
            MintMeltLimits::new(1, config.max_amount),
            Arc::new(backend),
        )
        .await
        .with_context(|| format!("add a payment processor for {unit}"))?;

    // No input fee. A fee would mean a voucher redeemed is worth slightly less
    // than a voucher issued, and delivery is one voucher per unit exactly.
    builder
        .set_unit_fee(&unit, 0)
        .map_err(|e| anyhow!("set the input fee: {e}"))?;

    let mint = builder
        .build_with_seed(db.clone(), &config.seed)
        .await
        .context("build the mint")?;

    mint.set_quote_ttl(QuoteTTL::new(10_000, 10_000))
        .await
        .context("set quote TTLs")?;

    if !mint.get_active_keysets().contains_key(&unit) {
        return Err(anyhow!(
            "the mint came up with no active keyset for {unit}; the unit is probably not supported"
        ));
    }

    mint.start().await.context("start the mint")?;
    Ok(mint)
}

/// Serve the mint over HTTP until `shutdown` resolves.
///
/// A peer funds its channel against **our** mint, so this has to be reachable
/// by peers — which it always is, because it is the node they are already
/// talking to.
pub async fn serve(
    mint: Arc<Mint>,
    listen: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // The market rides on the same listener as the mint under its own path.
    // It is a separate protocol that happens to be served by the same process;
    // `market.path` in the configuration schema may point somewhere else
    // entirely, including at a third party.
    let router = cdk_axum::create_mint_router(
        Arc::clone(&mint),
        vec![PaymentMethod::Known(KnownMethod::Bolt11).to_string()],
    )
    .await
    .context("build the mint router")?
    .merge(crate::market::router(Arc::clone(&mint)));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind the mint on {listen}"))?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serve the mint")?;

    mint.stop().await.context("stop the mint")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_unit_is_a_custom_keyset_unit() {
        // Cashu has no byte unit of its own; it is the resource that fixes it.
        assert_eq!(
            currency_unit("byte"),
            CurrencyUnit::Custom("byte".to_string())
        );
    }

    #[test]
    fn sat_stays_the_cashu_sat() {
        // Otherwise a sat-denominated node would be issuing something ordinary
        // wallets could not read.
        assert_eq!(currency_unit("sat"), CurrencyUnit::Sat);
    }

    #[tokio::test]
    async fn a_byte_denominated_mint_comes_up_with_an_active_keyset() {
        let mint = build(&MintConfig {
            url: "http://127.0.0.1:0".into(),
            unit: "byte".into(),
            seed: vec![7; 32],
            max_amount: 1_000_000_000,
        })
        .await
        .expect("build a byte mint");

        assert!(
            mint.get_active_keysets()
                .contains_key(&CurrencyUnit::Custom("byte".into())),
            "the byte keyset should be active"
        );
    }
}
