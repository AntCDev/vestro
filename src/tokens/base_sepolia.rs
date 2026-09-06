use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::assets::{AssetKey, AssetSpec, NETWORK_EVM};
use crate::networks::evm::EVMNetwork;
use crate::networks::{NetworkClient, NetworkRegistry};
use crate::tokens::checkout::{CheckoutContext, CheckoutView};
use crate::tokens::crypto::load_merchant_mnemonic;
use crate::tokens::evm_common::{evm_checkout_data, TokenConfig};
use crate::tokens::handler::{TokenDescriptor, TokenHandler};
use crate::tokens::invoicer::{Invoicer, PaymentDetails};
use crate::tokens::registry::TokenRegistry;

const CHAIN_ID: u64 = 84532;
#[allow(dead_code)]
const BLOCK_EXPLORER: &str = "https://sepolia.basescan.org";
#[allow(dead_code)]
const CHAIN_NAME: &str = "Base Sepolia";

pub const BASE_SEPOLIA_TOKENS: &[TokenConfig] = &[
    TokenConfig {
        id: "USDC_BASE_SEPOLIA",
        name: "USDC",
        detail: "(Base) (Sepolia)",
        info: "USDC stablecoin in the Base network's Sepolia testnet.",
        token_address: Some("0x036CbD53842c5426634e7929541eC2318f3dCF7e"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "USDT_BASE_SEPOLIA",
        name: "USDT",
        detail: "(Base) (Sepolia)",
        info: "Tether USD stablecoin in the Base network's Sepolia testnet.",
        token_address: Some("0x2203cBb29D4bA9A8aE48A3fdE90591E8572Bc09a"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "ETH_BASE_SEPOLIA",
        name: "ETH",
        detail: "(Base) (Sepolia)",
        info: "Native Ethereum coin on the Base network's Sepolia testnet.",
        token_address: None,
        decimals: 18,
        required_confirmations: 5,
    },
];

pub const CHECKOUT_VIEW: CheckoutView = CheckoutView {
    id: "evm",
    path: "/checkout/evm.html",
    description: "EVM checkout: deposit QR + vault call with optional ERC-20 approval step.",
};

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let network = match networks.evm_chain(CHAIN_ID) {
        Some(net) => net,
        None => {
            println!("  ❌ Base Sepolia (chain_id {CHAIN_ID}) not configured");
            return;
        }
    };

    // "84532" — same string the watcher puts in invoices.chain_ref.
    let chain_ref = network.chain_ref();

    for config in BASE_SEPOLIA_TOKENS {
        let key = match AssetKey::from_optional_address(NETWORK_EVM, &chain_ref, config.token_address)
        {
            Ok(k) => k,
            Err(e) => {
                println!("  ❌ {} not registered: {e}", config.id);
                continue;
            }
        };

        // EVM assets carry no extra chain-specific facts today. ERC-20 flavour
        // flags (fee-on-transfer, rebasing, permit support) would go here.
        let asset = AssetSpec::new(key, config.name, config.decimals, json!({}));

        let descriptor = TokenDescriptor::new(
            config.id,
            config.name,
            config.detail,
            config.info,
            NETWORK_EVM,
            &chain_ref,
            true, // sepolia
            asset,
        );

        registry.register(BaseSepoliaHandler {
            network: Arc::clone(&network),
            config: config.clone(),
            descriptor,
        });
    }
}

pub struct BaseSepoliaHandler {
    network: Arc<EVMNetwork>,
    config: TokenConfig,
    descriptor: TokenDescriptor,
}

impl TokenHandler for BaseSepoliaHandler {
    fn descriptor(&self) -> &TokenDescriptor {
        &self.descriptor
    }

    fn invoicer(&self) -> Option<&dyn Invoicer> {
        Some(self)
    }

    // When the EVM sweeper lands, it is one impl block and one line here:
    //
    //   impl Sweeper for BaseSepoliaHandler { … }
    //   fn sweeper(&self) -> Option<&dyn Sweeper> { Some(self) }
    //
    // If sweeping ever needs state this handler shouldn't hold (a hot key, a
    // nonce manager), make it a separate struct stored as
    // `sweeper: Option<Arc<EvmSweeper>>` and return `self.sweeper.as_deref()`.
}

#[async_trait]
impl Invoicer for BaseSepoliaHandler {
    fn checkout_view(&self) -> CheckoutView {
        CHECKOUT_VIEW
    }

    async fn checkout_data(&self, pool: &PgPool, ctx: &CheckoutContext) -> Result<Value, String> {
        evm_checkout_data(&self.network, &self.config, pool, ctx).await
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

        let (deposit_address, derived_wallet_index, payment_reference) = self
            .network
            .get_derive_address(pool, merchant_id, invoice_id, &merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        let expires_at = Utc::now() + Duration::minutes(30);
        let created_block = self
            .network
            .get_current_block()
            .await
            .map_err(|e| format!("Failed to fetch current block: {e}"))? as i64;
        // network_type / chain_ref / token_address / token_decimals are already
        // on the row, written by the orchestrator from the advertised asset.
        // Note this is now the *canonical lowercase* token address — see
        // REFACTOR.md, the EVM watcher must compare case-insensitively.
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
            deposit_address,
            derived_wallet_index as i32,
            expires_at,
            payment_reference,
            self.config.required_confirmations as i16,
            created_block,
            invoice_id
        )
        .execute(pool)
        .await
        .map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: self.descriptor.chain.clone(),
            deposit_address,
            token_address: self.descriptor.asset.key.address.clone(),
            decimals: self.descriptor.asset.decimals,
            required_confirmations: self.config.required_confirmations,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!(
            "BaseSepoliaHandler::cancel_payment({invoice_id}) for token: {}",
            self.descriptor.id
        );
        Ok(())
    }
}
