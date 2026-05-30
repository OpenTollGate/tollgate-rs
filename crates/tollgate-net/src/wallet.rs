//! Bootstrap-only wallet: verifies plain Cashu tokens against a mint.
//!
//! Uses the `cashu` crate (the protocol-primitives layer in cashubtc/cdk) for
//! NUT-00 token parsing. `cdk-spilman` for Spilman channels layers on top of
//! the same crate later without conflict.

use std::collections::HashSet;

use anyhow::{Context, bail};
use cashu::nuts::Token;

/// Bootstrap-only wallet. Accepts a Cashu token string, verifies it with its
/// mint (NUT-07 check-state), and returns the spendable amount in milli-units
/// (pricing_scale = 1000, so 1 sat → 1000 milli-units).
pub struct BootstrapWallet {
    accepted_mints: HashSet<String>,
    client: reqwest::Client,
}

impl BootstrapWallet {
    pub fn new(mint_urls: Vec<String>) -> Self {
        Self {
            accepted_mints: mint_urls.into_iter().collect(),
            client: reqwest::Client::new(),
        }
    }

    /// Parse and verify a Cashu token. Returns the amount in milli-sat if valid,
    /// or an error if invalid, already spent, or from an unaccepted mint.
    pub async fn verify(&self, token_str: &str) -> anyhow::Result<u64> {
        let token: Token = token_str.parse().context("invalid Cashu token")?;

        let mint_url = token.mint_url().context("token has no mint URL")?;
        let mint_url_str = mint_url.to_string();
        let mint_base = mint_url_str.trim_end_matches('/');

        if !self.accepted_mints.is_empty()
            && !self.accepted_mints.contains(mint_base)
            && !self.accepted_mints.contains(&mint_url_str)
        {
            bail!("mint {} not accepted", mint_url_str);
        }

        let amount_sat: u64 = token.value().context("could not sum token value")?.into();

        // Y-values (compressed pubkey of each proof's blinded secret) for the
        // NUT-07 check-state call — read from the token's raw proofs without
        // needing keyset info.
        let ys = token_proof_ys(&token);
        if ys.is_empty() {
            bail!("token contains no proofs");
        }

        self.check_proofs_unspent(mint_base, &ys).await?;

        // amount in milli-units (pricing_scale = 1000)
        Ok(amount_sat * 1_000)
    }

    /// NUT-07: check that all of `ys` are UNSPENT at the mint.
    async fn check_proofs_unspent(&self, mint_base_url: &str, ys: &[String]) -> anyhow::Result<()> {
        let url = format!("{mint_base_url}/v1/checkstate");
        let body = serde_json::json!({ "Ys": ys });

        let resp: serde_json::Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("mint check-state request failed")?
            .error_for_status()
            .context("mint returned error")?
            .json()
            .await
            .context("mint response not JSON")?;

        let states = resp["states"]
            .as_array()
            .context("mint response missing 'states'")?;
        for state in states {
            let s = state["state"].as_str().unwrap_or("");
            if s != "UNSPENT" {
                bail!("one or more proofs are already spent (state: {s})");
            }
        }
        Ok(())
    }
}

/// Extract the `C` (blinded secret pubkey) hex strings from a token's proofs.
/// These are the Y-values the mint uses for state lookup.
fn token_proof_ys(token: &Token) -> Vec<String> {
    match token {
        Token::TokenV3(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
        Token::TokenV4(t) => t
            .token
            .iter()
            .flat_map(|entry| entry.proofs.iter().map(|p| p.c.to_string()))
            .collect(),
    }
}

/// A real cashuB (v4) token for unit tests — 1 sat from testnut.cashu.space.
#[cfg(test)]
const SAMPLE_TOKEN: &str = "cashuBo2FteBtodHRwczovL3Rlc3RudXQuY2FzaHUuc3BhY2VhdWNzYXRhdIGiYWlIAYhKdLsvxe5hcIGkYWEBYXN4QDk1NTM1NzQ1YjQ2MzM2OGQ1OTVkMGVhMmQ1M2NmMDU0YjZkY2ZhZTY0NjhlOWU0N2U1MDc1YWU3OWRmNmUyODdhY1ghA03QgEalpQeCViTFYVixs-4tTxGmV0Dl-hKTQ8jLyG1ZYWSjYWVYIKlCWsnyOJRBHT_0xffz67uTQUWhk336QvZbnEQW6OUZYXNYIA88wEUIkwoL1RKs6j41AgtMZLp2e3JrlpZyU1o2M3TJYXJYILoalwd76VtIosztMCjHmQzbNUVKCM4VjvV02fSkG19-";

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn parses_token_and_reads_amount() {
        let token: Token = SAMPLE_TOKEN.parse().expect("valid cashuB token");
        let amount_sat: u64 = token.value().expect("has value").into();
        assert_eq!(amount_sat, 1);
        let mint = token.mint_url().expect("has mint URL").to_string();
        assert!(mint.contains("testnut.cashu.space"));
    }

    #[test]
    fn extracts_proof_y_values() {
        let token: Token = SAMPLE_TOKEN.parse().expect("valid token");
        let ys = token_proof_ys(&token);
        assert_eq!(ys.len(), 1);
        // Y-values are 33-byte compressed pubkeys in hex (66 chars).
        assert_eq!(ys[0].len(), 66);
    }

    #[test]
    fn rejects_token_from_unlisted_mint() {
        let wallet = BootstrapWallet::new(vec!["https://allowed-mint.example".to_string()]);
        let result = rt().block_on(wallet.verify(SAMPLE_TOKEN));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not accepted"));
    }

    #[test]
    fn open_mint_list_passes_mint_filter() {
        // Empty accepted_mints means any mint passes the filter.
        let wallet = BootstrapWallet::new(vec![]);
        assert!(wallet.accepted_mints.is_empty());
        let token: Token = SAMPLE_TOKEN.parse().expect("valid token");
        let _amount_sat: u64 = token.value().expect("has value").into();
    }

    #[test]
    fn rejects_invalid_token_string() {
        let wallet = BootstrapWallet::new(vec![]);
        let result = rt().block_on(wallet.verify("not-a-token"));
        assert!(result.is_err());
    }

    #[test]
    fn milli_unit_scaling() {
        // 1 sat → 1000 milli-units (pricing_scale = 1000).
        let token: Token = SAMPLE_TOKEN.parse().expect("valid token");
        let sat: u64 = token.value().expect("value").into();
        assert_eq!(sat * 1_000, 1_000);
    }
}
