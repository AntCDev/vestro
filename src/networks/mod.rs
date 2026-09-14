use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use serde_json::{json, Map, Value};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce, Key
};
use argon2::{
    password_hash::{PasswordHasher, PasswordVerifier},
};
use sha2::{Digest};
use crate::keys::crypto::decrypt_data;
use crate::keys::derivation::{DerivationScheme, DerivedAddress, KeyRole, WalletSpec, MAIN_WALLET};
use crate::networks::transfers::{SignedTransfer, TransferRequest, TransferStatus};

pub mod evm;
pub mod sol;
pub mod esplora;
pub mod transfers;
pub mod outbound;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SolanaCluster {
    MainnetBeta,
    Testnet,
    Devnet,
}

impl SolanaCluster {
    fn env_prefix(&self) -> &'static str {
        match self {
            SolanaCluster::MainnetBeta => "SOLANA_MAINNET_RPC_URLS",
            SolanaCluster::Testnet => "SOLANA_TESTNET_RPC_URLS",
            SolanaCluster::Devnet => "SOLANA_DEVNET_RPC_URLS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet4,
    Signet,
}

impl BitcoinNetwork {
    fn env_prefix(&self) -> &'static str {
        match self {
            BitcoinNetwork::Mainnet => "ESPLORA_MAINNET_URLS",
            BitcoinNetwork::Testnet4 => "ESPLORA_TESTNET4_URLS",
            BitcoinNetwork::Signet => "ESPLORA_SIGNET_URLS",
        }
    }
}

#[derive(Clone)]
pub struct NetworkRegistry {
    pub(crate) evm: HashMap<u64, Arc<evm::EVMNetwork>>,
    pub(crate) sol: HashMap<SolanaCluster, Arc<sol::SolanaNetwork>>,
    pub(crate) esplora: HashMap<BitcoinNetwork, Arc<esplora::EsploraNetwork>>,
}

impl NetworkRegistry {

    /// One client per *configured* family (evm/solana/esplora), used for
    /// address derivation where any chain in the family yields the same
    /// address format. Families with zero configured networks are omitted.
    pub fn representative_clients(&self) -> Vec<Arc<dyn NetworkClient>> {
        let mut out: Vec<Arc<dyn NetworkClient>> = Vec::new();
        if let Some(net) = self.evm.values().next() {
            out.push(net.clone() as Arc<dyn NetworkClient>);
        }
        if let Some(net) = self.sol.values().next() {
            out.push(net.clone() as Arc<dyn NetworkClient>);
        }
        if let Some(net) = self.esplora.values().next() {
            out.push(net.clone() as Arc<dyn NetworkClient>);
        }
        out
    }    
    pub fn from_env() -> Self {
        println!("\n🌐 Initializing Network Registry...");

        fn fetch_and_log_urls(name: &str, key: &str) -> Option<Vec<String>> {
            let urls: Vec<String> = match std::env::var(key) {
                Ok(raw) => raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                Err(_) => Vec::new(),
            };

            if urls.is_empty() {
                println!("  {} Network ❌ No valid RPC_URL found", name);
                None
            } else {
                let count = urls.len();
                let redundancy = if count > 1 { ", enabling redundancy" } else { "" };
                println!("  {} Network ✅ {} RPC_URL Found{}", name, count, redundancy);
                Some(urls)
            }
        }

        // Helper to fetch single optional strings (like contract addresses)
        fn fetch_optional_env(key: &str) -> Option<String> {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }

        // ---- EVM ----
        let mut evm = HashMap::new();
        let evm_configs = [
            (1, "Ethereum", "ETH_MAINNET_RPC_URLS", "ETH_MAINNET_CONTRACT_ADDRESS"),
            (8453, "Base", "BASE_MAINNET_RPC_URLS", "BASE_MAINNET_CONTRACT_ADDRESS"),
            (137, "Polygon", "POLYGON_MAINNET_RPC_URLS", "POLYGON_MAINNET_CONTRACT_ADDRESS"),
            (84532, "Base Sepolia", "BASE_SEPOLIA_RPC_URLS", "BASE_SEPOLIA_CONTRACT_ADDRESS"),
            (11155111, "Sepolia", "SEPOLIA_RPC_URLS", "SEPOLIA_CONTRACT_ADDRESS"),
        ];

        for (chain_id, name, rpc_key, contract_key) in evm_configs {
            if let Some(urls) = fetch_and_log_urls(name, rpc_key) {
                let contract_address = fetch_optional_env(contract_key);
                match &contract_address {
                    Some(addr) => println!("    └─ Contract Address: {addr}"),
                    None => println!("    └─ Contract Address: ⚠️ None configured"),
                }
                evm.insert(
                    chain_id,
                    Arc::new(evm::EVMNetwork::new(chain_id, name, urls, contract_address)),
                );
            }
        }

        // ---- Solana ----
        let mut sol = HashMap::new();
        for (cluster, name) in [
            (SolanaCluster::MainnetBeta, "Solana Mainnet"),
            (SolanaCluster::Testnet, "Solana Testnet"),
            (SolanaCluster::Devnet, "Solana Devnet"),
        ] {
            if let Some(urls) = fetch_and_log_urls(name, cluster.env_prefix()) {
                sol.insert(cluster, Arc::new(sol::SolanaNetwork::new(cluster, urls)));
            }
        }

        // ---- Esplora (Bitcoin) ----
        let mut esplora = HashMap::new();
        for (network_type, name) in [
            (BitcoinNetwork::Mainnet, "Bitcoin Mainnet"),
            (BitcoinNetwork::Testnet4, "Bitcoin Testnet4"),
            (BitcoinNetwork::Signet, "Bitcoin Signet"),
        ] {
            if let Some(urls) = fetch_and_log_urls(name, network_type.env_prefix()) {
                esplora.insert(
                    network_type,
                    Arc::new(esplora::EsploraNetwork::new(network_type, urls)),
                );
            }
        }

        Self { evm, sol, esplora }
    }

    /// Spawn every configured watcher. Call this LAST in main, after the token
    /// registry is built and after `sync_assets` has run — a watcher must never
    /// be able to credit a payment against an asset row that does not exist yet.
    pub fn spin_up_all(&self, pool: &PgPool) {
        println!("\n👁️  Spinning up network watchers...");

        for (chain_id, network) in &self.evm {
            let (network, pool, chain_id) = (network.clone(), pool.clone(), *chain_id);
            tokio::spawn(async move {
                outbound::spawn(pool.clone(), network.clone() as Arc<dyn NetworkClient>);
                if let Err(err) = network.spin_up(&pool).await {
                    eprintln!("❌ EVM network (chain {chain_id}) spin_up failed: {err}");
                }
            });
        }

        for (cluster, network) in &self.sol {
            let (network, pool, cluster) = (network.clone(), pool.clone(), *cluster);
            tokio::spawn(async move {
                outbound::spawn(pool.clone(), network.clone() as Arc<dyn NetworkClient>);
                if let Err(err) = network.spin_up(&pool).await {
                    eprintln!("❌ Solana network ({cluster:?}) spin_up failed: {err}");
                }
            });
        }

        for (bitcoin_network, network) in &self.esplora {
            let (network, pool, bitcoin_network) =
                (network.clone(), pool.clone(), *bitcoin_network);
            tokio::spawn(async move {
                outbound::spawn(pool.clone(), network.clone() as Arc<dyn NetworkClient>);
                if let Err(err) = network.spin_up(&pool).await {
                    eprintln!("❌ Bitcoin network ({bitcoin_network:?}) spin_up failed: {err}");
                }
            });
        }
    }    
    
    pub fn evm_chain(&self, chain_id: u64) -> Option<Arc<evm::EVMNetwork>> {
        self.evm.get(&chain_id).cloned()
    }

    pub fn sol_cluster(&self, cluster: SolanaCluster) -> Option<Arc<sol::SolanaNetwork>> {
        self.sol.get(&cluster).cloned()
    }

    pub fn esplora_network(&self, network: BitcoinNetwork) -> Option<Arc<esplora::EsploraNetwork>> {
        self.esplora.get(&network).cloned()
    }
}

#[derive(Clone, Debug)]
pub struct PaymentWatch {
    pub invoice_id: Uuid,
    pub address: String,
    pub token_address: Option<String>,
    pub decimals: u8,
    pub target_amount: u128,
    pub required_confirmations: u32,
    pub from_block: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Amount(pub u128);


#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GasModel {
    /// The signing address must itself hold native currency. A sweep is two
    /// transactions and the first one must confirm before the second is built.
    SelfFunded,
    /// A third party signs as fee payer alongside the authority, in the same
    /// transaction. No advance, no nonce race, no stranded dust at the deposit
    /// address — and no partial state if the sweep fails.
    FeePayer,
    /// Fees come out of the inputs being spent. Nothing to fund.
    InputFunded,
}

#[async_trait]
pub trait NetworkClient: Send + Sync {
    /// Canonical family string. Must equal one of the constants in
    /// `crate::assets`, and must be the same string this network's watcher
    /// writes to `invoices.network_type` / `merchant_wallets.network_type`.
    fn network_type(&self) -> &'static str;

    /// Canonical chain within that family: "8453", "84532", "devnet",
    /// "mainnet", "testnet4". A String, not an enum or an integer, precisely
    /// so an EVM chain id and a Solana cluster name share one column.
    fn chain_ref(&self) -> String;
    fn gas_model(&self) -> GasModel;

    /// The scheme this family derives under. Asserted against the DB at boot.
    fn derivation_scheme(&self) -> DerivationScheme;

    /// Named wallets every merchant needs on this family. Default is the main
    /// wallet only — UTXO chains pay fees from the inputs they spend and have
    /// no feeder. EVM and Solana override to add one.
    fn required_wallets(&self) -> &'static [WalletSpec] {
        &[MAIN_WALLET]
    }

    /// Canonical storage form. EVM lowercases; Solana and Bitcoin must not.
    /// Replaces the `if network_type == "evm"` check that was living at the
    /// registration call site.
    fn canonicalize_address(&self, address: &str) -> String {
        address.to_string()
    }

    /// Pure, no I/O. The single derivation entry point — every wallet on every
    /// network, deposit or operational, comes out of here. Returns an already
    /// canonicalized address.
    fn derive(&self, mnemonic: &str, role: KeyRole, index: u32)
              -> Result<DerivedAddress, String>;

    /// Allocates the next deposit index and returns the full derived record,
    /// including the on-chain reference for this invoice.
    async fn next_deposit_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<DerivedAddress, String>;

    fn validate_address(&self, address: &str) -> bool;
    async fn get_native_balance(&self, address: &str) -> Result<Amount, String>;
    async fn get_token_balance(
        &self,
        token_address: &str,
        address: &str,
        decimals: u8,
    ) -> Result<Amount, String>;
    async fn get_current_block(&self) -> Result<u64, String>;

    // ── Outbound ──────────────────────────────────────────────────────────
    /// Derive keys, resolve `Max`, allocate sequence, sign. No broadcast.
    /// `pool` is for sequence allocation only (chain_nonces).
    async fn build_and_sign(&self, pool: &PgPool, mnemonic: &str, req: &TransferRequest)
                            -> Result<SignedTransfer, String>;
    /// Idempotent, repeatable. "already known" is success.
    async fn broadcast(&self, signed: &SignedTransfer) -> Result<(), String>;
    async fn transfer_status(&self, signed: &SignedTransfer) -> Result<TransferStatus, String>;
    fn outbound_poll_interval(&self) -> std::time::Duration { std::time::Duration::from_secs(10) }

    async fn spin_up(&self, pool: &PgPool) -> Result<(), String>;
}

/// Enqueues a webhook event for the merchant that owns `invoice_id`.
///
/// - Looks up the merchant's `webhook_url` and the invoice's opaque `data`
///   field in one query, joined off invoice_id (no need for callers to carry
///   merchant_id around separately).
/// - If the merchant hasn't configured a webhook_url, this is a silent no-op —
///   there's nowhere to deliver to yet, and no point enqueueing a row that'll
///   never be dispatched.
/// - `dedupe_suffix` should uniquely identify the underlying occurrence
///   (payment_id, tx_hash, etc.) — it gets combined with event_type to form
///   the dedupe_key, so calling this twice for the same real-world event is
///   always safe (ON CONFLICT DO NOTHING against webhook_events_dedupe_uniq).
/// - `fields` are the event-specific payload fields (TxHash, BlockNumber, ...);
///   this function adds `Data` (the merchant's opaque invoice data) and
///   `InvoiceId` on top.
/// - Takes a `&mut Transaction` deliberately: call sites should insert/update
///   whatever mutated the domain state and enqueue the webhook in the same
///   transaction, so a rollback can never leave a webhook enqueued for a
///   change that didn't happen (or vice versa).
async fn enqueue_webhook(
    tx: &mut Transaction<'_, Postgres>,
    invoice_id: Uuid,
    event_type: &str,
    dedupe_suffix: &str,
    mut fields: Map<String, Value>,
) -> Result<(), String> {
    let row: (Uuid, Option<String>, Option<String>) = sqlx::query_as(
        r#"
        SELECT m.id, m.webhook_url, i.data
          FROM invoices i
          JOIN merchants m ON m.id = i.merchant_id
         WHERE i.id = $1
        "#,
    )
        .bind(invoice_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| format!("enqueue_webhook merchant lookup: {e}"))?;

    let (merchant_id, webhook_url, invoice_data) = row;

    let Some(url) = webhook_url else {
        // Merchant has no webhook configured — nothing to enqueue.
        return Ok(());
    };

    // The merchant-supplied opaque payload from invoice creation. Left as a
    // plain string on purpose: could be "25", could be `{"Amount":50}`, we
    // don't parse it, the merchant does.
    fields.insert(
        "Data".to_string(),
        invoice_data.map(Value::String).unwrap_or(Value::Null),
    );
    fields.insert("InvoiceId".to_string(), json!(invoice_id));

    let event_data = Value::Object(fields);
    let dedupe_key = format!("{event_type}:{dedupe_suffix}");

    sqlx::query(
        r#"
        INSERT INTO webhook_events (merchant_id, url, event_type, event_data, dedupe_key)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (merchant_id, dedupe_key) DO NOTHING
        "#,
    )
        .bind(merchant_id)
        .bind(url)
        .bind(event_type)
        .bind(event_data)
        .bind(dedupe_key)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("enqueue_webhook insert: {e}"))?;

    Ok(())
}