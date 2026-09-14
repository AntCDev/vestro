//! The generic outbound-transfer contract every NetworkClient implements.
//! Chain-agnostic on purpose: the worker in `networks/outbound.rs` drives this
//! without knowing what a nonce, a blockhash or an ATA is.

use serde_json::Value;
use uuid::Uuid;
use crate::keys::derivation::KeyRole;
use crate::ledgerer::{AddressKind, AssetKey};

/// Who can sign. Never an address — a sweeper must reach a key.
/// Canonical text form (`sweep_queue.authority_ref`) is "role:index", e.g. "0:7".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignerRef {
    pub role: KeyRole,
    pub index: u32,
}

impl SignerRef {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (r, i) = s.split_once(':').ok_or_else(|| format!("bad authority_ref {s:?}, want role:index"))?;
        Ok(Self {
            role: KeyRole::from_i16(r.parse().map_err(|_| format!("bad role in {s:?}"))?)?,
            index: i.parse().map_err(|_| format!("bad index in {s:?}"))?,
        })
    }
    pub fn to_ref(&self) -> String { format!("{}:{}", self.role.as_i16(), self.index) }
}

/// Where value sits + who moves it. Same address on EVM; ATA vs owner on SPL.
#[derive(Clone, Debug)]
pub struct SourceAccount {
    pub address: String,
    pub kind: AddressKind,
    pub authority: SignerRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferAmount {
    Exact(u128),
    /// Drain. Only the network knows the reserve (rent, gas headroom, fee).
    Max,
}

/// Fully-specified request. Reconstructed from an `outbound_transfers` row.
#[derive(Clone, Debug)]
pub struct TransferRequest {
    pub id: Uuid,                 // outbound_transfers.id — the idempotency key
    pub merchant_id: Uuid,
    pub asset: AssetKey,
    pub from: SourceAccount,
    pub to: String,
    pub amount: TransferAmount,
    /// None = `from.authority` pays. Solana sweeps put the gas feeder here.
    pub fee_payer: Option<SignerRef>,
    pub params: Value,
}

/// Produced by `build_and_sign`. Persisted *before* broadcast.
#[derive(Clone, Debug)]
pub struct SignedTransfer {
    pub tx_hash: String,
    pub raw: Vec<u8>,
    /// What the tx actually moves — `Max` resolved to a number.
    pub from: String,
    pub amount: u128,
    pub valid_until: Option<u64>,
    pub nonce: Option<u64>,
    pub fee_estimate: Option<u128>,
}

#[derive(Clone, Debug)]
pub enum TransferStatus {
    /// Not seen. May still land. NOT a licence to rebuild.
    Unknown,
    Pending,
    Confirmed { block: u64, fee_paid: u128 },
    /// Landed and reverted / errored on chain. Terminal; value did not move.
    Failed { reason: String },
    /// Can never land now (blockhash dead, nonce consumed elsewhere). Rebuild is safe.
    Expired,
}
