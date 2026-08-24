use crate::networks::sol::SolanaNetwork;
use crate::networks::{NetworkClient, NetworkRegistry, SolanaCluster};
use crate::tokens::{decrypt_data, CheckoutContext, CheckoutView, PaymentDetails, PresignContext, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use serde_json::{json, Value};
use uuid::Uuid;
use crate::tokens::sol_common::sol_checkout_data;

pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub info: &'static str,
    pub token_address: Option<&'static str>,  // None for native SOL
    pub token_program: Option<&'static str>,  // None iff token_address is None
    pub decimals: u8,
    pub required_confirmations: i32,
}

// Token configuration list for Solana Devnet
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
    // TokenConfig {
    //     id: "USDT_DEVNET",
    //     name: "USDT",
    //     detail: "(SOL) (Devnet)",
    //     info: "Tether USD stablecoin on the Solana Devnet.",
    //     token_address: Some("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
    //     // Tether's USDT is a legacy SPL Token mint.
    //     token_program: Some(TOKEN_PROGRAM_ID),
    //     decimals: 6,
    //     required_confirmations: 5,
    // },
    // TokenConfig {
    //     id: "USDS_DEVNET",
    //     name: "USDS",
    //     detail: "(SOL) (Devnet)",
    //     info: "USDS stablecoin on the Solana Devnet.",
    //     token_address: Some("FILL_ME_IN_USDS_MINT_ADDRESS"),
    //     // VERIFY ME: USDS (Sky/Maker) on Solana is, to my knowledge, a Token-2022
    //     // mint — this is exactly the case the reviewer was warning about. Confirm by
    //     // fetching the mint account and checking its `owner` field before going live:
    //     //   solana account <USDS_MINT> --output json | jq -r .account.owner
    //     // Expect TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb for Token-2022.
    //     token_program: Some(TOKEN_2022_PROGRAM_ID),
    //     decimals: 6,
    //     required_confirmations: 5,
    // },
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

    for config in DEVNET_TOKENS {
        let handler = DevnetHandler {
            network: Arc::clone(&network),
            config: config.clone(),
        };

        // Confirmation detail is stored in config but excluded from the registration call
        registry.register_token(
            config.id,
            config.name,
            config.detail,
            config.info,
            handler,
        );
    }
}

pub struct DevnetHandler {
    network: Arc<SolanaNetwork>,
    config: TokenConfig,
}

#[async_trait]
impl TokenHandler for DevnetHandler {
    fn token_id(&self) -> &str {
        self.config.id
    }

    fn checkout_view(&self) -> CheckoutView {
        CHECKOUT_VIEW
    }

    async fn checkout_data(
        &self,
        _pool: &PgPool,
        ctx: &CheckoutContext,
    ) -> Result<Value, String> {
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
        let master_key_hex = std::env::var("MASTER_KEY")
            .map_err(|_| "MASTER_KEY environment variable not set".to_string())?;

        let master_key_vec = hex::decode(&master_key_hex)
            .map_err(|e| format!("Failed to decode MASTER_KEY hex: {e}"))?;

        let master_key: &[u8; 32] = master_key_vec
            .as_slice()
            .try_into()
            .map_err(|_| "MASTER_KEY must be exactly 32 bytes (64 hex characters)".to_string())?;

        let key_material = sqlx::query!(
			r#"
			SELECT encrypted_secret, encryption_nonce
			FROM merchant_key_material
			WHERE merchant_id = $1 AND key_family = 'bip39'
			"#,
			merchant_id
		)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to fetch key material for merchant {merchant_id}: {e}"))?;

        let decrypted_bytes = decrypt_data(
            master_key,
            &key_material.encrypted_secret,
            &key_material.encryption_nonce,
        )?;

        let merchant_mnemonic = String::from_utf8(decrypted_bytes)
            .map_err(|e| format!("Invalid UTF-8 sequence in decrypted mnemonic: {e}"))?;

        // Normalise here rather than defending against "0"/"" in five places
        // downstream. NULL is the only representation of "native" the DB should hold.
        let token_address = self
            .config
            .token_address
            .as_ref()
            .map(ToString::to_string)
            .filter(|m| !m.is_empty() && m != "0");

        // Native SOL carries no token program. For a mint, the program is what
        // decides the ATA, so a missing one is a config bug, not a default.
        let token_program = match &token_address {
            None => None,
            Some(_) => match self.config.token_program {
                Some(p) if !p.is_empty() => Some(p.to_string()),
                _ => {
                    return Err(format!(
                        "token {} has a mint configured but no token_program; refusing to create \
						 invoice {invoice_id} with an unguessable ATA",
                        self.config.id
                    ))
                }
            },
        };

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

        // Written before derivation: get_derive_address reads these two columns back
        // off the row to decide which program the ATA is derived under. The invoice
        // is not yet visible to the watcher — its wallet_address is still '' — so a
        // crash between this statement and the one below leaves a row that is never
        // polled and simply expires.
        //
        // Bound by reference (`as_deref`) rather than by value: both values are read
        // again when PaymentDetails is built, and the macro takes ownership of what
        // it is handed.
        sqlx::query!(
			r#"
			UPDATE invoices
			SET token_address = $1,
				token_program = $2,
				updated_at = CURRENT_TIMESTAMP
			WHERE id = $3
			"#,
			token_address.as_deref(),
			token_program.as_deref(),
			invoice_id
		)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to set token fields on invoice {invoice_id}: {e}"))?;

        let (deposit_address, derived_wallet_index, payment_reference) = self
            .network
            .get_derive_address(pool, merchant_id, invoice_id, &merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        // TODO: merchant-configurable rather than a fixed half hour.
        let expires_at = Utc::now() + Duration::minutes(30);

        // Deliberately the FINALIZED slot, not the processed/confirmed tip.
        // `created_block` is used as a floor: any transaction below it is discarded
        // as predating the invoice. A tip reading can sit ahead of where the payer's
        // transaction lands, which would throw away a real payment. Finalized is
        // always behind, so erring here costs a few extra signatures to scan.
        let current_slot = self
            .network
            .get_finalized_block()
            .await
            .map_err(|e| format!("Failed to fetch finalized slot: {e}"))? as i64;

        // Must match the watcher's constant exactly, and must be the same string
        // merchant_wallets and merchant_network_indices use.
        let network_type = "solana";
        let chain_ref = self.network.chain_ref();

        // token_address / token_program are already committed above; setting
        // wallet_address here is what makes the invoice visible to the watcher.
        sqlx::query!(
			r#"
			UPDATE invoices
			SET wallet_address = $1,
				wallet_index = $2,
				expires_at = $3,
				payment_reference = $4,
				token_decimals = $5,
				required_confirmations = $6,
				network_type = $7,
				chain_ref = $8,
				created_block = $9,
				updated_at = CURRENT_TIMESTAMP
			WHERE id = $10
			"#,
			deposit_address.as_str(),
			derived_wallet_index as i32,
			expires_at,
			payment_reference.as_deref(),
			self.config.decimals as i16,
			self.config.required_confirmations as i16,
			network_type,
			chain_ref.as_str(),
			current_slot,
			invoice_id
		)
            .execute(pool)
            .await
            .map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: chain_ref,
            deposit_address,
            token_address,
            decimals: self.config.decimals,
            required_confirmations: self.config.required_confirmations,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }
    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!("DevnetHandler::cancel_payment({invoice_id}) for token: {}", self.config.id);
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