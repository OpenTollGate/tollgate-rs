//! What travels in `Accept` and `RolloverInit`: a Spilman channel's funding.
//!
//! Spilman carries funding with the first payment; the TollGate handshake
//! confirms a channel before any grant, so the opening payment is pulled out
//! and sent here instead. Its balance is zero — it buys nothing, it only opens.
//!
//! # Only what the receiver cannot work out
//!
//! cdk-spilman's own `Payment` is JSON, and most of it is redundant between two
//! parties who already share a secret. The receiver rebuilds all of it:
//!
//! - **The keyset.** Named by id and fetched from the mint, which the receiver
//!   has to do anyway before it can check a signature against it.
//! - **The channel secret.** ECDH between the two keys, so each side computes
//!   it and it never travels.
//! - **The channel id.** A hash over the terms and the secret.
//! - **The funding proofs' secrets, blinding factors and amounts.** All
//!   derived deterministically from the terms and the secret — which is what
//!   lets the receiver check them in the first place. What it cannot derive is
//!   the mint's signature on each one: `C`, and the DLEQ proof's `e` and `s`
//!   that show `C` is genuine.
//!
//! So the blob is the eleven terms the channel id commits to, the opening
//! signature, and 97 bytes per proof.
//!
//! # Encoding
//!
//! A CBOR map with integer keys, the same shape as the messages it rides in.
//! Unknown keys are skipped. `funding.cddl`, next to this file, is the
//! normative schema, and the tests below check the encoder against it.
//!
//! | Key | Field                  | Type                          |
//! |-----|------------------------|-------------------------------|
//! | 0   | signature              | bytes(64), BIP-340, balance 0 |
//! | 1   | mint                   | text                          |
//! | 2   | unit                   | text                          |
//! | 3   | capacity               | uint                          |
//! | 4   | funding_token_amount   | uint                          |
//! | 5   | keyset_id              | bytes (version byte + id)     |
//! | 6   | input_fee_ppk          | uint                          |
//! | 7   | maximum_amount         | uint                          |
//! | 8   | setup_timestamp        | uint                          |
//! | 9   | sender_pubkey          | bytes(33)                     |
//! | 10  | receiver_pubkey        | bytes(33)                     |
//! | 11  | expiry_timestamp       | uint                          |
//! | 12  | outputs                | array of `[C, e, s]`          |
//!
//! Outputs are in the order cdk-spilman derives them, smallest amount first.
//! `C` is 33 bytes; `e` and `s` are 32.

use anyhow::{Context, Result, anyhow, bail, ensure};
use cashu::Amount;
use cashu::nuts::{Id, Proof, ProofDleq, PublicKey, SecretKey};
use cdk_spilman::{
    ChannelParameters, DeterministicOutputsForOneContext, Payment, parse_keyset_info_from_json,
};
use minicbor::{Decoder, Encoder};

/// The terms a channel id commits to.
///
/// Field for field what cdk-spilman's `get_channel_id_params_json` produces,
/// and what `ChannelParameters::from_json_with_secret_key` rebuilds the full
/// parameters from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Terms {
    pub mint: String,
    pub unit: String,
    pub capacity: u64,
    pub funding_token_amount: u64,
    pub keyset_id: Id,
    pub input_fee_ppk: u64,
    pub maximum_amount: u64,
    pub setup_timestamp: u64,
    pub sender: PublicKey,
    pub receiver: PublicKey,
    pub expiry_timestamp: u64,
}

/// The mint's signature on one funding output, and its proof of honesty.
///
/// The rest of the proof — amount, secret, and the blinding factor the DLEQ
/// proof also needs — the receiver derives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Signed {
    c: [u8; 33],
    e: [u8; 32],
    s: [u8; 32],
}

/// A channel's funding, as it travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Funding {
    /// The sender's signature over a balance of zero.
    pub signature: [u8; 64],
    pub terms: Terms,
    pub outputs: Vec<Signed>,
}

/// Funding rebuilt into what `SpilmanBridge::fund_channel` takes.
#[derive(Debug)]
pub(super) struct Opening {
    pub channel_id: String,
    pub signature: String,
    pub params: serde_json::Value,
    pub proofs: Vec<Proof>,
}

impl Terms {
    fn from_json(params: &serde_json::Value) -> Result<Self> {
        let text = |key: &str| {
            params[key]
                .as_str()
                .ok_or_else(|| anyhow!("channel parameters have no {key}"))
        };
        let number = |key: &str| {
            params[key]
                .as_u64()
                .ok_or_else(|| anyhow!("channel parameters have no {key}"))
        };
        let key = |name: &str| -> Result<PublicKey> {
            PublicKey::from_hex(text(name)?).map_err(|e| anyhow!("{name}: {e}"))
        };

        Ok(Self {
            mint: text("mint")?.to_string(),
            unit: text("unit")?.to_string(),
            capacity: number("capacity")?,
            funding_token_amount: number("funding_token_amount")?,
            keyset_id: text("keyset_id")?
                .parse()
                .map_err(|e| anyhow!("keyset_id: {e}"))?,
            input_fee_ppk: number("input_fee_ppk")?,
            maximum_amount: number("maximum_amount")?,
            setup_timestamp: number("setup_timestamp")?,
            sender: key("sender_pubkey")?,
            receiver: key("receiver_pubkey")?,
            expiry_timestamp: number("expiry_timestamp")?,
        })
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "mint": self.mint,
            "unit": self.unit,
            "capacity": self.capacity,
            "funding_token_amount": self.funding_token_amount,
            "keyset_id": self.keyset_id.to_string(),
            "input_fee_ppk": self.input_fee_ppk,
            "maximum_amount": self.maximum_amount,
            "setup_timestamp": self.setup_timestamp,
            "sender_pubkey": self.sender.to_hex(),
            "receiver_pubkey": self.receiver.to_hex(),
            "expiry_timestamp": self.expiry_timestamp,
        })
    }
}

impl Funding {
    /// Strip the client's opening payment down to what travels.
    ///
    /// Refuses anything the receiver could not rebuild exactly — a proof on
    /// another keyset, or one carrying a witness — rather than send funding
    /// that would only fail on the other side.
    pub fn from_opening(opening: &Payment) -> Result<Self> {
        ensure!(
            opening.balance == 0,
            "an opening payment is for a balance of zero, not {}",
            opening.balance
        );
        let signature = hex::decode(&opening.signature)
            .ok()
            .and_then(|s| <[u8; 64]>::try_from(s).ok())
            .ok_or_else(|| anyhow!("the opening signature is not 64 bytes of hex"))?;
        let terms = Terms::from_json(
            opening
                .params
                .as_ref()
                .ok_or_else(|| anyhow!("the opening payment carried no channel parameters"))?,
        )?;
        let proofs = opening
            .funding_proofs
            .as_deref()
            .ok_or_else(|| anyhow!("the opening payment carried no proofs"))?;

        let outputs = proofs
            .iter()
            .map(|proof| {
                ensure!(
                    proof.keyset_id == terms.keyset_id,
                    "a funding proof is on keyset {}, not the channel's {}",
                    proof.keyset_id,
                    terms.keyset_id
                );
                ensure!(
                    proof.witness.is_none() && proof.p2pk_e.is_none(),
                    "a funding proof carries data the receiver cannot rebuild"
                );
                let dleq = proof
                    .dleq
                    .as_ref()
                    .ok_or_else(|| anyhow!("a funding proof has no DLEQ proof"))?;
                Ok(Signed {
                    c: proof.c.to_bytes(),
                    e: dleq.e.to_secret_bytes(),
                    s: dleq.s.to_secret_bytes(),
                })
            })
            .collect::<Result<_>>()?;

        Ok(Self {
            signature,
            terms,
            outputs,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let t = &self.terms;
        let mut e = Encoder::new(&mut out);
        // Writing to a Vec cannot fail.
        (|| -> Result<(), minicbor::encode::Error<core::convert::Infallible>> {
            e.map(13)?;
            e.u8(0)?.bytes(&self.signature)?;
            e.u8(1)?.str(&t.mint)?;
            e.u8(2)?.str(&t.unit)?;
            e.u8(3)?.u64(t.capacity)?;
            e.u8(4)?.u64(t.funding_token_amount)?;
            e.u8(5)?.bytes(&t.keyset_id.to_bytes())?;
            e.u8(6)?.u64(t.input_fee_ppk)?;
            e.u8(7)?.u64(t.maximum_amount)?;
            e.u8(8)?.u64(t.setup_timestamp)?;
            e.u8(9)?.bytes(&t.sender.to_bytes())?;
            e.u8(10)?.bytes(&t.receiver.to_bytes())?;
            e.u8(11)?.u64(t.expiry_timestamp)?;
            e.u8(12)?.array(self.outputs.len() as u64)?;
            for o in &self.outputs {
                e.array(3)?.bytes(&o.c)?.bytes(&o.e)?.bytes(&o.s)?;
            }
            Ok(())
        })()
        .expect("encoding into a Vec is infallible");
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(&mut Decoder::new(bytes)).context("funding blob is not the expected shape")
    }

    /// Rebuild the full funding, as the receiver.
    ///
    /// `keyset_info` is the mint's keyset as cdk-spilman reads it, and `ours`
    /// the receiver's key, from which the channel secret follows.
    pub fn open(&self, keyset_info: &str, ours: &SecretKey) -> Result<Opening> {
        let params_json = self.terms.to_json();
        let keyset = parse_keyset_info_from_json(keyset_info)
            .map_err(|e| anyhow!("keyset {}: {e}", self.terms.keyset_id))?;

        // The terms decide how many outputs are derived, one at a time, and a
        // peer writes the terms: a maximum of 1 against a large funding amount
        // would have us derive billions of them before anything is checked.
        // The split takes at least `funding_token_amount / largest` outputs, so
        // terms that need more than were sent are refused before deriving any.
        let max = self.terms.maximum_amount;
        let largest = keyset
            .amounts_largest_first
            .iter()
            .copied()
            .find(|&a| max == 0 || a <= max)
            .ok_or_else(|| anyhow!("the keyset has no amount at or below {max}"))?;
        ensure!(
            self.terms.funding_token_amount <= largest.saturating_mul(self.outputs.len() as u64),
            "{} cannot be funded in the {} proofs sent",
            self.terms.funding_token_amount,
            self.outputs.len()
        );

        let params =
            ChannelParameters::from_json_with_secret_key(&params_json.to_string(), keyset, ours)
                .context("rebuild the channel parameters")?;

        let derived = DeterministicOutputsForOneContext::new(
            "funding".into(),
            params.funding_token_amount,
            params.clone(),
        )
        .and_then(|d| d.get_secrets_with_blinding())
        .context("derive the funding outputs")?;
        if derived.len() != self.outputs.len() {
            bail!(
                "the channel's terms make {} funding proofs and {} were sent",
                derived.len(),
                self.outputs.len()
            );
        }

        let proofs = derived
            .into_iter()
            .zip(&self.outputs)
            .map(|(d, o)| {
                let c = PublicKey::from_slice(&o.c).map_err(|e| anyhow!("C: {e}"))?;
                let e = SecretKey::from_slice(&o.e).map_err(|e| anyhow!("DLEQ e: {e}"))?;
                let s = SecretKey::from_slice(&o.s).map_err(|e| anyhow!("DLEQ s: {e}"))?;
                let mut proof =
                    Proof::new(Amount::from(d.amount), self.terms.keyset_id, d.secret, c);
                proof.dleq = Some(ProofDleq::new(e, s, d.blinding_factor));
                Ok(proof)
            })
            .collect::<Result<_>>()?;

        Ok(Opening {
            channel_id: params.get_channel_id(),
            signature: hex::encode(self.signature),
            params: params_json,
            proofs,
        })
    }
}

fn decode(d: &mut Decoder<'_>) -> Result<Funding> {
    let pairs = d.map()?.ok_or_else(|| anyhow!("indefinite-length map"))?;

    let mut signature = None;
    let (mut mint, mut unit) = (None, None);
    let (mut capacity, mut funding_token_amount) = (None, None);
    let (mut keyset_id, mut input_fee_ppk, mut maximum_amount) = (None, None, None);
    let (mut setup_timestamp, mut expiry_timestamp) = (None, None);
    let (mut sender, mut receiver) = (None, None);
    let mut outputs = None;

    for _ in 0..pairs {
        match d.u8()? {
            0 => signature = Some(fixed::<64>(d)?),
            1 => mint = Some(d.str()?.to_string()),
            2 => unit = Some(d.str()?.to_string()),
            3 => capacity = Some(d.u64()?),
            4 => funding_token_amount = Some(d.u64()?),
            5 => {
                keyset_id = Some(Id::from_bytes(d.bytes()?).map_err(|e| anyhow!("keyset id: {e}"))?)
            }
            6 => input_fee_ppk = Some(d.u64()?),
            7 => maximum_amount = Some(d.u64()?),
            8 => setup_timestamp = Some(d.u64()?),
            9 => sender = Some(pubkey(d)?),
            10 => receiver = Some(pubkey(d)?),
            11 => expiry_timestamp = Some(d.u64()?),
            12 => {
                let n = d
                    .array()?
                    .ok_or_else(|| anyhow!("indefinite-length output list"))?;
                let mut list = Vec::new();
                for _ in 0..n {
                    if d.array()? != Some(3) {
                        bail!("a funding output is not [C, e, s]");
                    }
                    list.push(Signed {
                        c: fixed(d)?,
                        e: fixed(d)?,
                        s: fixed(d)?,
                    });
                }
                outputs = Some(list);
            }
            _ => d.skip()?,
        }
    }

    fn need<T>(value: Option<T>, key: u8) -> Result<T> {
        value.ok_or_else(|| anyhow!("missing key {key}"))
    }

    Ok(Funding {
        signature: need(signature, 0)?,
        terms: Terms {
            mint: need(mint, 1)?,
            unit: need(unit, 2)?,
            capacity: need(capacity, 3)?,
            funding_token_amount: need(funding_token_amount, 4)?,
            keyset_id: need(keyset_id, 5)?,
            input_fee_ppk: need(input_fee_ppk, 6)?,
            maximum_amount: need(maximum_amount, 7)?,
            setup_timestamp: need(setup_timestamp, 8)?,
            sender: need(sender, 9)?,
            receiver: need(receiver, 10)?,
            expiry_timestamp: need(expiry_timestamp, 11)?,
        },
        outputs: need(outputs, 12)?,
    })
}

fn fixed<const N: usize>(d: &mut Decoder<'_>) -> Result<[u8; N]> {
    let bytes = d.bytes()?;
    <[u8; N]>::try_from(bytes).map_err(|_| anyhow!("expected {N} bytes, got {}", bytes.len()))
}

fn pubkey(d: &mut Decoder<'_>) -> Result<PublicKey> {
    PublicKey::from_slice(&fixed::<33>(d)?).map_err(|e| anyhow!("public key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> PublicKey {
        SecretKey::from_slice(&[n; 32]).expect("key").public_key()
    }

    fn funding(outputs: usize) -> Funding {
        Funding {
            signature: [9; 64],
            terms: Terms {
                mint: "http://127.0.0.1:3338".into(),
                unit: "byte".into(),
                capacity: 1 << 30,
                funding_token_amount: 1 << 30,
                keyset_id: "00456a94ab4e1c46".parse().expect("keyset id"),
                input_fee_ppk: 0,
                maximum_amount: 1 << 30,
                setup_timestamp: 1_700_000_000,
                sender: key(1),
                receiver: key(2),
                expiry_timestamp: 1_700_007_200,
            },
            outputs: (0..outputs)
                .map(|i| Signed {
                    c: key(3 + i as u8).to_bytes(),
                    e: [4; 32],
                    s: [5; 32],
                })
                .collect(),
        }
    }

    #[test]
    fn funding_round_trips_through_cbor() {
        for n in [0, 1, 30] {
            let f = funding(n);
            assert_eq!(Funding::decode(&f.encode()).expect("decode"), f);
        }
    }

    const SCHEMA: &str = include_str!("funding.cddl");

    fn validate(cbor: &[u8]) -> std::result::Result<(), String> {
        cddl::validate_cbor_from_slice(SCHEMA, cbor, None).map_err(|e| e.to_string())
    }

    #[test]
    fn the_encoded_blob_matches_funding_cddl() {
        let v2: Id = format!("01{}", "ab".repeat(32))
            .parse()
            .expect("v2 keyset id");
        for n in [0, 1, 30] {
            let mut f = funding(n);
            validate(&f.encode()).unwrap_or_else(|e| panic!("{n} outputs, v1 keyset: {e}"));
            f.terms.keyset_id = v2;
            validate(&f.encode()).unwrap_or_else(|e| panic!("{n} outputs, v2 keyset: {e}"));
        }
    }

    #[test]
    fn funding_cddl_refuses_what_decode_refuses() {
        // Only a signature: every other required key is missing.
        let mut short = Vec::new();
        Encoder::new(&mut short)
            .map(1)
            .and_then(|e| e.u8(0))
            .and_then(|e| e.bytes(&[0; 64]))
            .expect("encode");
        assert!(Funding::decode(&short).is_err());
        let err = validate(&short).expect_err("a lone signature is not a funding blob");
        assert!(err.contains("missing key: 1"), "{err}");

        // A 63-byte signature, the rest intact. The signature is key 0, the
        // first thing after the map header.
        let mut bytes = funding(1).encode();
        assert_eq!(&bytes[1..4], &[0x00, 0x58, 0x40], "key 0, bytes(64)");
        bytes[3] = 63;
        bytes.remove(4);
        assert!(Funding::decode(&bytes).is_err());
        let err = validate(&bytes).expect_err("a 63-byte signature");
        assert!(err.contains(".size 64, got 63"), "{err}");
    }

    #[test]
    fn terms_round_trip_through_the_json_cdk_spilman_reads() {
        let t = funding(0).terms;
        assert_eq!(Terms::from_json(&t.to_json()).expect("parse"), t);
    }

    #[test]
    fn each_output_costs_what_the_receiver_cannot_derive_and_little_more() {
        // 33 + 32 + 32 bytes of payload, plus CBOR headers.
        let per_output = funding(2).encode().len() - funding(1).encode().len();
        assert_eq!(per_output, 1 + (2 + 33) + (2 + 32) + (2 + 32));
    }

    #[test]
    fn unknown_keys_are_skipped() {
        let f = funding(1);
        let bytes = f.encode();
        // Rewrite the map header from 13 pairs to 14 and append `99: "later"`.
        let mut extended = vec![0xae];
        extended.extend_from_slice(&bytes[1..]);
        let mut tail = Vec::new();
        Encoder::new(&mut tail)
            .u8(99)
            .and_then(|e| e.str("later"))
            .expect("encode");
        extended.extend(tail);
        assert_eq!(Funding::decode(&extended).expect("decode"), f);
    }

    #[test]
    fn missing_or_malformed_fields_are_refused() {
        let bytes = funding(1).encode();
        for cut in 0..bytes.len() {
            assert!(
                Funding::decode(&bytes[..cut]).is_err(),
                "a blob cut to {cut} bytes should not decode"
            );
        }
        assert!(Funding::decode(b"{\"channel_id\":\"\"}").is_err());

        let mut short = Vec::new();
        Encoder::new(&mut short)
            .map(1)
            .and_then(|e| e.u8(0))
            .and_then(|e| e.bytes(&[0; 64]))
            .expect("encode");
        assert!(Funding::decode(&short).is_err(), "only a signature");
    }
}
