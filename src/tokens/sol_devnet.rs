use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::assets::{AssetKey, AssetKind, AssetSpec, NETWORK_SOLANA};
use crate::networks::sol::SolanaNetwork;
use crate::networks::{NetworkClient, NetworkRegistry, SolanaCluster};
use crate::tokens::checkout::{CheckoutContext, CheckoutView, PresignContext};
use crate::tokens::crypto::load_merchant_mnemonic;
use crate::tokens::handler::{TokenDescriptor, TokenHandler};
use crate::tokens::invoicer::{Invoicer, PaymentDetails};
use crate::tokens::registry::TokenRegistry;
use crate::tokens::sol_common::sol_checkout_data;

pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
/// Property of the asset, not of any route: it is what decides the ATA address
/// for a given (owner, mint, program) triple. Every consumer of the asset —
/// watcher, sweeper, ledger — needs it, so it goes in `asset_params`.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub info: &'static str,
    pub token_address: Option<&'static str>, // None for native SOL
    pub token_program: Option<&'static str>, // None iff token_address is None
    pub decimals: u8,
    pub required_confirmations: i32,
}

pub const DEVNET_TOKENS: &[TokenConfig] = &[
    TokenConfig {
        id: "USDC_DEVNET",
        name: "USDC",
        detail: "(SOL) (Devnet)",
        info: "USDC stablecoin on the Solana Devnet.",
        token_address: Some("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU"),
        token_program: Some(TOKEN_PROGRAM_ID),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "SOL_DEVNET",
        name: "SOL",
        detail: "(SOL) (Devnet)",
        info: "Native Solana coin on the Devnet.",
        token_address: None,
        token_program: None,
        decimals: 9,
        required_confirmations: 5,
    },
];

pub const CHECKOUT_VIEW: CheckoutView = CheckoutView {
    id: "sol",
    path: "/checkout/sol.html",
    description: "Solana checkout: owner-address QR + Solana Pay transfer with reference.",
};

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let network = match networks.sol_cluster(SolanaCluster::Devnet) {
        Some(net) => net,
        None => {
            println!("   ❌ Solana Devnet not configured");
            return;
        }
    };

    // Single source of truth for the chain string: whatever the network client
    // uses in `invoices.chain_ref` is what the descriptor and the asset use.
    let chain_ref = network.chain_ref();

    for config in DEVNET_TOKENS {
        let asset = match build_asset(&chain_ref, config) {
            Ok(a) => a,
            Err(e) => {
                // A malformed mint is a config bug. Skip the token rather than
                // registering something that would poison the asset table.
                println!("   ❌ {} not registered: {e}", config.id);
                continue;
            }
        };

        let descriptor = TokenDescriptor::new(
            config.id,
            config.name,
            config.detail,
            config.info,
            NETWORK_SOLANA,
            &chain_ref,
            true, // devnet
            asset,
        )
        .experimental();

        registry.register(DevnetHandler {
            network: Arc::clone(&network),
            config: config.clone(),
            descriptor,
        });
    }
}

fn build_asset(chain_ref: &str, config: &TokenConfig) -> Result<AssetSpec, String> {
    let key = AssetKey::from_optional_address(NETWORK_SOLANA, chain_ref, config.token_address)?;

    let params = match key.kind {
        AssetKind::Native => json!({}),
        AssetKind::Contract => {
            // For a mint, the program is what decides the ATA, so a missing one
            // is a config bug, not a default. Caught here at boot instead of at
            // invoice time.
            let program = config
                .token_program
                .filter(|p| !p.is_empty())
                .ok_or_else(|| format!("{} has a mint configured but no token_program", config.id))?;
            json!({
                "token_program": program,
                "ata_program": ASSOCIATED_TOKEN_PROGRAM_ID,
            })
        }
    };

    Ok(AssetSpec::new(key, config.name, config.decimals, params))
}

pub struct DevnetHandler {
    network: Arc<SolanaNetwork>,
    config: TokenConfig,
    descriptor: TokenDescriptor,
}

impl TokenHandler for DevnetHandler {
    fn descriptor(&self) -> &TokenDescriptor {
        &self.descriptor
    }

    fn invoicer(&self) -> Option<&dyn Invoicer> {
        Some(self)
    }

    // No `sweeper()` override: devnet funds are not worth moving, and nothing
    // implements Sweeper yet. The handler still advertises its asset, so the
    // ledger can denominate devnet balances and show them as non-withdrawable.
}

#[async_trait]
impl Invoicer for DevnetHandler {
    fn checkout_view(&self) -> CheckoutView {
        CHECKOUT_VIEW
    }

    async fn checkout_data(&self, _pool: &PgPool, ctx: &CheckoutContext) -> Result<Value, String> {
        sol_checkout_data(&self.network, self.config.name, self.config.decimals, ctx)
    }

    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        _amount: rust_decimal::Decimal,
        _token_id: &str,
    ) -> Result<PaymentDetails, String> {
        let merchant_mnemonic = load_merchant_mnemonic(pool, merchant_id).await?;

        // token_address / token_program / token_decimals / network_type /
        // chain_ref were written by the orchestrator from the advertised asset
        // *before* this call, which is what get_derive_address reads back off
        // the row to decide which program the ATA is derived under. The invoice
        // is not yet visible to the watcher — wallet_address is still '' — so a
        // crash anywhere in here leaves a row that is never polled and expires.
        let mint = self.descriptor.asset.key.address.clone();

        // The reference path is dead without this row, so surface it at creation
        // time instead of letting the watcher log about it once per tick forever.
        let merchant_wallet = sqlx::query_scalar!(
            r#"
            SELECT address FROM merchant_wallets
            WHERE merchant_id = $1 AND network_type = 'solana'
            "#,
            merchant_id
        )
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("Failed to look up merchant wallet: {e}"))?;

        if merchant_wallet.is_none() {
            eprintln!(
                "merchant {merchant_id} has no merchant_wallets row for 'solana'; invoice \
                 {invoice_id} will only be payable via the direct/QR path"
            );
        }

        let (deposit_address, derived_wallet_index, payment_reference) = self
            .network
            .get_derive_address(pool, merchant_id, invoice_id, &merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        // TODO: merchant-configurable rather than a fixed half hour.
        let expires_at = Utc::now() + Duration::minutes(30);

        // Deliberately the FINALIZED slot, not the processed/confirmed tip.
        // `created_block` is a floor: anything below it predates the invoice. A
        // tip reading can sit ahead of where the payer's transaction lands,
        // which would throw away a real payment. Finalized is always behind, so
        // erring here costs a few extra signatures to scan.
        let current_slot = self
            .network
            .get_finalized_block()
            .await
            .map_err(|e| format!("Failed to fetch finalized slot: {e}"))? as i64;

        // Setting wallet_address is what makes the invoice visible to the watcher.
        sqlx::query!(
            r#"
            UPDATE invoices
            SET wallet_address = $1,
                wallet_index = $2,
                expires_at = $3,
                payment_reference = $4,
                required_confirmations = $5,
                created_block = $6,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = $7
            "#,
            deposit_address.as_str(),
            derived_wallet_index as i32,
            expires_at,
            payment_reference.as_deref(),
            self.config.required_confirmations as i16,
            current_slot,
            invoice_id
        )
        .execute(pool)
        .await
        .map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: self.descriptor.chain.clone(),
            deposit_address,
            token_address: mint,
            decimals: self.descriptor.asset.decimals,
            required_confirmations: self.config.required_confirmations,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!(
            "DevnetHandler::cancel_payment({invoice_id}) for token: {}",
            self.descriptor.id
        );
        Ok(())
    }

    async fn presign_data(
        &self,
        _pool: &PgPool,
        _ctx: &PresignContext,
    ) -> Result<Value, String> {
        let bh = self.network.get_recent_blockhash().await?;
        Ok(json!({
            "blockhash": bh.blockhash,
            "last_valid_block_height": bh.last_valid_block_height,
            "commitment": crate::networks::sol::BLOCKHASH_COMMITMENT,
        }))
    }
}
