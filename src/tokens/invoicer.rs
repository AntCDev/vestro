//! The *invoice* capability.
//!
//! A token that has an `Invoicer` can be picked at checkout: it derives a
//! deposit address, fills in the invoice row, and serves the checkout page.
//!
//! There is deliberately no separate `Observer`. Observation is not something a
//! token handler does — the network client spins up a watcher that queries
//! `invoices` for its own (network_type, chain_ref) and finds the rows itself.
//! Creating an invoice on a network whose client is running *is* subscribing to
//! it. A handler that can invoice but must not be watched is a network-level
//! configuration (don't spin the client up), not a handler-level flag.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::tokens::checkout::{CheckoutContext, CheckoutView, PresignContext, StatusContext, GENERIC_VIEW};

#[derive(Clone, Debug, Serialize)]
pub struct PaymentDetails {
    pub invoice_id: Uuid,
    pub network: String,
    pub deposit_address: String,
    pub token_address: Option<String>,
    pub decimals: u8,
    pub required_confirmations: i32,
    pub wallet_index: u32,
    pub expires_at: DateTime<Utc>,
}

#[async_trait]
pub trait Invoicer: Send + Sync {
    /// Fill in the invoice row the orchestrator already inserted.
    ///
    /// The orchestrator has already written `network_type`, `chain_ref`,
    /// `token_address`, `token_program` and `token_decimals` from the
    /// descriptor's advertised asset, so this method only owns what it
    /// actually computes: the derived address, the reference, the expiry,
    /// the confirmation policy and the creation-height floor.
    ///
    /// Setting `wallet_address` is what makes the row visible to the watcher,
    /// so it should be the last write.
    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
        token_id: &str,
    ) -> Result<PaymentDetails, String>;

    async fn cancel_payment(&self, pool: &PgPool, invoice_id: Uuid) -> Result<(), String>;

    /// The default checkout view for this token. Seeded into `checkout_views` /
    /// `token_checkout_views` on boot; after that the DB is authoritative and
    /// operator changes win.
    fn checkout_view(&self) -> CheckoutView {
        GENERIC_VIEW
    }

    /// Opaque, network-shaped payload for the checkout page. Called ONCE per
    /// page load, so it may be moderately expensive.
    ///
    /// PUBLIC: the invoice UUID is the only thing gating this endpoint.
    /// Never return derivation paths, wallet indices, or key material.
    async fn checkout_data(
        &self,
        _pool: &PgPool,
        _ctx: &CheckoutContext,
    ) -> Result<Value, String> {
        Ok(json!({}))
    }

    /// Optional extras merged into the polled status response. Called on EVERY
    /// poll — must be cheap. No RPC calls, no unbounded queries.
    async fn status_data(
        &self,
        _pool: &PgPool,
        _ctx: &StatusContext,
    ) -> Result<Value, String> {
        Ok(Value::Null)
    }

    /// Fresh, per-attempt data the wallet needs to build a signable
    /// transaction. Allowed to hit the network; handlers cache if expensive.
    ///
    /// `Value::Null` means "this token has no pre-sign step", which the API
    /// turns into a 400 rather than handing the page an empty object.
    async fn presign_data(
        &self,
        _pool: &PgPool,
        _ctx: &PresignContext,
    ) -> Result<Value, String> {
        Ok(Value::Null)
    }
}
