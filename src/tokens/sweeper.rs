//! The *sweep* capability. DUMMY — the trait and its types exist so the
//! registry can index sweep-capable handlers and the frontend can render the
//! capability badge. Nothing implements it yet.
//!
//! Shape note: a sweep is requested against an **asset**, not a token ID. The
//! ledger says "merchant M holds 50 of evm/8453/0xa0b8…", asks the registry
//! which handlers advertise that asset *and* can sweep, and then either calls
//! the only one or asks the operator to choose. The token ID never enters the
//! ledger.

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::assets::AssetKey;

#[derive(Clone, Debug)]
pub struct SweepRequest {
    pub merchant_id: Uuid,
    /// What to move. Must match the handler's advertised asset.
    pub asset: AssetKey,
    /// Base units, as the ledger holds them.
    pub amount: Decimal,
    /// Which derived deposit address to sweep from. `None` means "the handler
    /// decides" — e.g. consolidate every funded index it knows about.
    pub from_wallet_index: Option<u32>,
    /// Override the merchant's configured treasury address.
    pub destination: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SweepOutcome {
    /// Chain-shaped transaction identifier: EVM tx hash, Solana signature, txid.
    pub tx_ref: String,
    pub swept: Decimal,
    /// In the chain's *native* asset, not in `asset`. `None` if unknown at
    /// broadcast time.
    pub fee_paid: Option<Decimal>,
}

#[async_trait]
pub trait Sweeper: Send + Sync {
    async fn sweep(&self, pool: &PgPool, req: &SweepRequest) -> Result<SweepOutcome, String>;

    /// Lets the UI show "sweeping costs ~X" and lets a planner skip dust.
    async fn estimate_fee(
        &self,
        _pool: &PgPool,
        _req: &SweepRequest,
    ) -> Result<Option<Decimal>, String> {
        Ok(None)
    }
}
