//! Bootstrap-only wallet: verifies plain Cashu tokens against a mint.
//!
//! The `cashu` crate (the protocol-primitives layer inside cashubtc/cdk) is
//! the right dependency level for this — it provides NUT-00 token parsing and
//! the mint HTTP API client without pulling in the full wallet SDK.
//!
//! For now this is a stub that accepts everything so the server can start.
//! Real mint verification is the next step.

pub struct BootstrapWallet {
    #[allow(dead_code)]
    mint_urls: Vec<String>,
}

impl BootstrapWallet {
    pub fn new(mint_urls: Vec<String>) -> Self {
        Self { mint_urls }
    }

    /// Verify a raw Cashu token and return its value in milli-units (scale=1000).
    ///
    /// TODO: parse NUT-00 token, call mint /v1/check, return real value.
    pub async fn verify(&self, _token: &[u8]) -> anyhow::Result<Option<u64>> {
        tracing::warn!("bootstrap wallet is a stub — accepting all tokens");
        Ok(Some(5_000)) // 5 sat @ scale=1000
    }
}
