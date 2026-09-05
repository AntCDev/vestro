//! Types the checkout page hands to (and takes from) an `Invoicer`.
//! Pure data — no behaviour, no DB access.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// A frontend checkout file, identified by ID rather than path so the path
/// can be changed in the DB without a recompile.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CheckoutView {
    pub id: &'static str,
    pub path: &'static str,
    pub description: &'static str,
}

pub const GENERIC_VIEW: CheckoutView = CheckoutView {
    id: "generic",
    path: "/checkout/generic.html",
    description: "Address + QR only. Fallback for handlers with no dedicated view.",
};

/// Full invoice snapshot handed to an invoicer on checkout page load.
/// Read-only; the invoicer should not re-query the invoice.
#[derive(Debug, Clone)]
pub struct CheckoutContext {
    pub invoice_id: Uuid,
    pub merchant_id: Uuid,
    pub token_id: String,
    pub token_address: Option<String>,
    pub token_program: Option<String>,
    pub token_decimals: Option<i16>,
    pub amount_requested: rust_decimal::Decimal,
    pub amount_received: rust_decimal::Decimal,
    pub wallet_address: String,
    pub payment_reference: Option<String>,
    pub status: String,
    pub required_confirmations: Option<i16>,
    pub network_type: Option<String>,
    pub chain_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// invoices.data passthrough, if the handler stashed anything at creation.
    pub data: Option<String>,
}

/// Cheap subset, rebuilt on every poll.
#[derive(Debug, Clone)]
pub struct StatusContext {
    pub invoice_id: Uuid,
    pub token_id: String,
    pub wallet_address: String,
    pub payment_reference: Option<String>,
    pub status: String,
    pub amount_requested: rust_decimal::Decimal,
    pub amount_received: rust_decimal::Decimal,
    pub expires_at: DateTime<Utc>,
}

pub struct PresignContext {
    pub invoice_id: Uuid,
    pub token_id: String,
    pub status: String,
    pub expires_at: DateTime<Utc>,
}
