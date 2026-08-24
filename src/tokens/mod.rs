use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;
use serde_json::{json, Value};
use async_trait::async_trait;
use uuid::Uuid;
use sqlx::PgPool;
use chrono::{DateTime, Utc};
use crate::networks::NetworkRegistry; // Import your central network registry
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
pub mod eth;
pub mod sepolia;
pub mod base;
pub mod base_sepolia;
mod sol_devnet;
mod evm_common;
mod sol_common;
mod bitcoin;

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

/// Full invoice snapshot handed to a handler on checkout page load.
/// Read-only; the handler should not re-query the invoice.
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
pub struct PresignContext {
    pub invoice_id: Uuid,
    pub token_id: String,
    pub status: String,
    pub expires_at: DateTime<Utc>,
}
#[async_trait]
pub trait TokenHandler: Send + Sync {
    fn token_id(&self) -> &str;

    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
        token_id: &str,
    ) -> Result<PaymentDetails, String>;

    async fn cancel_payment(&self, pool: &PgPool, invoice_id: Uuid) -> Result<(), String>;

    /// The default checkout view for this handler. Seeded into
    /// `checkout_views` / `token_checkout_views` on boot; after that the DB
    /// is authoritative and operator changes win.
    fn checkout_view(&self) -> CheckoutView {
        GENERIC_VIEW
    }
    
    /// Opaque, network-shaped payload for the checkout page. Called ONCE per
    /// page load, so it may be moderately expensive (building a WalletConnect
    /// payload, an RPC call for a fee estimate, etc).
    ///
    /// PUBLIC: the invoice UUID is the only thing gating this endpoint.
    /// Anything returned here is readable by anyone holding the checkout link.
    /// Never return derivation paths, wallet indices, or key material.
    ///
    /// The shape is deliberately unconstrained — the view file assigned to this
    /// token is expected to know how to parse it. A mismatched view/handler
    /// pairing is an operator configuration error, not a runtime contract.
    async fn checkout_data(
        &self,
        _pool: &PgPool,
        _ctx: &CheckoutContext,
    ) -> Result<Value, String> {
        Ok(json!({}))
    }

    /// Optional network-specific extras merged into the polled status response.
    /// Called on EVERY poll — must be cheap. No RPC calls, no unbounded queries.
    /// Default is `null`, which costs nothing.
    async fn status_data(
        &self,
        _pool: &PgPool,
        _ctx: &StatusContext,
    ) -> Result<Value, String> {
        Ok(Value::Null)
    }

    /// Fresh, per-attempt data the wallet needs to build a signable
    /// transaction — a Solana blockhash, an EVM nonce and fee estimate.
    ///
    /// Unlike checkout_data this runs on every payment attempt, and it is
    /// allowed to hit the network; handlers cache if that call is expensive.
    ///
    /// PUBLIC, on the same terms as checkout_data: the invoice UUID is the
    /// only gate. This returns facts about the chain, never anything derived
    /// from a key, and the server signs nothing on this path.
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

#[derive(Clone, Serialize)]
pub struct TokenMetadata {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub info: String,
}

pub struct TokenRegistry {
    handlers: HashMap<String, Arc<dyn TokenHandler>>,
    metadata: Vec<TokenMetadata>,
}

impl TokenRegistry {
    /// Accepts the shared single-instance networks on initialization
    pub fn new(networks: Arc<NetworkRegistry>) -> Self {
        println!("\n🪙 Registering Token Handlers...");
        let mut registry = Self {
            handlers: HashMap::new(),
            metadata: Vec::new(),
        };

        // Pass the networks registry forward to sub-modules
        eth::register(&mut registry, networks.clone());
        sepolia::register(&mut registry, networks.clone());
        base::register(&mut registry, networks.clone());
        base_sepolia::register(&mut registry, networks.clone());

        sol_devnet::register(&mut registry, networks.clone());
        registry
    }

    pub fn register_token<H>(&mut self, id: &str, name: &str, detail: &str, info: &str, handler: H)
    where
        H: TokenHandler + 'static,
    {
        // Extract struct name from full type path (e.g. "my_app::tokens::eth::EthHandler" -> "EthHandler")
        let full_type = std::any::type_name::<H>();
        let handler_name = full_type.split("::").last().unwrap_or(full_type);

        println!("  ✅ {} - {} - {} - {}", id, name, detail, handler_name);

        self.metadata.push(TokenMetadata {
            id: id.to_string(),
            name: name.to_string(),
            detail: detail.to_string(),
            info: info.to_string(),
        });
        self.handlers.insert(id.to_string(), Arc::new(handler));
    }

    pub fn get_metadata(&self) -> Vec<TokenMetadata> {
        self.metadata.clone()
    }

    pub fn get_handler(&self, id: &str) -> Option<Arc<dyn TokenHandler>> {
        self.handlers.get(id).cloned()
    }

    /// Seeds the view catalogue and the token->view mapping.
    /// Idempotent: existing rows are never overwritten, so an operator who
    /// repoints a token in the DB keeps that choice across restarts.
    pub async fn sync_checkout_views(&self, pool: &PgPool) -> Result<(), sqlx::Error> {
        println!("\n🖼️  Syncing checkout views...");
        let mut tx = pool.begin().await?;

        for (token_id, handler) in &self.handlers {
            let view = handler.checkout_view();

            sqlx::query(
                r#"
                INSERT INTO checkout_views (id, path, description)
                VALUES ($1, $2, $3)
                ON CONFLICT (id) DO NOTHING
                "#,
            )
                .bind(view.id)
                .bind(view.path)
                .bind(view.description)
                .execute(&mut *tx)
                .await?;

            let inserted = sqlx::query(
                r#"
                INSERT INTO token_checkout_views (token_id, view_id)
                VALUES ($1, $2)
                ON CONFLICT (token_id) DO NOTHING
                "#,
            )
                .bind(token_id)
                .bind(view.id)
                .execute(&mut *tx)
                .await?;

            if inserted.rows_affected() == 1 {
                println!("  ✅ {} -> {} ({})", token_id, view.id, view.path);
            }
        }

        tx.commit().await?;

        // Surface tokens whose DB mapping diverges from the code default —
        // intentional after an operator edit, but worth seeing in the log.
        let overrides = sqlx::query_as::<_, (String, String)>(
            r#"SELECT token_id, view_id FROM token_checkout_views ORDER BY token_id"#,
        )
            .fetch_all(pool)
            .await?;

        for (token_id, view_id) in overrides {
            if let Some(h) = self.handlers.get(&token_id) {
                if h.checkout_view().id != view_id {
                    println!("  ⚙️  {} overridden -> {}", token_id, view_id);
                }
            } else {
                println!("  ⚠️  {} mapped to {} but no handler registered", token_id, view_id);
            }
        }

        Ok(())
    }    
}

fn decrypt_data(master_key: &[u8; 32], ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != 12 {
        return Err("Invalid nonce length: expected 12 bytes".to_string());
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| "Decryption failed (tampered data or wrong key)".to_string())
}