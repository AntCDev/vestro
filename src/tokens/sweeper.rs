//! The *sweep* capability. A sweeper does not move value. It finishes the
//! orchestrator's draft into a concrete TransferPlan; the network worker does
//! the moving. That keeps handlers stateless and keys out of this layer.

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::ledgerer::{AddressKind, AssetKey};
use crate::networks::transfers::{SignerRef, SourceAccount, TransferAmount};

/// What the orchestrator knows before asking the handler.
#[derive(Clone, Debug)]
pub struct SweepDraft {
    pub merchant_id: Uuid,
    pub asset: AssetKey,
    pub asset_id: Uuid,
    pub custody_address: String,
    pub custody_kind: AddressKind,
    pub authority_address: String,
    pub authority: SignerRef,
    /// Merchant main wallet on this family, already looked up.
    pub destination: String,

    /// Sum of the pending sweep rows being grouped. Informational
    pub queued_total: Decimal,
    pub amount: TransferAmount,
    pub movement_count: usize,
    /// Merged `sweep_queue.sweep_params` of the grouped rows.
    pub sweep_params: Value,
}

/// What the handler hands back. Persisted verbatim into outbound_transfers.
#[derive(Clone, Debug)]
pub struct TransferPlan {
    pub from: SourceAccount,
    pub to: String,
    pub amount: TransferAmount,
    pub fee_payer: Option<SignerRef>,
    pub params: Value,
}

#[async_trait]
pub trait Sweeper: Send + Sync {
    async fn plan(&self, pool: &PgPool, draft: &SweepDraft) -> Result<TransferPlan, String>;
}
