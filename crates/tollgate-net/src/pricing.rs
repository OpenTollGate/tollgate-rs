//! What this node's capacity sells for, turned into a price per token.
//!
//! An operator prices in one quantity — a Mbit — in the unit it thinks in:
//! `usd`, `eur` or `sat`. A buyer pays in tokens of some accepted mint, whose
//! unit is a sat or a cent. Between the two sits the BTC price in whichever
//! fiat currency is involved, fetched only when the price and the payment are
//! in different units (`docs/design/core/tollgate-configuration.md`).
//!
//! What comes out is the table the market has always used: for each accepted
//! mint and unit, how many bytes one unit of its tokens buys
//! ([`crate::market::Accepted`]). `merchantd` recomputes it whenever a rate or
//! a price changes, so nothing downstream knows there was a currency at all.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::market::Accepted;

/// Bytes in a megabit.
const BYTES_PER_MBIT: f64 = 125_000.0;

/// A price: so much of `unit` per Mbit of capacity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// `usd`, `eur` or `sat`.
    pub unit: String,
    /// What one Mbit costs, in `unit` — dollars or euros, not cents.
    pub per_mbit: f64,
}

/// One mint whose tokens buy this node's capacity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    /// The mint.
    pub mint: String,
    /// The unit of its tokens taken here: `sat`, `usd` or `eur`.
    pub unit: String,
    /// This issuer's own price. Without one, the default applies.
    #[serde(default)]
    pub price: Option<Price>,
}

/// What a BTC is worth, per fiat currency, as last fetched.
///
/// Keyed by lower-case currency code. A currency that has never been fetched
/// successfully is absent, and nothing priced through it is sold.
#[derive(Debug, Clone, Default)]
pub struct Rates(Arc<RwLock<BTreeMap<String, f64>>>);

impl Rates {
    /// The BTC price in `currency`, if one has been had.
    pub fn btc_in(&self, currency: &str) -> Option<f64> {
        self.0.read().expect("not poisoned").get(currency).copied()
    }

    /// Record a BTC price. A zero or negative one is no price and is ignored,
    /// so the last good one stays.
    pub fn set(&self, currency: &str, btc: f64) {
        if btc.is_finite() && btc > 0.0 {
            self.0
                .write()
                .expect("not poisoned")
                .insert(currency.to_owned(), btc);
        }
    }

    /// Every rate held.
    pub fn all(&self) -> BTreeMap<String, f64> {
        self.0.read().expect("not poisoned").clone()
    }
}

/// What one unit is worth in sats, given the rates. A fiat unit here is the
/// whole currency — a dollar, a euro.
fn sats_per(unit: &str, rates: &Rates) -> Option<f64> {
    match unit {
        "sat" => Some(1.0),
        fiat @ ("usd" | "eur") => rates.btc_in(fiat).map(|btc| 1e8 / btc),
        _ => None,
    }
}

/// What one unit of a *token* is, in the price's terms: a sat is a sat, and a
/// `usd` or `eur` token counts cents.
fn token_unit_value(unit: &str) -> Option<(&str, f64)> {
    match unit {
        "sat" => Some(("sat", 1.0)),
        "usd" => Some(("usd", 0.01)),
        "eur" => Some(("eur", 0.01)),
        _ => None,
    }
}

/// How many bytes one unit of a `token_unit` token buys at `price`.
///
/// Rounded down: a fraction of a byte is the buyer's to lose, never capacity
/// given away. `None` when the price is not positive, the unit unknown, or a
/// rate it needs has never been fetched.
pub fn bytes_per_unit(price: &Price, token_unit: &str, rates: &Rates) -> Option<u64> {
    if !(price.per_mbit.is_finite() && price.per_mbit > 0.0) {
        return None;
    }
    let (currency, amount) = token_unit_value(token_unit)?;
    // One token unit, and one Mbit's price, both in sats — or directly, with
    // no rate at all, when they are the same currency.
    let (token_value, mbit_price) = if currency == price.unit {
        (amount, price.per_mbit)
    } else {
        (
            amount * sats_per(currency, rates)?,
            price.per_mbit * sats_per(&price.unit, rates)?,
        )
    };
    // Decimal prices are not exact in binary: a thousandth of a cent comes out
    // a hair under, and flooring that would lose a whole byte. A tolerance far
    // below one byte in the largest table keeps the rounding honest.
    let bytes = (token_value / mbit_price * BYTES_PER_MBIT * (1.0 + 1e-12)).floor();
    (bytes >= 1.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// The price table the market sells by: one row per accepted mint that can be
/// priced right now.
pub fn table(default: Option<&Price>, accepts: &[Accept], rates: &Rates) -> Vec<Accepted> {
    accepts
        .iter()
        .filter_map(|a| {
            let price = a.price.as_ref().or(default)?;
            Some(Accepted {
                mint: a.mint.trim_end_matches('/').to_owned(),
                unit: a.unit.clone(),
                bytes_per_unit: bytes_per_unit(price, &a.unit, rates)?,
            })
        })
        .collect()
}

/// The fiat currencies a rate is needed for: any pair of price unit and token
/// unit that differ, where either side is not sats.
pub fn currencies_needed(default: Option<&Price>, accepts: &[Accept]) -> Vec<String> {
    let mut needed = std::collections::BTreeSet::new();
    for a in accepts {
        let Some(price) = a.price.as_ref().or(default) else {
            continue;
        };
        if price.unit == a.unit {
            continue;
        }
        for unit in [price.unit.as_str(), a.unit.as_str()] {
            if unit != "sat" {
                needed.insert(unit.to_owned());
            }
        }
    }
    needed.into_iter().collect()
}

/// Where a BTC price is read from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The URL. `{CUR}` and `{cur}` become the currency code in upper and
    /// lower case.
    pub url: String,
    /// A JSON pointer to the number in the response, templated the same way.
    pub path: String,
}

/// The public, keyless sources tried in order by default.
pub fn default_sources() -> Vec<Source> {
    let s = |url: &str, path: &str| Source {
        url: url.into(),
        path: path.into(),
    };
    vec![
        s("https://mempool.space/api/v1/prices", "/{CUR}"),
        s(
            "https://api.coinbase.com/v2/prices/BTC-{CUR}/spot",
            "/data/amount",
        ),
        s(
            "https://api.kraken.com/0/public/Ticker?pair=XBT{CUR}",
            "/result/XXBTZ{CUR}/c/0",
        ),
        s(
            "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies={cur}",
            "/bitcoin/{cur}",
        ),
    ]
}

fn template(text: &str, currency: &str) -> String {
    text.replace("{CUR}", &currency.to_uppercase())
        .replace("{cur}", &currency.to_lowercase())
}

/// Read a number out of a JSON document at `pointer`, whether it is a number
/// or a string holding one — several of the sources quote theirs.
pub fn extract(body: &serde_json::Value, pointer: &str) -> Option<f64> {
    match body.pointer(pointer)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Fetch one currency's BTC price, trying each source in order.
///
/// A source that fails, times out, or answers zero is skipped for the next.
pub async fn fetch(
    http: &reqwest::Client,
    sources: &[Source],
    currency: &str,
) -> Option<(f64, String)> {
    for source in sources {
        let url = template(&source.url, currency);
        let answer = async {
            let body: serde_json::Value = http
                .get(&url)
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()?;
            extract(&body, &template(&source.path, currency))
        }
        .await;
        match answer {
            Some(price) if price.is_finite() && price > 0.0 => return Some((price, url)),
            _ => debug!(%url, currency, "no usable rate from this source"),
        }
    }
    None
}

/// Keep `rates` current for `currencies`, forever.
///
/// If every source fails, the last good rate stays in use: selling at a rate a
/// few minutes old is better than not selling.
pub async fn keep_fetching(
    rates: Rates,
    sources: Vec<Source>,
    currencies: Vec<String>,
    every: Duration,
    changed: impl Fn() + Send + 'static,
) {
    if currencies.is_empty() {
        return;
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("an HTTP client");
    loop {
        for currency in &currencies {
            match fetch(&http, &sources, currency).await {
                Some((btc, from)) => {
                    info!(currency, btc, %from, "BTC rate");
                    rates.set(currency, btc);
                }
                None => match rates.btc_in(currency) {
                    Some(last) => {
                        warn!(currency, last, "every rate source failed; keeping the last")
                    }
                    None => warn!(
                        currency,
                        "every rate source failed, and there is no rate yet"
                    ),
                },
            }
        }
        changed();
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(unit: &str, per_mbit: f64) -> Price {
        Price {
            unit: unit.into(),
            per_mbit,
        }
    }

    fn rates(usd: f64, eur: f64) -> Rates {
        let r = Rates::default();
        r.set("usd", usd);
        r.set("eur", eur);
        r
    }

    #[test]
    fn a_sat_price_paid_in_sats_needs_no_rate() {
        // 0.8 sat a Mbit: a sat buys 1.25 Mbit, 156 250 bytes.
        let none = Rates::default();
        assert_eq!(
            bytes_per_unit(&price("sat", 0.8), "sat", &none),
            Some(156_250)
        );
    }

    #[test]
    fn a_fiat_price_paid_in_that_fiat_counts_cents() {
        // $0.00001 a Mbit is a thousandth of a cent: a cent buys 1000 Mbit.
        let none = Rates::default();
        assert_eq!(
            bytes_per_unit(&price("usd", 0.00001), "usd", &none),
            Some(125_000_000)
        );
    }

    #[test]
    fn a_fiat_price_paid_in_sats_goes_through_the_btc_price() {
        // BTC at $100 000: a sat is $0.001. At $0.001 a Mbit, a sat buys 1 Mbit.
        let r = rates(100_000.0, 90_000.0);
        assert_eq!(
            bytes_per_unit(&price("usd", 0.001), "sat", &r),
            Some(125_000)
        );
        // And without a rate, nothing is sold rather than guessed at.
        assert_eq!(
            bytes_per_unit(&price("usd", 0.001), "sat", &Rates::default()),
            None
        );
    }

    #[test]
    fn a_usd_price_paid_in_eur_goes_through_both_rates() {
        // BTC at $100 000 and €80 000: a euro cent is $0.0125. At $0.0125 a
        // Mbit, a euro cent buys one Mbit.
        let r = rates(100_000.0, 80_000.0);
        assert_eq!(
            bytes_per_unit(&price("usd", 0.0125), "eur", &r),
            Some(125_000)
        );
    }

    #[test]
    fn a_zero_rate_is_ignored_and_the_last_good_one_stays() {
        let r = rates(100_000.0, 80_000.0);
        r.set("usd", 0.0);
        assert_eq!(r.btc_in("usd"), Some(100_000.0));
    }

    #[test]
    fn nothing_is_given_away_for_a_price_of_zero() {
        assert_eq!(
            bytes_per_unit(&price("sat", 0.0), "sat", &Rates::default()),
            None
        );
    }

    #[test]
    fn the_table_prices_each_issuer_and_skips_what_cannot_be_priced() {
        let accepts = vec![
            Accept {
                mint: "https://trusted/".into(),
                unit: "sat".into(),
                price: Some(price("sat", 0.5)),
            },
            Accept {
                mint: "https://other".into(),
                unit: "sat".into(),
                price: None,
            },
            Accept {
                mint: "https://dollars".into(),
                unit: "usd".into(),
                price: None,
            },
        ];
        // The default is in dollars and there is no rate yet: the sat mint on
        // the default cannot be priced, the dollar mint can.
        let table = table(Some(&price("usd", 0.00001)), &accepts, &Rates::default());
        assert_eq!(
            table,
            vec![
                Accepted {
                    mint: "https://trusted".into(),
                    unit: "sat".into(),
                    bytes_per_unit: 250_000,
                },
                Accepted {
                    mint: "https://dollars".into(),
                    unit: "usd".into(),
                    bytes_per_unit: 125_000_000,
                },
            ]
        );
        assert_eq!(
            currencies_needed(Some(&price("usd", 0.00001)), &accepts),
            vec!["usd".to_string()]
        );
    }

    #[test]
    fn sources_are_templated_per_currency_and_quoted_numbers_are_read() {
        assert_eq!(
            template("https://x/BTC-{CUR}?c={cur}", "eur"),
            "https://x/BTC-EUR?c=eur"
        );
        let body = serde_json::json!({"data": {"amount": "65000.5"}, "USD": 64000});
        assert_eq!(extract(&body, "/data/amount"), Some(65_000.5));
        assert_eq!(extract(&body, "/USD"), Some(64_000.0));
        assert_eq!(extract(&body, "/missing"), None);
    }
}
