use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::assets::{AssetKey, AssetSpec, ChainRef, NETWORK_EVM};
use crate::networks::evm::EVMNetwork;
use crate::networks::{NetworkClient, NetworkRegistry};
use crate::tokens::checkout::{CheckoutContext, CheckoutView};
use crate::keys::store::load_merchant_seed;
use crate::ledgerer::AddressKind;
use crate::networks::transfers::{SourceAccount, TransferAmount};
use crate::tokens::evm_common::{evm_checkout_data, TokenConfig};
use crate::tokens::handler::{TokenDescriptor, TokenHandler};
use crate::tokens::invoicer::{Invoicer, PaymentDetails};
use crate::tokens::registry::TokenRegistry;
use crate::tokens::Sweeper;
use crate::tokens::sweeper::{SweepDraft, TransferPlan};

const CHAIN_ID: u64 = 11155111;
#[allow(dead_code)]
const BLOCK_EXPLORER: &str = "https://sepolia.etherscan.io";
#[allow(dead_code)]
const CHAIN_NAME: &str = "Sepolia";

// Token configuration list for Ethereum Sepolia
pub const SEPOLIA_TOKENS: &[TokenConfig] = &[
    TokenConfig {
        id: "USDC_SEPOLIA",
        name: "USDC",
        detail: "(Sepolia)",
        info: "USDC stablecoin on the Ethereum Sepolia testnet.",
        token_address: Some("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "USDT_SEPOLIA",
        name: "USDT",
        detail: "(Sepolia)",
        info: "Tether USD stablecoin on the Ethereum Sepolia testnet.",
        token_address: Some("0x8d412FD0bc5d826615065B931171Eed10F5AF266"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "DAI_SEPOLIA",
        name: "DAI",
        detail: "(Sepolia)",
        info: "DAI stablecoin on the Ethereum Sepolia testnet.",
        token_address: Some("0xFF34B3d4Aee8ddCd6F9AFFFB6Fe49bD371b8a357"),
        decimals: 18,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "ETH_SEPOLIA",
        name: "ETH",
        detail: "(Sepolia)",
        info: "Native Ethereum coin on the Sepolia testnet.",
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
            println!("  ❌ Sepolia (chain_id {CHAIN_ID}) not configured");
            return;
        }
    };

    // "11155111" — same string the watcher puts in invoices.chain_ref.
    let chain_ref = network.chain_ref();

    for config in SEPOLIA_TOKENS {
        let chain = ChainRef::new(NETWORK_EVM, &chain_ref); // Or construct ChainRef appropriately

        let key = match AssetKey::from_optional_address(chain, config.token_address.as_deref()) {
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

        registry.register(SepoliaHandler {
            network: Arc::clone(&network),
            config: config.clone(),
            descriptor,
        });
    }
}

pub struct SepoliaHandler {
    network: Arc<EVMNetwork>,
    config: TokenConfig,
    descriptor: TokenDescriptor,
}

impl TokenHandler for SepoliaHandler {
    fn descriptor(&self) -> &TokenDescriptor {
        &self.descriptor
    }

    fn invoicer(&self) -> Option<&dyn Invoicer> {
        Some(self)
    }

    // When the EVM sweeper lands, it is one impl block and one line here:
    //
    //   impl Sweeper for SepoliaHandler { … }
    //   fn sweeper(&self) -> Option<&dyn Sweeper> { Some(self) }
    //
    // If sweeping ever needs state this handler shouldn't hold (a hot key, a
    // nonce manager), make it a separate struct stored as
    // `sweeper: Option<Arc<EvmSweeper>>` and return `self.sweeper.as_deref()`.
    fn sweeper(&self) -> Option<&dyn Sweeper> { Some(self) }
}

#[async_trait]
impl Sweeper for SepoliaHandler {
    async fn plan(&self, _pool: &PgPool, d: &SweepDraft) -> Result<TransferPlan, String> {
        match d.custody_kind {
            AddressKind::DepositAddress => Ok(TransferPlan {
                from: SourceAccount {
                    address: d.custody_address.clone(),
                    kind: d.custody_kind,
                    authority: d.authority,
                },
                to: d.destination.clone(),
                amount: TransferAmount::Max,
                fee_payer: None,
                params: json!({}),
            }),

            AddressKind::Vault => {
                let vault = self
                    .network
                    .vault_address()
                    .map(str::to_lowercase)
                    .ok_or("vault sweep but no contract_address configured for this chain")?;

                if d.custody_address.to_lowercase() != vault {
                    return Err(format!(
                        "sweep row custody_address {} is not the configured vault {vault}",
                        d.custody_address
                    ));
                }
                // sweep(token) transfers to msg.sender. A destination that isn't
                // the signer cannot be honoured by this contract.
                if d.destination.to_lowercase() != d.authority_address.to_lowercase() {
                    return Err(format!(
                        "vault sweep pays its caller: destination {} != authority {}",
                        d.destination, d.authority_address
                    ));
                }

                Ok(TransferPlan {
                    from: SourceAccount {
                        address: vault.clone(),
                        kind: AddressKind::Vault,
                        authority: d.authority,
                    },
                    to: d.destination.clone(),
                    // The queue row's amount is the confirmed figure. Max only as an
                    // operator escape hatch — it takes everything, confirmed or not.
                    amount: d.amount,
                    fee_payer: None,
                    params: json!({
                        "mechanism": "vault_sweep",
                        "vault": vault,
                        "token": self.descriptor.asset.key.address,
                        "authority_address": d.authority_address.to_lowercase(),
                    }),
                })
            }

            k => Err(format!("cannot sweep from {k:?}")),
        }
    }
}

#[async_trait]
impl Invoicer for SepoliaHandler {
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
        let merchant_mnemonic = load_merchant_seed(pool, merchant_id).await?;

        let derived = self
            .network
            .next_deposit_address(pool, merchant_id, invoice_id, &merchant_mnemonic)
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
            SET wallet_address = $1, wallet_index = $2, wallet_role = $3,
                wallet_path = $4, scheme_version = $5,
                expires_at = $6, payment_reference = $7,
                required_confirmations = $8, created_block = $9,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = $10
            "#,
            derived.address, derived.index as i32, derived.role.as_i16(),
            derived.path, derived.scheme_version,
            expires_at, derived.reference,
            self.config.required_confirmations as i16,
            (created_block - 2).max(0),
            invoice_id
        )
            .execute(pool)
            .await
            .map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: self.descriptor.chain.clone(),
            deposit_address: derived.address,
            token_address: self.descriptor.asset.key.address.clone(),
            decimals: self.descriptor.asset.decimals,
            required_confirmations: self.config.required_confirmations,
            wallet_index: derived.index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!(
            "SepoliaHandler::cancel_payment({invoice_id}) for token: {}",
            self.descriptor.id
        );
        Ok(())
    }
}