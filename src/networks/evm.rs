use super::{enqueue_webhook, Amount, NetworkClient, PaymentWatch, decrypt_data, GasModel};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;
use serde_json::{json, Map, Value};
use sqlx::{Postgres, Row, Transaction};

use bip32::{DerivationPath, PrivateKey, XPrv};
use bip39::Mnemonic;
use sha2::Sha256;
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};
use sqlx::PgPool;
use std::collections::{HashMap, VecDeque};
use rust_decimal::Decimal;

use chrono::{DateTime, Utc};
use crate::keys::derivation::{DerivationScheme, DerivedAddress, KeyRole, WalletSpec, GAS_FEEDER_WALLET, MAIN_WALLET, SCHEME_VERSION};
use crate::keys::store::assert_scheme;
use crate::keys::wallets::{allocate_deposit_index, ensure_merchant_wallets};
use crate::ledgerer::{
    AddressKind, AssetKey, ChainRef, Custody, Finality, LedgerError, Ledgerer,
    ObservedInbound, ObservedTransfer, OrphanInput, PaymentPath, RecognizeInput,
};
use crate::networks::transfers::{SignedTransfer, TransferAmount, TransferRequest, TransferStatus};

// ==========================================
// ### PRIVATE RPC STRUCTS ###
// ==========================================
#[derive(Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: serde_json::Value,
    id: u32,
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<String>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}


#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Log {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
    #[serde(rename = "transactionIndex")]
    pub transaction_index: String,
    #[serde(rename = "blockHash")]
    pub block_hash: String,
    #[serde(rename = "logIndex")]
    pub log_index: String,
    pub removed: bool,
}

#[derive(Deserialize)]
struct RpcResponseLogs {
    result: Option<Vec<Log>>,
    error: Option<RpcError>,
}

struct BlockRecord {
    number: u64,
    hash: String,
}



// Providers cap how many values you can put in one topic slot. 100 is safe; chunk the watched-address list.
const MAX_TOPIC_ADDRESSES: usize = 100;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanRange {
    from: i64,
    to: Option<i64>,
}

impl ScanRange {
    fn contains(&self, n: i64) -> bool {
        n >= self.from && self.to.map_or(true, |t| n <= t)
    }
    fn end_or(&self, ceiling: i64) -> i64 {
        self.to.unwrap_or(ceiling)
    }
}

/// Sort + coalesce. Adjacent ranges (`prev.to + 1 == r.from`) merge too, so we
/// never pay for a jump to save a single block. An open-ended range swallows
/// everything after it.
fn merge_ranges(mut ranges: Vec<ScanRange>) -> Vec<ScanRange> {
    ranges.sort_by_key(|r| (r.from, r.to.unwrap_or(i64::MAX)));
    let mut out: Vec<ScanRange> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(prev) if prev.to.map_or(true, |t| r.from <= t + 1) => {
                prev.to = match (prev.to, r.to) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None, // either side open => merged range is open
                };
            }
            _ => out.push(r),
        }
    }
    out
}

/// Given the block we'd *like* to scan next, return the block we should
/// actually scan: `want` if it's inside a live range, otherwise the start of
/// the next range after it, or `None` if there's nothing left to watch at all.
/// `plan` must be sorted ascending (merge_ranges guarantees that).
fn plan_next_block(plan: &[ScanRange], want: i64) -> Option<i64> {
    for r in plan {
        if r.contains(want) {
            return Some(want);
        }
        if r.from > want {
            return Some(r.from);
        }
    }
    None
}

/// Last block of the range `n` currently sits in — the point at which we should
/// stop walking forward and consider jumping.
fn plan_range_end(plan: &[ScanRange], n: i64, ceiling: i64) -> i64 {
    plan.iter()
        .find(|r| r.contains(n))
        .map(|r| r.end_or(ceiling))
        .unwrap_or(ceiling)
}


// ─────────────────────────────────────────────────────────────────────────────
// Tunables. All of these become per-merchant / per-chain config later.
// ─────────────────────────────────────────────────────────────────────────────
const FINAL_CONFIRMATIONS: i64 = 48;    // -> 'system_confirmed', we stop polling it
const POLL_INTERVAL_SECS: u64 = 12;     // ~1 block on mainnet; per-chain later
const MAX_BLOCKS_PER_TICK: u64 = 250;   // catch-up throttle so a long outage doesn't nuke the RPC
const MAX_REORG_DEPTH: u64 = 64;        // how far back we're willing to unwind

const NETWORK_TYPE: &str = "evm";
const SCAN_SCOPE_ADDRESSES: &str = "addresses";
const SCAN_SCOPE_LOGS: &str = "logs";

/// eth_getLogs range per request. Most providers cap somewhere between 1k and
/// 10k blocks (and/or 10k results); 1000 is safe basically everywhere.
/// TODO: per-provider config, and halve-and-retry on "response too large" errors.
const MAX_LOG_BLOCK_RANGE: u64 = 10;



/// keccak256("Payment(address,address,bytes16,address,uint256,uint256,uint256)")
/// Fallback if TOPIC_0 isn't set in the environment.
const DEFAULT_PAYMENT_TOPIC0: &str =
    "0x099d178f911e9b704ac40d2373ef01bce3f790aeca9723177c283461078bd70a";

const NATIVE_TOKEN_SENTINEL: &str = "0x0000000000000000000000000000000000000000";

const ERC20_TRANSFER_TOPIC0: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

pub const DEFAULT_VAULT_PAY_ABI: &str =
    "function pay(address token, uint256 amount, bytes16 identifier, address merchant)";

pub const DEFAULT_VAULT_PAY_NATIVE_ABI: &str =
    "function payNative(bytes16 identifier, address merchant) payable";

pub const EVM_COIN_TYPE: u32 = 60;
pub const EVM_TEMPLATE: &str = "m/44'/{coin}'/{role}'/{index}'";

const EVM_REQUIRED_WALLETS: &[WalletSpec] = &[MAIN_WALLET, GAS_FEEDER_WALLET];

fn address_to_topic(addr_lc: &str) -> String {
    format!("0x{:0>64}", addr_lc.trim_start_matches("0x"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Views over chain data (changed: from-address, log_index, timestamp)
// ─────────────────────────────────────────────────────────────────────────────

/// A top-level value transfer. `from_lc` is `tx.from`, informational only —
/// it becomes `chain_movements.from_address`, which is nullable, never a key.
struct NativeTransfer {
    tx_hash: String,
    from_lc: String,
    to_lc: String,
    value: u128,
}

struct Erc20Transfer {
    tx_hash: String,
    /// Position in the block's log list. Feeds `event_index`.
    log_index: u64,
    token_lc: String,
    from_lc: String,
    to_lc: String,
    amount: u128,
    pub block_hash: String,
}


/// One canonical block, only the bits we need.
struct BlockView {
    number: u64,
    hash: String,
    parent_hash: String,
    /// `block.timestamp`, seconds since epoch. Stamped onto
    /// chain_transactions.block_time so recognition resolves the fee rate at the
    /// moment the money landed and records `occurred_at_exact = true`.
    timestamp: Option<i64>,
    transfers: Vec<NativeTransfer>,
}

impl BlockView {
    fn block_time(&self) -> Option<DateTime<Utc>> {
        self.timestamp.and_then(|t| DateTime::from_timestamp(t, 0))
    }
}

/// One attributed credit inside one transaction. Everything the payments
/// insert and the chain_movements row need, resolved once by the caller.
struct Credit<'a> {
    inv: &'a WatchedInvoice,
    amount: Decimal,
    path: PaymentPath,
    asset: AssetKey,
    event_index: i32,
    event_ref: String,
    from_lc: Option<String>,
    to_lc: String,
    to_kind: AddressKind,
    /// Path-specific webhook fields (TokenAddress, Payer…). The common ones
    /// are added in `apply_credits`.
    extra: Map<String, Value>,
}

fn payment_topic0() -> &'static str {
    static TOPIC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOPIC.get_or_init(|| {
        std::env::var("TOPIC_0")
            .unwrap_or_else(|_| DEFAULT_PAYMENT_TOPIC0.to_string())
            .to_lowercase()
    })
}

/// An indexed `address` topic is the address right-aligned in a 32-byte word.
fn topic_to_address(topic: &str) -> Result<String, String> {
    let h = topic.trim_start_matches("0x");
    if h.len() != 64 {
        return Err(format!("bad address topic '{topic}'"));
    }
    Ok(format!("0x{}", h[24..].to_lowercase()))
}

/// An indexed `bytes16` topic is LEFT-aligned (right-padded) in the 32-byte
/// word — fixed-size bytesN are the opposite of address/uint alignment. The
/// contract strips the dashes from a UUIDv4, so the first 16 bytes of the
/// topic ARE the invoice UUID.
fn topic_to_uuid(topic: &str) -> Result<Uuid, String> {
    let h = topic.trim_start_matches("0x");
    if h.len() != 64 {
        return Err(format!("bad bytes16 topic '{topic}'"));
    }
    let bytes = hex::decode(&h[..32]).map_err(|e| format!("bad hex in topic '{topic}': {e}"))?;
    Uuid::from_slice(&bytes).map_err(|e| format!("topic '{topic}' is not a UUID: {e}"))
}

/// Decoded CustodialPaymentVault.Payment event.
struct PaymentEvent {
    invoice_id: Uuid,
    merchant_lc: String,
    token_lc: String,
    payer_lc: String,
    amount_requested: u128,
    /// What the vault ACTUALLY received (post fee-on-transfer). This is the
    /// number we credit — mirrors the contract's own accounting.
    amount_received: u128,
}

fn decode_payment_log(log: &Log) -> Result<PaymentEvent, String> {
    if log.topics.len() != 4 {
        return Err(format!(
            "Payment log {} has {} topics, expected 4",
            log.transaction_hash, log.topics.len()
        ));
    }

    let merchant_lc = topic_to_address(&log.topics[1])?;
    let token_lc = topic_to_address(&log.topics[2])?;
    let invoice_id = topic_to_uuid(&log.topics[3])?;

    // data = payer (32) | amountRequested (32) | amountReceived (32) | timestamp (32)
    let d = log.data.trim_start_matches("0x");
    if d.len() < 4 * 64 {
        return Err(format!(
            "Payment log {} data too short: {} hex chars",
            log.transaction_hash, d.len()
        ));
    }
    let word = |i: usize| &d[i * 64..(i + 1) * 64];

    let payer_lc = format!("0x{}", word(0)[24..].to_lowercase());
    // hex_to_u128 errors on anything above u128 — same ceiling as the rest of
    // the money pipeline (see wei_to_decimal TODO about moving to u256).
    let amount_requested = hex_to_u128(word(1))?;
    let amount_received = hex_to_u128(word(2))?;

    Ok(PaymentEvent { invoice_id, merchant_lc, token_lc, payer_lc, amount_requested, amount_received })
}
pub fn evm_scheme() -> DerivationScheme {
    DerivationScheme {
        network_type: crate::assets::NETWORK_EVM,
        coin_type: EVM_COIN_TYPE,
        template: EVM_TEMPLATE,
        version: SCHEME_VERSION,
    }
}

/// Address and signing key from one derivation, so a feeder can never sign
/// with a key that doesn't match the address it believes it holds.
/// `derive_evm_address` is now a thin wrapper over this — there is exactly one
/// path from mnemonic to address in the codebase.
pub fn derive_evm_keypair(
    mnemonic: &str,
    role: KeyRole,
    index: u32,
) -> Result<(k256::ecdsa::SigningKey, String), String> {
    let scheme = evm_scheme();
    let seed = scheme.seed(mnemonic)?;
    let path = scheme.path(role, index)?;

    let xprv = XPrv::derive_from_path(&seed, &path)
        .map_err(|e| format!("Failed to derive at {}: {e}", scheme.path_string(role, index)))?;

    let secret = xprv.private_key();
    let point = secret.public_key().to_encoded_point(false);

    let mut hasher = Keccak256::new();
    hasher.update(&point.as_bytes()[1..]);
    let hash = hasher.finalize();
    let address = format!("0x{}", hex::encode(&hash[12..]));

    Ok((secret.clone(), address))
}


fn hex_to_u64(hex_str: &str) -> u64 {
    u64::from_str_radix(hex_str.trim_start_matches("0x"), 16).unwrap_or(0)
}
fn hex_to_u128(s: &str) -> Result<u128, String> {
    u128::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| format!("bad hex u128 '{s}': {e}"))
}

/// Base-unit (wei) -> Decimal. NUMERIC(78,0) can hold a full uint256 but
/// rust_decimal caps out at ~7.9e28, which is fine for wei amounts
/// (that's ~79 billion ETH) but *will* bite on a token with 24 decimals.
/// TODO: move the money type to a u256/BigInt wrapper and bind as string.
fn wei_to_decimal(v: u128) -> Result<rust_decimal::Decimal, String> {
    rust_decimal::Decimal::from_str_exact(&v.to_string()).map_err(|e| format!("amount {v} doesn't fit Decimal: {e}"))
}

#[derive(Clone)]
struct WatchedInvoice {
    invoice_id: Uuid,
    merchant_id: Uuid,
    address_lc: String,          // naive-QR per-invoice HD address
    merchant_wallet_lc: String,  // index-0 merchant wallet for smart contract path
    amount_requested: rust_decimal::Decimal,
    required_confirmations: i64,
    created_block: Option<i64>,
    /// ERC-20 contract address, lowercase. None => native. Log matching only.
    token_lc: Option<String>,
    /// Ledger route identifier. Not an address — this is what the Ledgerer
    /// resolves fee rates and treasury accounts against.
    token_id: String,
}

// ==========================================
// ### NETWORK IMPLEMENTATION ###
// ==========================================
pub struct EVMNetwork {
    chain_id: u64,
    pub network_name: String,
    display_name: String,
    rpc_urls: Vec<String>,
    pub contract_address: Option<String>,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
    ledger: Ledgerer,
}

impl EVMNetwork {
    const REORG_WINDOW: usize = 64;

    pub fn new(
        chain_id: u64,
        display_name: &str,
        rpc_urls: Vec<String>,
        contract_address: Option<String>,
    ) -> Self {
        assert!(!rpc_urls.is_empty(), "EVMNetwork requires at least one RPC URL");
        Self {
            chain_id,
            network_name: crate::assets::NETWORK_EVM.to_string(),
            display_name: display_name.to_string(),
            rpc_urls,
            contract_address,
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
            ledger: Ledgerer::new(),
        }
    }

    fn chain(&self) -> ChainRef {
        ChainRef::new(NETWORK_TYPE, self.chain_ref())
    }

    /// `chain_movements.event_index` for EVM.
    ///
    /// The top-level value transfer is ordinal 0; a log is `log_index + 1`.
    /// One tx has at most one top-level value transfer and its log indices are
    /// fixed by the block, so this is stable across replays and a tx that both
    /// sends ETH and emits Transfer logs can never collide on (tx_id, event_index).
    fn event_index_value() -> (i32, String) {
        (0, "value".to_string())
    }
    fn event_index_log(log_index: u64) -> (i32, String) {
        ((log_index + 1) as i32, format!("log:{log_index}"))
    }

    /// Token address must already be lowercase hex — the Ledgerer looks it up
    /// verbatim. `None` => native.
    fn asset_for(&self, token_lc: Option<&str>) -> Result<AssetKey, String> {
        AssetKey::from_optional_address(self.chain(), token_lc)
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn vault_address(&self) -> Option<&str> {
        self.contract_address.as_deref()
    }

    fn derivation_scheme(&self) -> DerivationScheme { evm_scheme() }

    fn canonicalize_address(&self, address: &str) -> String { address.to_lowercase() }
    async fn next_deposit_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<DerivedAddress, String> {
        // next_index holds the highest index allocated. Seeding at 1 on insert
        // means index 0 is permanently reserved for the main wallet.
        let row = sqlx::query!(
            r#"
            INSERT INTO merchant_network_indices (merchant_id, network, role, next_index)
            VALUES ($1, $2, $3, 1)
            ON CONFLICT (merchant_id, network, role)
            DO UPDATE SET
                next_index = merchant_network_indices.next_index + 1,
                updated_at = CURRENT_TIMESTAMP
            RETURNING next_index
            "#,
            merchant_id,
            self.network_name,
            KeyRole::Deposit.as_i16()
        )
            .fetch_one(pool).await
            .map_err(|e| format!("Failed to allocate deposit index: {e}"))?;

        let mut derived = self.derive(mnemonic, KeyRole::Deposit, row.next_index as u32)?;
        derived.reference = Some(format!("0x{}", hex::encode(invoice_id.as_bytes())));
        Ok(derived)
    }

    pub fn vault_pay_abi(&self) -> String {
        self.abi_env("EVM_VAULT_PAY_ABI", DEFAULT_VAULT_PAY_ABI)
    }

    pub fn vault_pay_native_abi(&self) -> String {
        self.abi_env("EVM_VAULT_PAY_NATIVE_ABI", DEFAULT_VAULT_PAY_NATIVE_ABI)
    }

    fn abi_env(&self, base_key: &str, default: &str) -> String {
        std::env::var(format!("{base_key}_{}", self.chain_id))
            .or_else(|_| std::env::var(base_key))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    fn chain_ref(&self) -> String {
        self.chain_id.to_string()
    }
    async fn get_block_number(&self) -> Result<u64, String> {
        let hex = self.call_rpc("eth_blockNumber", serde_json::json!([])).await?;
        u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|e| format!("Failed to parse block number '{hex}': {e}"))
    }

    /// `full = true` pulls the tx bodies so we can do native-value matching in
    /// one round trip instead of N `eth_getTransactionByHash` calls.
    async fn get_block(&self, number: u64, full: bool) -> Result<Option<BlockView>, String> {
        let raw = self.call_rpc_json(
            "eth_getBlockByNumber",
            serde_json::json!([format!("0x{:x}", number), full]),
        ).await?;

        if raw.is_null() {
            // Tip raced ahead of us / provider hasn't got the block yet.
            return Ok(None);
        }
        Self::parse_block(&raw).map(Some)
    }

    fn parse_block(raw: &serde_json::Value) -> Result<BlockView, String> {
        let number      = hex_to_u64(raw["number"].as_str().ok_or("block: no number")?);
        let hash        = raw["hash"].as_str().ok_or("block: no hash")?.to_lowercase();
        let parent_hash = raw["parentHash"].as_str().ok_or("block: no parentHash")?.to_lowercase();

        // Absent only on provider-side pending blocks. None is survivable: recognition
        // falls back to now() and marks occurred_at_exact = false.
        let timestamp = raw["timestamp"].as_str().map(|t| hex_to_u64(t) as i64);

        let mut transfers = Vec::new();
        if let Some(txs) = raw["transactions"].as_array() {
            for tx in txs {
                // full = false yields bare hash strings; nothing to match on.
                let (Some(tx_hash), Some(from), Some(to)) =
                    (tx["hash"].as_str(), tx["from"].as_str(), tx["to"].as_str())
                else {
                    continue; // contract creation has to = null
                };
                let value = hex_to_u128(
                    tx["value"].as_str().unwrap_or("0").trim_start_matches("0x"),
                ).unwrap_or(0);
                if value == 0 {
                    continue;
                }
                transfers.push(NativeTransfer {
                    tx_hash: tx_hash.to_lowercase(),
                    from_lc: from.to_lowercase(),
                    to_lc:   to.to_lowercase(),
                    value,
                });
            }
        }

        Ok(BlockView { number, hash, parent_hash, timestamp, transfers })
    }


    /// Used during reorg handling: "does this tx still exist, and where?"
    /// Returns Ok(None) if the node has never heard of it (dropped),
    /// Ok(Some((None, _))) if it's back in the mempool (mined_block = None).
    async fn locate_tx(&self, tx_hash: &str) -> Result<Option<(Option<u64>, Option<String>)>, String> {
        let raw = self.call_rpc_json(
            "eth_getTransactionByHash",
            serde_json::json!([tx_hash]),
        ).await?;

        if raw.is_null() {
            return Ok(None);
        }
        let bn = raw.get("blockNumber").and_then(|v| v.as_str()).map(hex_to_u64);
        let bh = raw.get("blockHash").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
        Ok(Some((bn, bh)))
    }


    // ─────────────────────────────────────────────────────────────────────────
    // Detection: one DB transaction per chain transaction
    // ─────────────────────────────────────────────────────────────────────────

    /// Record every credit a chain transaction produced, and return the
    /// invoices it touched so the caller recomputes each once.
    ///
    /// One database transaction for the whole tx hash. That matters because a
    /// tx that pays two invoices (a batching router hitting two deposit
    /// addresses) must land both credits or neither — a half commit leaves one
    /// invoice permanently short with nothing left to rescan.
    ///
    /// Idempotent on (invoice_id, tx_hash). Rescans hit ON CONFLICT and do
    /// nothing; the only UPDATE that fires is relocation (block changed) or
    /// resurrection (orphaned and re-landed). The amount is never rewritten.
    #[allow(clippy::too_many_arguments)]
    async fn apply_credits(
        &self,
        pool: &PgPool,
        tx_hash: &str,
        block_number: i64,
        block_hash: &str,
        block_time: Option<DateTime<Utc>>,
        credits: &[Credit<'_>],
    ) -> Result<Vec<Uuid>, String> {
        if credits.is_empty() {
            return Ok(Vec::new());
        }

        let mut db_tx = pool
            .begin()
            .await
            .map_err(|e| format!("apply_credits begin tx: {e}"))?;

        let mut touched = Vec::with_capacity(credits.len());
        let mut transfers = Vec::with_capacity(credits.len());

        for c in credits {
            let inv = c.inv;

            let inserted: Option<Uuid> = sqlx::query_scalar(
                r#"
                INSERT INTO payments
                    (invoice_id, tx_hash, amount, block_number, block_hash,
                     confirmations, status, payment_path)
                VALUES ($1, $2, $3, $4, $5, 0, 'detected', $6)
                ON CONFLICT (invoice_id, tx_hash) DO NOTHING
                RETURNING id
                "#,
            )
                .bind(inv.invoice_id)
                .bind(tx_hash)
                .bind(c.amount)
                .bind(block_number)
                .bind(block_hash)
                .bind(c.path.as_str())
                .fetch_optional(&mut *db_tx)
                .await
                .map_err(|e| format!("insert payment: {e}"))?;

            let payment_id = match inserted {
                Some(id) => {
                    println!(
                        "[{}] detected {} base units via {} path -> {} (invoice {}, merchant {}, tx {}, block {})",
                        self.network_name, c.amount, c.path.as_str(), c.to_lc,
                        inv.invoice_id, inv.merchant_id, tx_hash, block_number
                    );

                    let mut fields = c.extra.clone();
                    fields.insert("TxHash".into(), json!(tx_hash));
                    fields.insert("AmountBaseUnits".into(), json!(c.amount.to_string()));
                    fields.insert("BlockNumber".into(), json!(block_number));
                    fields.insert("BlockHash".into(), json!(block_hash));
                    fields.insert("PaymentPath".into(), json!(c.path.as_str()));
                    fields.insert("Confirmations".into(), json!(0));

                    // webhook_events is UNIQUE (merchant_id, dedupe_key). A bare
                    // tx_hash collides when one tx pays two invoices of the same
                    // merchant. Scope to event type + subject.
                    let dedupe_key = format!("payment.detected:{}:{}", inv.invoice_id, tx_hash);
                    enqueue_webhook(&mut db_tx, inv.invoice_id, "payment.detected", &dedupe_key, fields)
                        .await?;
                    id
                }
                None => {
                    // Seen before. Relocate if the block changed (re-mined after
                    // a reorg) and resurrect if we had orphaned it. Amount stays.
                    sqlx::query(
                        r#"
                        UPDATE payments
                           SET block_number = $2,
                               block_hash   = $3,
                               status       = CASE WHEN status = 'orphaned' THEN 'detected' ELSE status END,
                               payment_path = COALESCE(payment_path, $5),
                               updated_at   = now()
                         WHERE invoice_id = $1
                           AND tx_hash = $4
                           AND (block_hash <> $3 OR status = 'orphaned')
                        "#,
                    )
                        .bind(inv.invoice_id)
                        .bind(block_number)
                        .bind(block_hash)
                        .bind(tx_hash)
                        .bind(c.path.as_str())
                        .execute(&mut *db_tx)
                        .await
                        .map_err(|e| format!("relocate payment: {e}"))?;

                    sqlx::query_scalar::<_, Uuid>(
                        "SELECT id FROM payments WHERE invoice_id = $1 AND tx_hash = $2",
                    )
                        .bind(inv.invoice_id)
                        .bind(tx_hash)
                        .fetch_one(&mut *db_tx)
                        .await
                        .map_err(|e| format!("fetch payment id: {e}"))?
                }
            };

            transfers.push(ObservedTransfer {
                event_index: c.event_index,
                event_ref: Some(c.event_ref.clone()),
                asset: c.asset.clone(),
                amount: c.amount,
                from_address: c.from_lc.clone(),
                from_kind: Some(AddressKind::External),
                to_address: Some(c.to_lc.clone()),
                to_kind: Some(c.to_kind),
                merchant_id: Some(inv.merchant_id),
                invoice_id: Some(inv.invoice_id),
                payment_id: Some(payment_id),
                token_id: Some(inv.token_id.clone()),
            });

            touched.push(inv.invoice_id);
        }

        // A tx paying invoices of two different merchants has merchant_id = None
        // on the tx row; the movements carry their own.
        let tx_merchant = {
            let first = credits[0].inv.merchant_id;
            credits.iter().all(|c| c.inv.merchant_id == first).then_some(first)
        };

        // Chain layer, same transaction. Also what flips an orphaned
        // chain_transactions row back to 'detected' when the tx re-lands.
        self.ledger
            .record_detected(
                &mut *db_tx,
                &ObservedInbound {
                    chain: self.chain(),
                    tx_hash: tx_hash.to_string(),
                    block_number: Some(block_number),
                    block_hash: Some(block_hash.to_string()),
                    block_time,
                    merchant_id: tx_merchant,
                    token_id: None, // observed, not initiated; route is on each movement
                    transfers,
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        db_tx
            .commit()
            .await
            .map_err(|e| format!("apply_credits commit tx: {e}"))?;

        Ok(touched)
    }

    /// Credit every native transfer in this block that lands on a watched address.
    async fn apply_block(
        &self,
        pool: &PgPool,
        block: &BlockView,
        by_address: &HashMap<String, Vec<WatchedInvoice>>,
    ) -> Result<(), String> {
        let block_time = block.block_time();
        let mut touched: HashMap<Uuid, &WatchedInvoice> = HashMap::new();

        for t in &block.transfers {
            // A zero-value call to a deposit address moved nothing; recording it
            // would be a payment row with a zero movement.
            if t.value == 0 {
                continue;
            }
            let Some(invoices) = by_address.get(&t.to_lc) else { continue };
            let amount = wei_to_decimal(t.value)?;
            let (event_index, event_ref) = Self::event_index_value();
            let asset = self.asset_for(None)?;

            let credits: Vec<Credit> = invoices
                .iter()
                .filter(|inv| {
                    // Native-value only — a token invoice at this address must
                    // never be credited from a plain ETH transfer.
                    inv.token_lc.is_none()
                        && inv.created_block.map_or(true, |c| (block.number as i64) >= c)
                })
                .map(|inv| Credit {
                    inv,
                    amount,
                    path: PaymentPath::Direct,
                    asset: asset.clone(),
                    event_index,
                    event_ref: event_ref.clone(),
                    from_lc: Some(t.from_lc.clone()),
                    to_lc: t.to_lc.clone(),
                    to_kind: AddressKind::DepositAddress,
                    extra: Map::new(),
                })
                .collect();

            for id in self
                .apply_credits(pool, &t.tx_hash, block.number as i64, &block.hash, block_time, &credits)
                .await?
            {
                if let Some(inv) = credits.iter().find(|c| c.inv.invoice_id == id) {
                    touched.insert(id, inv.inv);
                }
            }
        }

        for (id, inv) in touched {
            self.recompute_invoice_totals(pool, id, std::slice::from_ref(inv)).await?;
        }
        Ok(())
    }

    /// Credit ERC-20 Transfer logs onto watched deposit addresses. `transfers`
    /// are already filtered to this block's hash by the caller.
    async fn apply_erc20_transfers(
        &self,
        pool: &PgPool,
        block: &BlockView,
        transfers: &[Erc20Transfer],
        by_address: &HashMap<String, Vec<WatchedInvoice>>,
    ) -> Result<(), String> {
        let block_time = block.block_time();

        // Group by tx so a router that fans one tx out to two deposit addresses
        // lands both credits in one DB transaction.
        let mut by_tx: HashMap<&str, Vec<Credit>> = HashMap::new();

        for t in transfers {
            if t.amount == 0 {
                continue;
            }
            let Some(invoices) = by_address.get(&t.to_lc) else { continue };
            let amount = wei_to_decimal(t.amount)?;
            let (event_index, event_ref) = Self::event_index_log(t.log_index);

            for inv in invoices {
                // Only credit invoices that expect exactly this token.
                let Some(expected) = inv.token_lc.as_deref() else { continue };
                if expected != t.token_lc {
                    continue;
                }
                if let Some(c) = inv.created_block {
                    if (block.number as i64) < c {
                        continue;
                    }
                }

                let mut extra = Map::new();
                extra.insert("TokenAddress".into(), json!(t.token_lc));

                by_tx.entry(t.tx_hash.as_str()).or_default().push(Credit {
                    inv,
                    amount,
                    path: PaymentPath::Direct,
                    asset: self.asset_for(Some(&t.token_lc))?,
                    event_index,
                    event_ref: event_ref.clone(),
                    from_lc: Some(t.from_lc.clone()),
                    to_lc: t.to_lc.clone(),
                    to_kind: AddressKind::DepositAddress,
                    extra,
                });
            }
        }

        let mut touched: HashMap<Uuid, &WatchedInvoice> = HashMap::new();
        for (tx_hash, credits) in &by_tx {
            for id in self
                .apply_credits(pool, tx_hash, block.number as i64, &block.hash, block_time, credits)
                .await?
            {
                if let Some(c) = credits.iter().find(|c| c.inv.invoice_id == id) {
                    touched.insert(id, c.inv);
                }
            }
        }

        for (id, inv) in touched {
            self.recompute_invoice_totals(pool, id, std::slice::from_ref(inv)).await?;
        }
        Ok(())
    }

    /// Vault `Payment` log: the WalletConnect / smart-contract path.
    ///
    /// The vault does NOT forward. `pay()`/`payNative()` credit
    /// `_vault[token][merchant]` and the value stays in the contract until the
    /// merchant wallet calls `sweep(token)`. So the movement names the vault as
    /// destination and the Ledgerer books it to custody_unswept with a sweep
    /// row; the sweep is a contract call signed by the merchant wallet, not an
    /// EOA transfer (see the Vault arm in refresh_confirmations).
    async fn apply_payment_log(
        &self,
        pool: &PgPool,
        log: &Log,
        by_id: &HashMap<Uuid, WatchedInvoice>,
        vault_lc: &str,
        block_time: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        if log.removed {
            return Ok(()); // reorg-removed entry from a lagging provider; reorg path owns this
        }

        let ev = match decode_payment_log(log) {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("[{}] skipping undecodable Payment log {}: {e}", self.network_name, log.transaction_hash);
                return Ok(());
            }
        };

        let Some(inv) = by_id.get(&ev.invoice_id) else {
            return Ok(());
        };

        if inv.merchant_wallet_lc != ev.merchant_lc {
            eprintln!(
                "[{}] Payment log for invoice {} credits wrong merchant {} (expected {}), ignoring",
                self.network_name, ev.invoice_id, ev.merchant_lc, inv.merchant_wallet_lc
            );
            return Ok(());
        }

        let expected_token: &str = inv.token_lc.as_deref().unwrap_or(NATIVE_TOKEN_SENTINEL);
        if expected_token != ev.token_lc {
            eprintln!(
                "[{}] Payment log for invoice {} paid in wrong token {} (expected {}), ignoring",
                self.network_name, ev.invoice_id, ev.token_lc, expected_token
            );
            return Ok(());
        }

        let block_number = hex_to_u64(&log.block_number) as i64;
        let block_hash = log.block_hash.to_lowercase();
        let tx_hash = log.transaction_hash.to_lowercase();
        let log_index = hex_to_u64(&log.log_index);

        if let Some(created) = inv.created_block {
            if block_number < created {
                return Ok(());
            }
        }

        if ev.amount_received == 0 {
            return Ok(());
        }
        let amount = wei_to_decimal(ev.amount_received)?;
        if ev.amount_received != ev.amount_requested {
            println!(
                "[{}] note: fee-on-transfer delta on invoice {}: requested {} received {}",
                self.network_name, inv.invoice_id, ev.amount_requested, ev.amount_received
            );
        }

        let (event_index, event_ref) = Self::event_index_log(log_index);

        let mut extra = Map::new();
        extra.insert("TokenAddress".into(), json!(ev.token_lc));
        extra.insert("Payer".into(), json!(ev.payer_lc));
        extra.insert("AmountRequested".into(), json!(ev.amount_requested.to_string()));


        let credit = Credit {
            inv,
            amount,
            path: PaymentPath::Reference, // identified by invoice id in the log
            // Native invoices arrive tagged address(0) in the event; the asset
            // is the chain's native one, not a contract at the sentinel.
            asset: self.asset_for(inv.token_lc.as_deref())?,
            event_index,
            event_ref,
            from_lc: Some(ev.payer_lc.clone()),
            to_lc: vault_lc.to_string(),
            to_kind: AddressKind::Vault,
            extra,
        };

        // No block body on this path, so no block_time. Recognition falls back
        // to now() and records occurred_at_exact = false. If that matters for
        // fee-rate resolution, fetch the block header per distinct block in
        // tick_logs and pass it through.
        let touched = self
            .apply_credits(pool, &tx_hash, block_number, &block_hash, block_time, std::slice::from_ref(&credit))
            .await?;

        if !touched.is_empty() {
            self.recompute_invoice_totals(pool, inv.invoice_id, std::slice::from_ref(inv)).await?;
        }
        Ok(())
    }


    // ── Building the plan ──────────────────────────────────────────────────────────
    /// The set of block ranges that can possibly contain money we care about.
    ///
    /// Two kinds of interest:
    ///
    ///   * **Live invoice** (`pending`/`underpaid`, not yet expired):
    ///     `[min(created_block, first_payment_block) .. open]`. Open-ended
    ///     because expiry is a wall-clock time in the future; when it passes,
    ///     the invoice simply stops coming back from this query and the range
    ///     disappears. No `expires_block` column, no per-chain block-time math.
    ///
    ///   * **Dead invoice with payments still counting confirmations**:
    ///     `[first_payment_block .. last_payment_block + FINAL_CONFIRMATIONS + 1]`.
    ///     We don't need these blocks to *detect* anything (confirmations are
    ///     computed off the DB against the live tip), but scanning them keeps
    ///     the `network_seen_blocks` anchors dense enough for the reorg
    ///     detector to unwind a payment that gets orphaned.
    ///
    /// Everything between those ranges is dead space and gets skipped by
    /// cursor jump instead of block-by-block RPC grinding.
    async fn load_scan_plan(&self, pool: &PgPool, tip: i64) -> Result<Vec<ScanRange>, String> {
        let rows = sqlx::query_as::<_, (Option<i64>, bool, Option<i64>, Option<i64>)>(
            r#"
            SELECT i.created_block,
                   (i.status IN ('pending','underpaid') AND i.expires_at > now()) AS live,
                   MIN(p.block_number) AS first_pay,
                   MAX(p.block_number) AS last_pay
              FROM invoices i
              LEFT JOIN payments p
                     ON p.invoice_id = i.id
                    AND p.status IN ('detected','merchant_confirmed')
             WHERE i.network_type = $1
               AND i.chain_ref   = $2
               AND (
                     (i.status IN ('pending','underpaid') AND i.expires_at > now())
                  OR EXISTS (SELECT 1 FROM payments p2
                              WHERE p2.invoice_id = i.id
                                AND p2.status IN ('detected','merchant_confirmed'))
                   )
             GROUP BY i.id, i.created_block, i.status, i.expires_at
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .fetch_all(pool)
            .await
            .map_err(|e| format!("load_scan_plan: {e}"))?;

        let mut ranges = Vec::with_capacity(rows.len());
        for (created_block, live, first_pay, last_pay) in rows {
            let anchor = [created_block, first_pay].into_iter().flatten().min();

            if live {
                // Still collectable => scan right up to the tip.
                ranges.push(ScanRange {
                    from: anchor.unwrap_or(tip).max(0),
                    to: None,
                });
            } else if let (Some(first), Some(last)) = (first_pay, last_pay) {
                // Expired or settled, but confirmations still in flight.
                ranges.push(ScanRange {
                    from: first.max(0),
                    to: Some(last + FINAL_CONFIRMATIONS + 1),
                });
            }
            // else: expired, never paid, nothing in flight -> no interest at all.
        }

        Ok(merge_ranges(ranges))
    }

    /// Park the cursor on `to_block` without scanning anything between here and
    /// there. We still fetch and remember the header, because the next tick's
    /// parent-hash continuity check and the reorg detector both need an anchor
    /// they can compare against.
    async fn fast_forward_cursor(
        &self,
        pool: &PgPool,
        scope: &str,
        to_block: i64,
    ) -> Result<(i64, String), String> {
        let anchor = self
            .get_block(to_block as u64, false)
            .await?
            .ok_or_else(|| format!("fast_forward_cursor: block {to_block} unavailable"))?;

        self.remember_block(pool, scope, &anchor).await?;
        self.save_cursor(pool, scope, to_block, &anchor.hash).await?;

        println!(
            "[{}] {} scanner skipped ahead to block {} (no watched invoice in between)",
            self.network_name, scope, to_block
        );
        Ok((to_block, anchor.hash))
    }


    // ── Scan state ───────────────────────────────────────────────────────────

    async fn load_cursor(&self, pool: &PgPool, scope: &str) -> Result<Option<(i64, String)>, String> {
        sqlx::query_as::<_, (i64, String)>(
            r#"
            SELECT last_block, last_block_hash
              FROM network_scan_state
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(scope)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load_cursor({scope}): {e}"))
    }

    async fn save_cursor(&self, pool: &PgPool, scope: &str, number: i64, hash: &str) -> Result<(), String> {
        sqlx::query(
            r#"
            INSERT INTO network_scan_state
                (network_type, chain_ref, scope, last_block, last_block_hash, updated_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (network_type, chain_ref, scope) DO UPDATE
               SET last_block = EXCLUDED.last_block,
                   last_block_hash = EXCLUDED.last_block_hash,
                   updated_at = now()
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(scope)
            .bind(number).bind(hash)
            .execute(pool).await
            .map_err(|e| format!("save_cursor({scope}): {e}"))?;
        Ok(())
    }

    async fn remember_block(&self, pool: &PgPool, scope: &str, b: &BlockView) -> Result<(), String> {
        sqlx::query(
            r#"
            INSERT INTO network_seen_blocks
                (network_type, chain_ref, scope, block_number, block_hash, parent_hash, seen_at)
            VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (network_type, chain_ref, scope, block_number) DO UPDATE
               SET block_hash = EXCLUDED.block_hash,
                   parent_hash = EXCLUDED.parent_hash,
                   seen_at = now()
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(scope)
            .bind(b.number as i64).bind(&b.hash).bind(&b.parent_hash)
            .execute(pool).await
            .map_err(|e| format!("remember_block({scope}): {e}"))?;
        Ok(())
    }

    async fn our_hash_at(&self, pool: &PgPool, scope: &str, number: i64) -> Result<Option<String>, String> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT block_hash FROM network_seen_blocks
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3 AND block_number = $4
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(scope).bind(number)
            .fetch_optional(pool).await
            .map_err(|e| format!("our_hash_at({scope}): {e}"))
    }

    async fn prune_seen_blocks(&self, pool: &PgPool, scope: &str, tip: i64) -> Result<(), String> {
        sqlx::query(
            r#"
            DELETE FROM network_seen_blocks
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3
               AND block_number < $4
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(scope)
            .bind(tip - (MAX_REORG_DEPTH as i64 * 2))
            .execute(pool).await
            .map_err(|e| format!("prune_seen_blocks({scope}): {e}"))?;
        Ok(())
    }

    // ── Who are we watching ──────────────────────────────────────────────────────────

    /// Changed vs the old version: `i.status = 'pending'` became
    /// `i.status IN ('pending','underpaid')`.
    ///
    /// That was the bug where partially-paid invoices vanished. Sequence was:
    /// partial payment lands -> recompute_invoice_totals writes status
    /// 'underpaid' -> that payment eventually reaches 'system_confirmed' ->
    /// both arms of the WHERE go false -> the invoice stops being watched even
    /// though it's unexpired and still owed money. The rest of the top-up never
    /// gets credited.
    ///
    /// Everything with an open interest on this chain:
    ///   - still-pending, unexpired invoices (we're waiting for money), OR
    ///   - invoices with at least one payment that hasn't reached
    ///     'system_confirmed' yet (money arrived, we're still counting).
    ///
    /// The second clause is the restart-safety bit: an invoice that already went
    /// 'paid' still needs its confirmation counter driven to FINAL_CONFIRMATIONS,
    /// and that must survive a process restart with an empty `self.pending`.
    async fn load_watched_invoices(&self, pool: &PgPool) -> Result<Vec<WatchedInvoice>, String> {
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, rust_decimal::Decimal, i64, Option<i64>, Option<String>, String)>(
            r#"
    SELECT i.id,
           i.merchant_id,
           lower(i.wallet_address),
           lower(mw.address),
           i.amount_requested,
           COALESCE(i.required_confirmations, $3)::bigint,
           i.created_block,
           lower(i.token_address),
           i.token_id
      FROM invoices i
      JOIN merchant_wallets mw
        ON mw.merchant_id  = i.merchant_id
       AND mw.network_type = $1
     WHERE i.network_type = $1
       AND i.chain_ref   = $2
       AND (
             (i.status IN ('pending','underpaid') AND i.expires_at > now())
          OR EXISTS (SELECT 1 FROM payments p
                      WHERE p.invoice_id = i.id
                        AND p.status IN ('detected','merchant_confirmed'))
       )
    "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(FINAL_CONFIRMATIONS)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("load_watched_invoices: {e}"))?;

        Ok(rows
            .into_iter()
            .map(|(invoice_id, merchant_id, address_lc, merchant_wallet_lc, amount_requested,
                      required_confirmations, created_block, token_lc, token_id)| {
                WatchedInvoice {
                    invoice_id,
                    merchant_id,
                    address_lc,
                    merchant_wallet_lc,
                    amount_requested,
                    required_confirmations,
                    created_block,
                    token_lc,
                    token_id,
                }
            })
            .collect())
    }

    // ── Batched ERC-20 log fetch ──────────────────────────────────────────────────────────

    /// One `eth_getLogs` per *chunk* instead of one per block. On the free tier
    /// with MAX_LOG_BLOCK_RANGE = 10 that's a 10x cut in RPC calls on the
    /// address scanner, which is most of why catch-up was so slow.
    ///
    /// Keyed by block number; `block_hash` is carried on each transfer so the
    /// caller can drop logs that belong to a sibling block if the chain moved
    /// under us mid-chunk.
    async fn get_erc20_transfers_range(
        &self,
        from_block: u64,
        to_block: u64,
        to_addresses: &[String],
    ) -> Result<HashMap<u64, Vec<Erc20Transfer>>, String> {
        let mut out: HashMap<u64, Vec<Erc20Transfer>> = HashMap::new();
        if to_addresses.is_empty() {
            return Ok(out);
        }

        // Two invoices can share a deposit address. Chunking a list with
        // duplicates in it puts the same address in two filters and returns the
        // same log twice, which is a duplicate (tx_id, event_index) movement.
        let mut uniq: Vec<String> = to_addresses.to_vec();
        uniq.sort();
        uniq.dedup();

        for addr_chunk in uniq.chunks(MAX_TOPIC_ADDRESSES) {
            let to_topics: Vec<String> = addr_chunk.iter().map(|a| address_to_topic(a)).collect();

            let filter = serde_json::json!({
                "fromBlock": format!("0x{:x}", from_block),
                "toBlock":   format!("0x{:x}", to_block),
                "topics": [ERC20_TRANSFER_TOPIC0, serde_json::Value::Null, to_topics],
            });

            for log in self.get_logs(filter).await? {
                if log.removed {
                    continue;
                }
                // Standard Transfer(address,address,uint256) has exactly 3 topics.
                if log.topics.len() != 3 {
                    continue;
                }
                let from_lc = match topic_to_address(&log.topics[1]) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let to_lc = match topic_to_address(&log.topics[2]) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let data = log.data.trim_start_matches("0x");
                if data.len() < 64 {
                    continue;
                }
                let amount = match hex_to_u128(&data[..64]) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                if amount == 0 {
                    continue;
                }

                out.entry(hex_to_u64(&log.block_number))
                    .or_default()
                    .push(Erc20Transfer {
                        tx_hash: log.transaction_hash.to_lowercase(),
                        log_index: hex_to_u64(&log.log_index),
                        token_lc: log.address.to_lowercase(),
                        from_lc,
                        to_lc,
                        amount,
                        block_hash: log.block_hash.to_lowercase(),
                    });
            }
        }

        Ok(out)
    }

    // ── The service loop ─────────────────────────────────────────────────────

    pub async fn watch_addresses(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "EVMNetwork::watch_addresses service started for {} ({})",
            self.network_name, self.chain_id
        );

        loop {
            if let Err(e) = self.tick_addresses(pool).await {
                // Transient by assumption: RPC hiccup, quorum split mid-reorg,
                // provider lagging the tip. The cursor is only advanced on
                // success, so the next tick just redoes the work.
                eprintln!(
                    "EVMNetwork::watch_addresses tick failed [{}]: {e}",
                    self.network_name
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    async fn tick_addresses(&self, pool: &PgPool) -> Result<(), String> {
        let watched = self.load_watched_invoices(pool).await?;
        let tip = self.get_block_number().await? as i64;
        let scan_ceiling = (tip - 1).max(0);
        let plan = self.load_scan_plan(pool, tip).await?;

        let mut by_address: HashMap<String, Vec<WatchedInvoice>> = HashMap::new();
        for w in &watched {
            by_address.entry(w.address_lc.clone()).or_default().push(w.clone());
        }

        // 1. Where do we resume from? Cold start floors at the plan's first
        //    interesting block (or the tip if there's nothing to watch).
        let mut cursor = match self.load_cursor(pool, SCAN_SCOPE_ADDRESSES).await? {
            Some(c) => c,
            None => {
                let from = plan
                    .first()
                    .map(|r| (r.from - 1).max(0))
                    .unwrap_or(scan_ceiling);
                println!(
                    "[{}] no scan cursor, cold-starting at block {}",
                    self.network_name,
                    from + 1
                );
                self.save_cursor(pool, SCAN_SCOPE_ADDRESSES, from, "").await?;
                (from, String::new())
            }
        };

        // 2. Reorg check + unwind, before we apply anything new. (unchanged)
        {
            let (last_block, last_hash) = cursor.clone();
            if !last_hash.is_empty() {
                if let Some(fork_point) =
                    self.detect_fork_point(pool, last_block, &last_hash).await?
                {
                    if fork_point < last_block {
                        println!(
                            "[{}] reorg detected: cursor was {}, rewinding to {}",
                            self.network_name, last_block, fork_point
                        );
                        self.handle_reorg(pool, fork_point, &watched).await?;
                        let fork_hash = self
                            .our_hash_at(pool, SCAN_SCOPE_ADDRESSES, fork_point)
                            .await?
                            .unwrap_or_default();
                        self.save_cursor(pool, SCAN_SCOPE_ADDRESSES, fork_point, &fork_hash)
                            .await?;
                        cursor = (fork_point, fork_hash);
                    }
                }
            }
        }

        let (mut last_block, mut last_hash) = cursor;

        // 3. Skip dead space *before* spending any budget.
        match plan_next_block(&plan, last_block + 1) {
            None => {
                // Nothing to watch at or after the cursor. Don't let scan debt
                // accumulate while we're idle — park on the ceiling so the next
                // invoice starts from "now" instead of from wherever we stopped
                // days ago. This is the "no active invoices => don't poll" case.
                if scan_ceiling > last_block {
                    let (b, h) = self
                        .fast_forward_cursor(pool, SCAN_SCOPE_ADDRESSES, scan_ceiling)
                        .await?;
                    last_block = b;
                    last_hash = h;
                }
                self.refresh_confirmations(pool, tip, &watched).await?;
                self.prune_seen_blocks(pool, SCAN_SCOPE_ADDRESSES, last_block).await?;
                return Ok(());
            }
            Some(n) if n > last_block + 1 => {
                let jump_to = (n - 1).min(scan_ceiling);
                if jump_to > last_block {
                    let (b, h) = self
                        .fast_forward_cursor(pool, SCAN_SCOPE_ADDRESSES, jump_to)
                        .await?;
                    last_block = b;
                    last_hash = h;
                }
            }
            _ => {}
        }

        // 4. Apply new blocks, chunked, budget-capped.
        let watched_addresses: Vec<String> = by_address.keys().cloned().collect();
        let mut scanned: u64 = 0;
        let mut n = last_block + 1;

        'outer: while scanned < MAX_BLOCKS_PER_TICK && n <= scan_ceiling {
            let range_end = plan_range_end(&plan, n, scan_ceiling);
            let budget_end = n + (MAX_BLOCKS_PER_TICK - scanned) as i64 - 1;
            let chunk_end = *[
                scan_ceiling,
                range_end,
                budget_end,
                n + MAX_LOG_BLOCK_RANGE as i64 - 1,
            ]
                .iter()
                .min()
                .unwrap();

            // One getLogs for the whole chunk.
            let mut erc20_by_block = self
                .get_erc20_transfers_range(n as u64, chunk_end as u64, &watched_addresses)
                .await?;

            let mut m = n;
            while m <= chunk_end {
                let block = match self.get_block(m as u64, true).await? {
                    Some(b) => b,
                    None => break 'outer, // provider lagging the tip
                };

                if !last_hash.is_empty() && block.parent_hash != last_hash {
                    println!(
                        "[{}] parent mismatch at block {} (expected parent {}, got {}), deferring to reorg handling",
                        self.network_name, m, last_hash, block.parent_hash
                    );
                    break 'outer;
                }

                self.apply_block(pool, &block, &by_address).await?;

                if let Some(transfers) = erc20_by_block.remove(&(m as u64)) {
                    // Drop anything that came from a sibling block: the getLogs
                    // and the getBlock are separate round trips, so a reorg can
                    // land between them. Whatever we drop here gets picked up on
                    // the rescan the parent-mismatch/reorg path triggers.
                    let transfers: Vec<Erc20Transfer> = transfers
                        .into_iter()
                        .filter(|t| t.block_hash == block.hash)
                        .collect();
                    if !transfers.is_empty() {
                        self.apply_erc20_transfers(pool, &block, &transfers, &by_address)
                            .await?;
                    }
                }

                self.remember_block(pool, SCAN_SCOPE_ADDRESSES, &block).await?;
                last_hash = block.hash.clone();
                last_block = m;
                self.save_cursor(pool, SCAN_SCOPE_ADDRESSES, last_block, &last_hash).await?;

                scanned += 1;
                m += 1;
            }

            n = chunk_end + 1;

            // Walked off the end of a range? Jump to the next one rather than
            // grinding through the gap.
            if n > range_end {
                match plan_next_block(&plan, n) {
                    Some(next_from) if next_from > n => {
                        let jump_to = (next_from - 1).min(scan_ceiling);
                        if jump_to > last_block {
                            let (b, h) = self
                                .fast_forward_cursor(pool, SCAN_SCOPE_ADDRESSES, jump_to)
                                .await?;
                            last_block = b;
                            last_hash = h;
                            n = last_block + 1;
                        } else {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }

        // 5. Confirmations off the DB against the live tip. (unchanged)
        self.refresh_confirmations(pool, tip, &watched).await?;
        self.prune_seen_blocks(pool, SCAN_SCOPE_ADDRESSES, last_block).await?;
        Ok(())
    }

    /// Walk back from our cursor comparing our remembered hashes with the
    /// canonical chain. Returns the highest block we still agree on, or None if
    /// nothing changed / we have no history to compare against.
    async fn detect_fork_point(
        &self,
        pool: &PgPool,
        last_block: i64,
        last_hash: &str,
    ) -> Result<Option<i64>, String> {
        let canonical = self.get_block(last_block as u64, false).await?;
        if let Some(b) = &canonical {
            if b.hash == last_hash {
                return Ok(None); // no reorg
            }
        }

        let floor = (last_block - MAX_REORG_DEPTH as i64).max(0);
        let mut probe = last_block - 1;
        while probe >= floor {
            let ours = match self.our_hash_at(pool, SCAN_SCOPE_ADDRESSES, probe).await? {
                Some(h) => h,
                // We never saw this block (pruned, or cold-started above it).
                // Nothing older to compare — treat it as the fork point and
                // rescan forward from here.
                None => return Ok(Some(probe)),
            };
            match self.get_block(probe as u64, false).await? {
                Some(b) if b.hash == ours => return Ok(Some(probe)),
                _ => probe -= 1,
            }
        }

        // Deeper than we're willing to unwind. This is not a "retry" situation —
        // it means our assumptions about the chain are wrong (or the RPC set is
        // serving a different chain entirely).
        // TODO: raise an operational alert / freeze this chain's payouts instead
        //       of silently rewinding, and expose it on a health endpoint.
        eprintln!(
            "[{}] reorg deeper than MAX_REORG_DEPTH ({} blocks) — clamping to {}",
            self.network_name, MAX_REORG_DEPTH, floor
        );
        Ok(Some(floor))
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Confirmations / recognition
    // ─────────────────────────────────────────────────────────────────────────

    async fn refresh_confirmations(
        &self,
        pool: &PgPool,
        tip: i64,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        if watched.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();
        let thresholds: HashMap<Uuid, i64> =
            watched.iter().map(|w| (w.invoice_id, w.required_confirmations)).collect();

        sqlx::query(
            r#"
            UPDATE payments
               SET confirmations = GREATEST(0, $2 - block_number + 1),
                   updated_at = now()
             WHERE invoice_id = ANY($1)
               AND status IN ('detected', 'merchant_confirmed')
               AND confirmations <> GREATEST(0, $2 - block_number + 1)
            "#,
        )
            .bind(&ids).bind(tip)
            .execute(pool).await
            .map_err(|e| format!("refresh_confirmations: {e}"))?;

        // `system_confirmed` is recognized and no longer polled; a deep reorg
        // that reaches back past it is handle_reorg's business, not this loop's.
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, i64, String, i64, String)>(
            r#"
            SELECT p.id, p.invoice_id, p.tx_hash, p.block_number, p.block_hash,
                   p.confirmations::bigint, p.status
              FROM payments p
             WHERE p.invoice_id = ANY($1)
               AND p.status IN ('detected', 'merchant_confirmed')
             ORDER BY p.block_number ASC
            "#,
        )
            .bind(&ids)
            .fetch_all(pool).await
            .map_err(|e| format!("refresh_confirmations select: {e}"))?;

        for (payment_id, invoice_id, tx_hash, block_number, block_hash, confirmations, status) in rows {
            let Some(required) = thresholds.get(&invoice_id).copied() else { continue };

            // ── merchant threshold ───────────────────────────────────────────
            if status == "detected" && confirmations >= required {
                let mut tx = pool.begin().await
                    .map_err(|e| format!("refresh_confirmations begin tx (confirmed): {e}"))?;

                // The guarded UPDATE is the once-only latch.
                let promoted = sqlx::query(
                    r#"
                    UPDATE payments
                       SET status = 'merchant_confirmed', updated_at = now()
                     WHERE id = $1 AND status = 'detected'
                    "#,
                )
                    .bind(payment_id)
                    .execute(&mut *tx).await
                    .map_err(|e| format!("promote merchant_confirmed: {e}"))?
                    .rows_affected() == 1;

                if promoted {
                    println!(
                        "[{}] payment {} reached {}/{} confirmations -> merchant_confirmed",
                        self.network_name, payment_id, confirmations, required
                    );

                    // Chain layer only. No journal at this level (§5.3).
                    self.ledger
                        .mark_confirmed(&mut *tx, &self.chain(), &tx_hash)
                        .await
                        .map_err(|e| e.to_string())?;

                    let mut fields = Map::new();
                    fields.insert("PaymentId".into(), json!(payment_id));
                    fields.insert("TxHash".into(), json!(tx_hash));
                    fields.insert("BlockNumber".into(), json!(block_number));
                    fields.insert("BlockHash".into(), json!(block_hash));
                    fields.insert("Confirmations".into(), json!(confirmations));
                    fields.insert("RequiredConfirmations".into(), json!(required));

                    let dedupe_key = format!("payment.confirmed:{payment_id}");
                    enqueue_webhook(&mut tx, invoice_id, "payment.confirmed", &dedupe_key, fields).await?;

                    tx.commit().await
                        .map_err(|e| format!("refresh_confirmations commit tx (confirmed): {e}"))?;
                } else {
                    tx.rollback().await.ok();
                }
            }

            // ── final / system threshold ─────────────────────────────────────
            // TODO: FINAL_CONFIRMATIONS is global; should be per-chain.
            if confirmations >= FINAL_CONFIRMATIONS {
                let mut tx = pool.begin().await
                    .map_err(|e| format!("refresh_confirmations begin tx (finalized): {e}"))?;

                // §5.4: the guarded UPDATE and the journal are one transaction.
                // `status <> 'orphaned'`: a row handle_reorg orphaned in this
                // same tick must not be resurrected by a stale confirmations read.
                let finalized = sqlx::query(
                    r#"
                    UPDATE payments
                       SET status = 'system_confirmed', updated_at = now()
                     WHERE id = $1
                       AND status NOT IN ('system_confirmed', 'orphaned')
                    "#,
                )
                    .bind(payment_id)
                    .execute(&mut *tx).await
                    .map_err(|e| format!("promote system_confirmed: {e}"))?
                    .rows_affected() == 1;

                if !finalized {
                    tx.rollback().await.ok();
                    continue;
                }

                println!(
                    "[{}] payment {} reached {} confirmations -> system_confirmed (block {}), no longer polled",
                    self.network_name, payment_id, confirmations, block_number
                );

                // Everything recognition needs, read inside the same tx. The
                // asset comes off the payment's own movement — the thing the
                // Ledgerer will book — not re-derived from the invoice.
                let ctx = sqlx::query(
                    r#"
                    SELECT i.merchant_id, i.token_id, lower(i.wallet_address) AS wallet_address,
                           i.wallet_index, p.payment_path,
                           m.token_address, m.asset_params, m.to_kind
                      FROM payments p
                      JOIN invoices i ON i.id = p.invoice_id
                      LEFT JOIN LATERAL (
                          SELECT NULLIF(a.address, '') AS token_address, a.asset_params, cm.to_kind
                            FROM chain_movements cm
                            JOIN assets a ON a.id = cm.asset_id
                           WHERE cm.payment_id = p.id
                           ORDER BY cm.event_index
                           LIMIT 1
                      ) m ON true
                     WHERE p.id = $1
                    "#,
                )
                    .bind(payment_id)
                    .fetch_one(&mut *tx).await
                    .map_err(|e| format!("recognition ctx: {e}"))?;

                let merchant_id: Uuid = ctx.get("merchant_id");
                let token_id: String = ctx.get("token_id");
                let wallet_address: String = ctx.get("wallet_address");
                let wallet_index: Option<i32> = ctx.get("wallet_index");
                let path_str: Option<String> = ctx.get("payment_path");
                let token_address: Option<String> = ctx.get("token_address");

                // A NULL path means the row predates the ledger. It also has no
                // movements, so recognition would fail anyway — fail here with a
                // message that says what to do.
                let path = path_str
                    .as_deref()
                    .and_then(PaymentPath::from_db)
                    .ok_or_else(|| format!(
                        "payment {payment_id}: payment_path is {path_str:?}; pre-ledger row, \
                         backfill payment_path + chain_movements or let it drain before deploy"
                    ))?;

                // §2.8: on EVM the deposit EOA signs for itself. An ERC-20 sweep
                // needs the EOA to hold gas first, which is the gas wallet's job.
                let to_kind: Option<String> = ctx.get("to_kind");

                // Custody is decided by where the value landed, not by path.
                let custody = match to_kind.as_deref() {
                    // §2.8: the deposit EOA signs for itself. An ERC-20 sweep
                    // needs the EOA to hold gas first, which is the gas wallet's job.
                    Some("deposit_address") => Some(Custody {
                        address: wallet_address.clone(),
                        kind: AddressKind::DepositAddress,
                        authority_address: wallet_address.clone(),
                        authority_ref: wallet_index.map(|i| i.to_string()),
                        sweep_params: json!({
                            "chain_id": self.chain_id,
                            "mechanism": "eoa_transfer",
                            "token": token_address,               // null => native
                            "gas_topup_required": token_address.is_some(),
                            "external_fee_payer": false,
                        }),
                    }),

                    // Vault: value sits in the contract under
                    // _vault[token][merchant]. Only the merchant wallet can
                    // pull it (sweep() pays msg.sender), so the authority is
                    // the index-0 merchant wallet, and it always needs gas —
                    // even for native, since the ETH is inside the contract.
                    // sweep(token) drains the whole (token, merchant) balance,
                    // so the sweeper must claim and settle every pending row
                    // for that pair with one call, not one tx per row.
                    Some("vault") => {
                        let vault = self
                            .contract_address
                            .as_deref()
                            .map(str::to_lowercase)
                            .ok_or_else(|| format!(
                                "payment {payment_id}: vault movement but no contract_address configured"
                            ))?;
                        let merchant_wallet = watched
                            .iter()
                            .find(|w| w.invoice_id == invoice_id)
                            .map(|w| w.merchant_wallet_lc.clone())
                            .ok_or_else(|| format!(
                                "payment {payment_id}: vault movement but invoice not in watched set"
                            ))?;
                        Some(Custody {
                            address: vault.clone(),
                            kind: AddressKind::Vault,
                            authority_address: merchant_wallet,
                            authority_ref: Some(0.to_string()),
                            sweep_params: json!({
                                "chain_id": self.chain_id,
                                "mechanism": "vault_sweep",
                                "vault": vault,
                                "token": token_address,           // null => sweep(address(0))
                                "aggregate_by": ["custody_address", "authority_address", "asset_id"],
                                "gas_topup_required": true,
                                "external_fee_payer": false,
                            }),
                        })
                    }

                    // Landed in the merchant's own wallet: nothing to sweep.
                    Some("merchant_main") => None,

                    other => return Err(format!(
                        "payment {payment_id}: cannot build custody for to_kind {other:?}"
                    )),
                };


                let outcome = self
                    .ledger
                    .recognize_payment(
                        &mut *tx,
                        &RecognizeInput {
                            chain: self.chain(),
                            tx_hash: tx_hash.clone(),
                            payment_id,
                            invoice_id,
                            merchant_id,
                            token_id,
                            path,
                            block_time: None, // stamped on chain_txs at detection
                            custody,
                            already_swept: false, // until payments.swept_by_tx_id exists
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;

                if let Some(j) = outcome.journal_id {
                    println!(
                        "[{}] payment {} recognized: journal {} amount {} fee {} ({} bps), {} sweep row(s)",
                        self.network_name, payment_id, j, outcome.amount, outcome.fee,
                        outcome.fee_bps, outcome.sweep_rows_enqueued
                    );
                }

                let mut fields = Map::new();
                fields.insert("PaymentId".into(), json!(payment_id));
                fields.insert("TxHash".into(), json!(tx_hash));
                fields.insert("BlockNumber".into(), json!(block_number));
                fields.insert("BlockHash".into(), json!(block_hash));
                fields.insert("Confirmations".into(), json!(confirmations));

                let dedupe_key = format!("payment.finalized:{payment_id}");
                enqueue_webhook(&mut tx, invoice_id, "payment.finalized", &dedupe_key, fields).await?;

                tx.commit().await
                    .map_err(|e| format!("refresh_confirmations commit tx (finalized): {e}"))?;
            }

            self.recompute_invoice_totals(pool, invoice_id, watched).await?;
        }

        // Anything fully settled drops out of the in-memory hint map so we stop
        // doing work for it. The DB query at the top of the tick already
        // excludes it, this just keeps `pending` from growing forever.
        let done = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT i.id FROM invoices i
             WHERE i.id = ANY($1)
               AND i.status <> 'pending'
               AND NOT EXISTS (
                   SELECT 1 FROM payments p
                    WHERE p.invoice_id = i.id
                      AND p.status IN ('detected', 'merchant_confirmed')
               )
            "#,
        )
            .bind(&ids)
            .fetch_all(pool).await
            .map_err(|e| format!("settled invoices: {e}"))?;

        if !done.is_empty() {
            if let Ok(mut pending) = self.pending.lock() {
                for id in done {
                    pending.remove(&id);
                }
            }
        }

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Reorg
    // ─────────────────────────────────────────────────────────────────────────

    /// Everything above `fork_point` is suspect. Re-located txs keep their
    /// value and restart the confirmation countdown; dropped txs are orphaned
    /// and, if already recognized, reversed in the ledger.
    ///
    /// A SweepInFlight alarm on one payment does not stop the others being
    /// handled — but it does fail the tick, so the cursor stays put and the
    /// fork is re-detected next tick. That is a retry loop on purpose: a sweep
    /// in flight against vanished value is not something to step past quietly.
    async fn handle_reorg(
        &self,
        pool: &PgPool,
        fork_point: i64,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();
        if ids.is_empty() {
            return Ok(());
        }

        // Includes system_confirmed: a reorg deeper than FINAL_CONFIRMATIONS is
        // exactly the probabilistic case the reversal path exists for.
        let affected = sqlx::query_as::<_, (Uuid, Uuid, String, i64, String, String)>(
            r#"
            SELECT p.id, p.invoice_id, p.tx_hash, p.block_number, p.block_hash, p.status
              FROM payments p
             WHERE p.invoice_id = ANY($1)
               AND p.block_number > $2
               AND p.status <> 'orphaned'
            "#,
        )
            .bind(&ids)
            .bind(fork_point)
            .fetch_all(pool).await
            .map_err(|e| format!("handle_reorg select: {e}"))?;

        let mut first_err: Option<String> = None;

        for (payment_id, invoice_id, tx_hash, old_block, old_hash, old_status) in affected {
            match self.locate_tx(&tx_hash).await? {
                // Still mined, in a block we now consider canonical.
                Some((Some(new_block), Some(new_hash))) => {
                    let mut tx = pool.begin().await
                        .map_err(|e| format!("handle_reorg begin tx (re-mine): {e}"))?;

                    sqlx::query(
                        r#"
                        UPDATE payments
                           SET block_number = $2,
                               block_hash   = $3,
                               confirmations = 0,
                               status = 'detected',
                               updated_at = now()
                         WHERE id = $1
                        "#,
                    )
                        .bind(payment_id).bind(new_block as i64).bind(&new_hash)
                        .execute(&mut *tx).await
                        .map_err(|e| format!("handle_reorg re-mine update: {e}"))?;

                    // Chain layer follows. record_detected with no transfers is
                    // the sanctioned relocate: it refreshes block fields on
                    // chain_transactions and never regresses confirmed/final.
                    // If a journal already exists (was system_confirmed), the
                    // latch in recognize_payment makes the re-climb a no-op —
                    // the money never left, so the books don't move.
                    self.ledger
                        .record_detected(
                            &mut *tx,
                            &ObservedInbound {
                                chain: self.chain(),
                                tx_hash: tx_hash.clone(),
                                block_number: Some(new_block as i64),
                                block_hash: Some(new_hash.clone()),
                                block_time: None,
                                merchant_id: None,
                                token_id: None,
                                transfers: Vec::new(),
                            },
                        )
                        .await
                        .map_err(|e| e.to_string())?;

                    tx.commit().await
                        .map_err(|e| format!("handle_reorg commit tx (re-mine): {e}"))?;

                    println!(
                        "[{}] payment {} re-mined {}@{} -> {}@{}, confirmations reset (no webhook)",
                        self.network_name, payment_id, old_block, old_hash, new_block, new_hash
                    );
                }

                // Back in the mempool, or dropped entirely.
                Some((Some(_), None)) | Some((None, _)) | None => {
                    match self
                        .orphan_payment(pool, payment_id, invoice_id, &tx_hash, old_block, &old_hash, &old_status)
                        .await
                    {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("[{}] handle_reorg: {e}", self.network_name);
                            first_err.get_or_insert(e);
                            continue;
                        }
                    }
                }
            }

            // Rebuilt from surviving payments, never decremented, so this stays
            // correct no matter how many times a reorg replays.
            self.recompute_invoice_totals(pool, invoice_id, watched).await?;
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The one reorg case that always notifies the merchant: money we told
    /// them about is gone.
    ///
    /// Same DB transaction for the payments flip, the chain-layer flip, the
    /// reversal journal (if one is due) and the webhook. On a sweep-in-flight
    /// alarm everything rolls back and the payments row stays where it was
    /// for a human to look at.
    #[allow(clippy::too_many_arguments)]
    async fn orphan_payment(
        &self,
        pool: &PgPool,
        payment_id: Uuid,
        invoice_id: Uuid,
        tx_hash: &str,
        old_block: i64,
        old_hash: &str,
        old_status: &str,
    ) -> Result<bool, String> {
        let mut tx = pool.begin().await
            .map_err(|e| format!("orphan_payment begin tx: {e}"))?;

        let orphaned = sqlx::query(
            r#"
            UPDATE payments
               SET status = 'orphaned', confirmations = 0, updated_at = now()
             WHERE id = $1 AND status <> 'orphaned'
            "#,
        )
            .bind(payment_id)
            .execute(&mut *tx).await
            .map_err(|e| format!("orphan update: {e}"))?
            .rows_affected() == 1;

        if !orphaned {
            tx.rollback().await.ok();
            return Ok(false);
        }

        println!(
            "[{}] payment {} orphaned (tx {} no longer mined, was {}@{}, prev status {})",
            self.network_name, payment_id, tx_hash, old_block, old_hash, old_status
        );

        // §5.1: N confirmations is a probability, so a recognized journal here
        // is a legitimate case and gets a reversal. SweepInFlight is the one
        // thing that refuses: value vanished under a sweep that's already
        // claimed or broadcast.
        let outcome = self
            .ledger
            .orphan(
                &mut *tx,
                &OrphanInput {
                    chain: self.chain(),
                    tx_hash: tx_hash.to_string(),
                    payment_id,
                    finality: Finality::Probabilistic,
                    reason: format!("reorg: tx no longer mined, was {old_block}@{old_hash}"),
                },
            )
            .await;

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                if matches!(e, LedgerError::SweepInFlight { .. } | LedgerError::ImpossibleReversal { .. }) {
                    eprintln!("[{}] REFUSING to orphan payment {payment_id}: {e}", self.network_name);
                }
                tx.rollback().await.ok();
                return Err(e.to_string());
            }
        };

        let reversed = match &outcome {
            crate::ledgerer::OrphanOutcome::Reversed { reversal_journal_id, .. } => {
                println!(
                    "[{}] payment {} was recognized; reversal journal {} written",
                    self.network_name, payment_id, reversal_journal_id
                );
                Some(*reversal_journal_id)
            }
            _ => None,
        };

        let mut fields = Map::new();
        fields.insert("PaymentId".into(), json!(payment_id));
        fields.insert("TxHash".into(), json!(tx_hash));
        fields.insert("OldBlockNumber".into(), json!(old_block));
        fields.insert("OldBlockHash".into(), json!(old_hash));
        fields.insert("PreviousStatus".into(), json!(old_status));
        fields.insert("LedgerReversed".into(), json!(reversed.is_some()));

        // Block in the key: a payment can be orphaned, re-land, and be
        // orphaned again; on the bare payment_id the second event would be
        // deduped away.
        let dedupe_key = format!("payment.orphaned:{payment_id}:{old_block}");

        // TODO: skip once merchant webhook settings exist and this merchant
        //       has opted out of orphaned notifications.
        enqueue_webhook(&mut tx, invoice_id, "payment.orphaned", &dedupe_key, fields).await?;

        tx.commit().await
            .map_err(|e| format!("orphan_payment commit tx: {e}"))?;

        Ok(true)
    }


    /// Rebuild invoices.amount_received / status from the non-orphaned payments.
    /// Always a full recompute (never a delta) so reorgs, rescans and duplicate
    /// ticks all converge on the same number.
    async fn recompute_invoice_totals(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let Some(inv) = watched.iter().find(|w| w.invoice_id == invoice_id) else { return Ok(()) };

        // All amounts are base units (wei / smallest token unit), never human
        // readable — decimals are only applied at the presentation layer.
        let received = sqlx::query_scalar::<_, Decimal>(
            r#"
            SELECT COALESCE(SUM(amount), 0)
              FROM payments
             WHERE invoice_id = $1 AND status <> 'orphaned'
            "#,
        )
            .bind(invoice_id)
            .fetch_one(pool).await
            .map_err(|e| format!("sum payments: {e}"))?;

        let new_status = if received >= inv.amount_requested {
            if received > inv.amount_requested { "overpaid" } else { "paid" }
        } else if received > Decimal::ZERO {
            "underpaid"
        } else {
            "pending"
        };

        // Guarded update: only write (and therefore only fire) on a real
        // transition, and never resurrect an 'expired' invoice's status.
        let old_status = sqlx::query_scalar::<_, String>(
            r#"
            UPDATE invoices
               SET amount_received = $2,
                   status = CASE WHEN status = 'expired' THEN status ELSE $3 END,
                   updated_at = now()
             WHERE id = $1
               AND (amount_received <> $2 OR (status <> $3 AND status <> 'expired'))
            RETURNING (SELECT status FROM invoices WHERE id = $1)
            "#,
        )
            .bind(invoice_id)
            .bind(received)
            .bind(new_status)
            .fetch_optional(pool).await
            .map_err(|e| format!("update invoice totals: {e}"))?;

        if let Some(prev) = old_status {
            let was_settled = prev == "paid" || prev == "overpaid";
            let is_settled = new_status == "paid" || new_status == "overpaid";

            if is_settled && !was_settled {
                println!(
                    "[{}] invoice {} settled: received {} / requested {} ({})",
                    self.network_name, invoice_id, received, inv.amount_requested, new_status
                );

                // ── WEBHOOK ───────────────────────────────────────────────────
                // Invoice fully funded (amount_received >= amount_requested).
                // Fired on the *amount* threshold, independent of confirmations
                // — payment.detected/confirmed/finalized carry the confirmation
                // story. The status guard in the UPDATE above makes it once-only.
                let mut tx = pool.begin().await
                    .map_err(|e| format!("recompute_invoice_totals begin tx: {e}"))?;

                let mut fields = Map::new();
                fields.insert("AmountReceived".into(), json!(received));
                fields.insert("AmountRequested".into(), json!(inv.amount_requested));
                fields.insert("Overpaid".into(), json!(new_status == "overpaid"));

                let dedupe_key = format!("payment.finished:{}:{}", invoice_id, new_status);
                enqueue_webhook(&mut tx, invoice_id, "payment.finished", &dedupe_key, fields).await?;

                tx.commit().await
                    .map_err(|e| format!("recompute_invoice_totals commit tx: {e}"))?;
                // TODO: make the trigger policy a merchant setting:
                //       'on_detected' (fire now, current behaviour),
                //       'on_confirmed' (require every contributing payment to be
                //       merchant_confirmed first), or 'on_finalized'.
                // TODO: also decide the underpaid tolerance here (dust /
                //       rounding), currently strict >=.
                // ──────────────────────────────────────────────────────────────

            } else if !is_settled && was_settled {
                // Reorg clawed us back below the requested amount. The
                // payment.orphaned event already told the merchant why, so we
                // don't emit a second "unfinished" event here.
                // TODO: if merchants ask for it, add 'payment.reverted'.
                println!(
                    "[{}] invoice {} fell back to {} after reorg (received {})",
                    self.network_name, invoice_id, new_status, received
                );
            }
        }

        Ok(())
    }

    async fn call_rpc_single(&self, url: &str, method: &'static str, params: serde_json::Value) -> Result<String, String> {
        let payload = RpcRequest { jsonrpc: "2.0", method, params, id: 1 };

        let response = self.client.post(url).json(&payload).send().await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: RpcResponse = response.json().await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error from {url}: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    /// Fans out to every configured endpoint for this chain and only trusts a
    /// result once at least 2 of them agree. With a single-URL config (local
    /// dev, testnets where you only have one provider) it skips straight to
    /// that node — quorum only kicks in when you've actually configured >1 URL.
    async fn call_rpc(&self, method: &'static str, params: serde_json::Value) -> Result<String, String> {
        if self.rpc_urls.len() == 1 {
            return self.call_rpc_single(&self.rpc_urls[0], method, params).await;
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single(url, method, params.clone()));
        let results: Vec<Result<String, String>> = futures::future::join_all(futures).await;

        let oks: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for {method} on chain {}: only {}/{} endpoints responded. Errors: {:?}",
                self.chain_id, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        // Return the first value that at least 2 endpoints agree on.
        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok((*candidate).clone());
            }
        }

        // All responded but none matched — e.g. a 3-way split during a reorg.
        // This isn't something a "tiebreaker" can resolve (there's no majority
        // to break a tie toward), so treat it as transient and let the caller retry.
        Err(format!(
            "Quorum disagreement for {method} on chain {}: endpoints returned different values: {:?}",
            self.chain_id, oks
        ))
    }

    /// Sequential fallback. For writes and for reads about *our own* tx, quorum
    /// is wrong: two nodes disagreeing about whether they've seen a tx we just
    /// sent is normal for a few seconds.
    async fn call_rpc_fallback(&self, method: &'static str, params: serde_json::Value)
                               -> Result<serde_json::Value, String> {
        let mut errs = Vec::new();
        for url in &self.rpc_urls {
            match self.call_rpc_single_json(url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => errs.push(e),
            }
        }
        Err(format!("all endpoints failed for {method}: {errs:?}"))
    }

    /// Nonce is a DB fact, not an RPC fact. Seeded from the chain on first use.
    async fn allocate_nonce(&self, pool: &PgPool, address: &str) -> Result<u64, String> {
        let on_chain = self.call_rpc("eth_getTransactionCount", json!([address, "pending"])).await?;
        let seed = u64::from_str_radix(on_chain.trim_start_matches("0x"), 16).map_err(|e| e.to_string())?;
        let n: i64 = sqlx::query_scalar(r#"
            INSERT INTO chain_nonces (address, network_type, chain_ref, next_nonce)
            VALUES ($1, $2, $3, $4 + 1)
            ON CONFLICT (address, network_type, chain_ref) DO UPDATE
               SET next_nonce = GREATEST(chain_nonces.next_nonce, $4) + 1, updated_at = now()
            RETURNING next_nonce - 1"#)
            .bind(address).bind(self.network_type()).bind(self.chain_ref()).bind(seed as i64)
            .fetch_one(pool).await.map_err(|e| e.to_string())?;
        Ok(n as u64)
    }

    /// Same idea as `call_rpc`, but for methods whose `result` is a JSON
    /// object/array (e.g. `eth_getBlockByNumber`) rather than a plain
    /// string, which is all the existing `call_rpc`/`call_rpc_single`
    /// support. Quorum comparison here is structural (serde_json::Value's
    /// PartialEq), so key-ordering differences between providers' JSON
    /// don't cause false disagreements.
    async fn call_rpc_single_json(
        &self,
        url: &str,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let payload = RpcRequest { jsonrpc: "2.0", method, params, id: 1 };

        let response = self.client.post(url).json(&payload).send().await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: serde_json::Value = response.json().await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.get("error") {
            return Err(format!("RPC Error from {url}: {err}"));
        }

        rpc_res.get("result").cloned()
            .ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    async fn call_rpc_json(&self, method: &'static str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        if self.rpc_urls.len() == 1 {
            return self.call_rpc_single_json(&self.rpc_urls[0], method, params).await;
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single_json(url, method, params.clone()));
        let results: Vec<Result<serde_json::Value, String>> = futures::future::join_all(futures).await;

        let oks: Vec<&serde_json::Value> = results.iter().filter_map(|r| r.as_ref().ok()).collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for {method} on chain {}: only {}/{} endpoints responded. Errors: {:?}",
                self.chain_id, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok((*candidate).clone());
            }
        }

        Err(format!(
            "Quorum disagreement for {method} on chain {}: endpoints returned different values",
            self.chain_id
        ))
    }



    async fn call_rpc_single_logs(&self, url: &str, params: serde_json::Value) -> Result<Vec<Log>, String> {
        let payload = RpcRequest { jsonrpc: "2.0", method: "eth_getLogs", params, id: 1 };

        let response = self.client.post(url).json(&payload).send().await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: RpcResponseLogs = response.json().await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error from {url}: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    /// Same quorum logic as call_rpc: single-URL configs skip straight to that
    /// node; multi-URL configs fan out to all of them and only trust a log set
    /// once at least 2 endpoints return the same (order-normalized) set.
    ///
    /// `filter` is a raw eth_getLogs filter object, e.g.:
    ///   serde_json::json!({
    ///       "address": vault_address,
    ///       "topics": [payment_topic0],
    ///       "fromBlock": "0x...",
    ///       "toBlock": "latest"
    ///   })
    pub async fn get_logs(&self, filter: serde_json::Value) -> Result<Vec<Log>, String> {
        let params = serde_json::Value::Array(vec![filter]);

        if self.rpc_urls.len() == 1 {
            return self.call_rpc_single_logs(&self.rpc_urls[0], params).await;
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single_logs(url, params.clone()));
        let results: Vec<Result<Vec<Log>, String>> = futures::future::join_all(futures).await;

        let oks: Vec<Vec<Log>> = results.iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|logs| {
                let mut sorted = logs.clone();
                sorted.sort_by_key(|l| (hex_to_u64(&l.block_number), hex_to_u64(&l.log_index)));
                sorted
            })
            .collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for eth_getLogs on chain {}: only {}/{} endpoints responded. Errors: {:?}",
                self.chain_id, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok(candidate.clone());
            }
        }

        Err(format!(
            "Quorum disagreement for eth_getLogs on chain {}: endpoints returned different log sets ({} responses, no 2 matched)",
            self.chain_id, oks.len()
        ))
    }


    /// Internal parser to get raw integer units directly from hexadecimal outputs
    fn parse_hex_balance(hex_str: &str) -> Result<Amount, String> {
        let clean_hex = hex_str.trim_start_matches("0x");
        if clean_hex.is_empty() {
            return Ok(Amount(0));
        }

        let raw_units = u128::from_str_radix(clean_hex, 16)
            .map_err(|_| "Failed to parse hex balance".to_string())?;

        Ok(Amount(raw_units))
    }

    /// Fork detection for a SPARSE history. The logs scanner only anchors one
    /// header per getLogs chunk, so unlike detect_fork_point we can't demand a
    /// remembered hash at every height — we walk back through the anchors we
    /// actually have and find the highest one still canonical.
    async fn detect_fork_point_sparse(
        &self,
        pool: &PgPool,
        scope: &str,
        last_block: i64,
        last_hash: &str,
    ) -> Result<Option<i64>, String> {
        // Fast path: cursor block still canonical => no reorg.
        if let Some(b) = self.get_block(last_block as u64, false).await? {
            if b.hash == last_hash {
                return Ok(None);
            }
        }

        let floor = (last_block - MAX_REORG_DEPTH as i64).max(0);

        let anchors = sqlx::query_as::<_, (i64, String)>(
            r#"
            SELECT block_number, block_hash FROM network_seen_blocks
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3
               AND block_number < $4 AND block_number >= $5
             ORDER BY block_number DESC
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(scope)
            .bind(last_block).bind(floor)
            .fetch_all(pool).await
            .map_err(|e| format!("detect_fork_point_sparse({scope}): {e}"))?;

        for (n, ours) in anchors {
            match self.get_block(n as u64, false).await? {
                Some(b) if b.hash == ours => return Ok(Some(n)),
                _ => continue,
            }
        }

        // No surviving anchor within the window. Same posture as the dense
        // detector: clamp and rescan; TODO raise an operational alert instead.
        eprintln!(
            "[{}] {} reorg deeper than MAX_REORG_DEPTH ({} blocks) — clamping to {}",
            self.network_name, scope, MAX_REORG_DEPTH, floor
        );
        Ok(Some(floor))
    }

    pub async fn watch_logs(&self, pool: &PgPool) -> Result<(), String> {
        let Some(contract) = self.contract_address.as_deref() else {
            // e.g. POLYGON_MAINNET_CONTRACT_ADDRESS="" — vault not deployed
            // here (yet). Native watching still runs; there's just no contract
            // to watch, so exit instead of spinning.
            println!(
                "EVMNetwork::watch_logs: no contract address for {} ({}), service not started",
                self.network_name, self.chain_id
            );
            return Ok(());
        };
        let contract_lc = contract.to_lowercase();

        println!(
            "EVMNetwork::watch_logs service started for {} ({}) on vault {} topic0 {}",
            self.network_name, self.chain_id, contract_lc, payment_topic0()
        );

        loop {
            if let Err(e) = self.tick_logs(pool, &contract_lc).await {
                // Same posture as watch_addresses: cursor only advances on
                // success, so failures are safe to just retry next tick.
                eprintln!(
                    "EVMNetwork::watch_logs tick failed [{}]: {e}",
                    self.network_name
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    async fn tick_logs(&self, pool: &PgPool, contract_lc: &str) -> Result<(), String> {
        let watched = self.load_watched_invoices(pool).await?;
        let by_id: HashMap<Uuid, WatchedInvoice> =
            watched.iter().map(|w| (w.invoice_id, w.clone())).collect();

        let tip = self.get_block_number().await? as i64;
        let scan_ceiling = (tip - 1).max(0);
        let plan = self.load_scan_plan(pool, tip).await?;

        // 1. Resume point.
        let mut cursor = match self.load_cursor(pool, SCAN_SCOPE_LOGS).await? {
            Some(c) => c,
            None => {
                let from = plan
                    .first()
                    .map(|r| (r.from - 1).max(0))
                    .unwrap_or(scan_ceiling);
                println!(
                    "[{}] no logs scan cursor, cold-starting at block {}",
                    self.network_name,
                    from + 1
                );
                self.save_cursor(pool, SCAN_SCOPE_LOGS, from, "").await?;
                (from, String::new())
            }
        };

        // 2. Reorg check + unwind (sparse detector). (unchanged)
        {
            let (last_block, last_hash) = cursor.clone();
            if !last_hash.is_empty() {
                if let Some(fork_point) = self
                    .detect_fork_point_sparse(pool, SCAN_SCOPE_LOGS, last_block, &last_hash)
                    .await?
                {
                    if fork_point < last_block {
                        println!(
                            "[{}] logs reorg detected: cursor was {}, rewinding to {}",
                            self.network_name, last_block, fork_point
                        );
                        self.handle_reorg(pool, fork_point, &watched).await?;
                        let fork_hash = self
                            .our_hash_at(pool, SCAN_SCOPE_LOGS, fork_point)
                            .await?
                            .unwrap_or_default();
                        self.save_cursor(pool, SCAN_SCOPE_LOGS, fork_point, &fork_hash).await?;
                        cursor = (fork_point, fork_hash);
                    }
                }
            }
        }

        let (mut last_block, _) = cursor;

        // 3. Skip dead space before spending budget.
        match plan_next_block(&plan, last_block + 1) {
            None => {
                if scan_ceiling > last_block {
                    let (b, _) = self
                        .fast_forward_cursor(pool, SCAN_SCOPE_LOGS, scan_ceiling)
                        .await?;
                    last_block = b;
                }
                self.refresh_confirmations(pool, tip, &watched).await?;
                self.prune_seen_blocks(pool, SCAN_SCOPE_LOGS, last_block).await?;
                return Ok(());
            }
            Some(n) if n > last_block + 1 => {
                let jump_to = (n - 1).min(scan_ceiling);
                if jump_to > last_block {
                    let (b, _) = self
                        .fast_forward_cursor(pool, SCAN_SCOPE_LOGS, jump_to).await?;
                    last_block = b;
                }
            }
            _ => {}
        }

        // 4. Batched log search inside the plan.
        let mut scanned: u64 = 0;
        let mut from = last_block + 1;

        while scanned < MAX_BLOCKS_PER_TICK && from <= scan_ceiling {
            let range_end = plan_range_end(&plan, from, scan_ceiling);
            let budget_end = from + (MAX_BLOCKS_PER_TICK - scanned) as i64 - 1;
            let to = *[
                scan_ceiling,
                range_end,
                budget_end,
                from + MAX_LOG_BLOCK_RANGE as i64 - 1,
            ]
                .iter()
                .min()
                .unwrap();

            let filter = serde_json::json!({
                "address": contract_lc,
                "topics": [payment_topic0()],
                "fromBlock": format!("0x{:x}", from),
                "toBlock":   format!("0x{:x}", to),
            });
            let logs = self.get_logs(filter).await?;

            // Anchor fetched AFTER the logs on purpose: a reorg landing between
            // the two calls leaves an anchor that won't match canonical next
            // tick, so we rewind and rescan.
            let anchor = match self.get_block(to as u64, false).await? {
                Some(b) => b,
                None => break, // provider lagging the tip
            };

            // One header per distinct block so recognition gets the real
            // block_time (occurred_at_exact = true) instead of now().
            let mut block_times: HashMap<u64, Option<DateTime<Utc>>> = HashMap::new();
            for log in &logs {
                let n = hex_to_u64(&log.block_number);
                if !block_times.contains_key(&n) {
                    let t = self.get_block(n, false).await?.and_then(|b| b.block_time());
                    block_times.insert(n, t);
                }
            }

            for log in &logs {
                let bt = block_times
                    .get(&hex_to_u64(&log.block_number))
                    .copied()
                    .flatten();
                self.apply_payment_log(pool, log, &by_id, contract_lc, bt).await?;
            }

            self.remember_block(pool, SCAN_SCOPE_LOGS, &anchor).await?;
            self.save_cursor(pool, SCAN_SCOPE_LOGS, to, &anchor.hash).await?;

            scanned += (to - from + 1) as u64;
            last_block = to;
            from = to + 1;

            if from > range_end {
                match plan_next_block(&plan, from) {
                    Some(next_from) if next_from > from => {
                        let jump_to = (next_from - 1).min(scan_ceiling);
                        if jump_to > last_block {
                            let (b, _) = self
                                .fast_forward_cursor(pool, SCAN_SCOPE_LOGS, jump_to).await?;
                            last_block = b;
                            from = last_block + 1;
                        } else {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }

        // 5. Confirmations off the DB against the current tip. (unchanged)
        self.refresh_confirmations(pool, tip, &watched).await?;
        self.prune_seen_blocks(pool, SCAN_SCOPE_LOGS, last_block).await?;
        Ok(())
    }

    async fn fee_params(&self) -> Result<(u128, u128), String> {
        let block = self.call_rpc_json("eth_getBlockByNumber", json!(["latest", false])).await?;
        let base = block["baseFeePerGas"].as_str()
            .map(|h| hex_to_u128(h.trim_start_matches("0x")).unwrap_or(0))
            .unwrap_or(0);
        let priority = match self.call_rpc("eth_maxPriorityFeePerGas", json!([])).await {
            Ok(h) => hex_to_u128(h.trim_start_matches("0x")).unwrap_or(1_000_000_000),
            Err(_) => 1_000_000_000, // 1 gwei; pre-London / stingy providers
        };
        Ok((priority, base * 2 + priority))
    }

    async fn estimate_gas(&self, from: &str, to: &str, value: u128, data: &[u8]) -> Result<u64, String> {
        let h = self.call_rpc("eth_estimateGas", json!([{
            "from": from, "to": to,
            "value": format!("0x{value:x}"),
            "data": format!("0x{}", hex::encode(data)),
        }])).await?;
        let g = u64::from_str_radix(h.trim_start_matches("0x"), 16).map_err(|e| e.to_string())?;
        Ok(g + g / 5) // 20% headroom
    }

}

#[async_trait]
impl NetworkClient for EVMNetwork {
    fn derivation_scheme(&self) -> DerivationScheme { evm_scheme() }
    fn required_wallets(&self) -> &'static [WalletSpec] { EVM_REQUIRED_WALLETS }

    fn canonicalize_address(&self, address: &str) -> String { address.to_lowercase() }

    fn network_type(&self) -> &'static str {
        crate::assets::NETWORK_EVM
    }

    fn chain_ref(&self) -> String {
        self.chain_id.to_string()
    }
    fn gas_model(&self) -> GasModel { GasModel::SelfFunded }
    fn derive(&self, mnemonic: &str, role: KeyRole, index: u32)
              -> Result<DerivedAddress, String>
    {
        let scheme = evm_scheme();
        let (_key, address) = derive_evm_keypair(mnemonic, role, index)?;
        Ok(DerivedAddress {
            address: self.canonicalize_address(&address),
            role,
            index,
            path: scheme.path_string(role, index),
            scheme_version: scheme.version,
            reference: None,
        })
    }

    async fn next_deposit_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<DerivedAddress, String> {
        let index =
            allocate_deposit_index(pool, merchant_id, self.network_type()).await?;
        let mut derived = self.derive(mnemonic, KeyRole::Deposit, index)?;
        derived.reference = Some(format!("0x{}", hex::encode(invoice_id.as_bytes())));
        Ok(derived)
    }

    fn validate_address(&self, address: &str) -> bool {
        let clean_addr = address.trim_start_matches("0x");

        if clean_addr.len() != 40 {
            return false;
        }

        clean_addr.chars().all(|c| c.is_ascii_hexdigit())
    }

    // --- CHAIN STATE METHODS ---

    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        let hex_balance = self.call_rpc("eth_getBalance", json!([address, "latest"])).await?;
        Self::parse_hex_balance(&hex_balance)
    }

    async fn get_token_balance(&self, token_address: &str, address: &str, _decimals: u8) -> Result<Amount, String> {
        let clean_addr = address.trim_start_matches("0x");
        let data = format!("0x70a08231{:0>64}", clean_addr);
        let params = json!([{ "to": token_address, "data": data }, "latest"]);
        let hex_balance = self.call_rpc("eth_call", params).await?;
        Self::parse_hex_balance(&hex_balance)
    }

    async fn get_current_block(&self) -> Result<u64, String> {
        let hex_block = self.call_rpc("eth_blockNumber", json!([])).await?;
        let clean_hex = hex_block.trim_start_matches("0x");

        u64::from_str_radix(clean_hex, 16)
            .map_err(|_| "Failed to parse hex block number".to_string())
    }

    async fn build_and_sign(&self, pool: &PgPool, mnemonic: &str, req: &TransferRequest)
                            -> Result<SignedTransfer, String>
    {
        if req.fee_payer.is_some() {
            return Err("EVM is SelfFunded: fee_payer must be None".into());
        }
        let (key, derived_addr) = derive_evm_keypair(mnemonic, req.from.authority.role, req.from.authority.index)?;
        if derived_addr.to_lowercase() != req.from.address.to_lowercase() {
            return Err(format!(
                "authority {} derives {} but from_address is {} — refusing to sign",
                req.from.authority.to_ref(), derived_addr, req.from.address));
        }
        let from = &req.from.address;
        let to20 = parse_address(&req.to)?;
        let (priority, max_fee) = self.fee_params().await?;

        let (tx_to, value, data, amount) = match &req.asset.address {
            None => {
                // Native. Gas for a plain send is fixed; estimate anyway in case `to` is a contract.
                let gas = self.estimate_gas(from, &req.to, 1, &[]).await?;
                let amount = match req.amount {
                    TransferAmount::Exact(n) => n,
                    TransferAmount::Max => {
                        let bal = self.get_native_balance(from).await?.0;
                        let reserve = gas as u128 * max_fee;
                        bal.checked_sub(reserve)
                            .ok_or_else(|| format!("{from}: balance {bal} < gas reserve {reserve}"))?
                    }
                };
                (to20, amount, Vec::new(), amount)
            }
            Some(token) => {
                let amount = match req.amount {
                    TransferAmount::Exact(n) => n,
                    TransferAmount::Max => self.get_token_balance(token, from, 0).await?.0,
                };
                if amount == 0 { return Err(format!("{from}: nothing to sweep for {token}")); }
                (parse_address(token)?, 0, erc20_transfer_calldata(&to20, amount), amount)
            }
        };

        let gas_limit = self.estimate_gas(
            from, &format!("0x{}", hex::encode(tx_to)), value, &data).await?;

        // Gas must be there NOW. If not, fail before touching the nonce counter.
        let native = self.get_native_balance(from).await?.0;
        let needed = value + gas_limit as u128 * max_fee;
        if native < needed {
            return Err(format!("{from}: has {native} wei, needs {needed} (gas refill required)"));
        }

        let nonce = self.allocate_nonce(pool, from).await?;
        let tx = Eip1559Tx {
            chain_id: self.chain_id, nonce, max_priority_fee: priority, max_fee,
            gas_limit, to: tx_to, value, data,
        };
        let (raw, tx_hash) = tx.sign(&key)?;

        Ok(SignedTransfer {
            tx_hash, raw, amount,
            from: from.clone(),
            valid_until: None,
            nonce: Some(nonce),
            fee_estimate: Some(gas_limit as u128 * max_fee),
        })
    }

    async fn broadcast(&self, signed: &SignedTransfer) -> Result<(), String> {
        let raw = format!("0x{}", hex::encode(&signed.raw));
        match self.call_rpc_fallback("eth_sendRawTransaction", json!([raw])).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let l = e.to_lowercase();
                // Idempotent: node already has it, or it already mined.
                if l.contains("already known") || l.contains("known transaction")
                    || l.contains("nonce too low") || l.contains("already imported") {
                    Ok(())
                } else { Err(e) }
            }
        }
    }

    async fn transfer_status(&self, signed: &SignedTransfer) -> Result<TransferStatus, String> {
        let receipt = self.call_rpc_fallback("eth_getTransactionReceipt", json!([signed.tx_hash])).await?;
        if !receipt.is_null() {
            let block = hex_to_u64(receipt["blockNumber"].as_str().ok_or("receipt: no blockNumber")?);
            let gas_used = hex_to_u128(receipt["gasUsed"].as_str().unwrap_or("0x0").trim_start_matches("0x")).unwrap_or(0);
            let price = hex_to_u128(receipt["effectiveGasPrice"].as_str().unwrap_or("0x0").trim_start_matches("0x")).unwrap_or(0);
            let fee_paid = gas_used * price;
            return Ok(match receipt["status"].as_str() {
                Some("0x1") => TransferStatus::Confirmed { block, fee_paid },
                _ => TransferStatus::Failed { reason: format!("reverted in block {block}") },
            });
        }

        // No receipt. In the mempool?
        let tx = self.call_rpc_fallback("eth_getTransactionByHash", json!([signed.tx_hash])).await?;
        if !tx.is_null() { return Ok(TransferStatus::Pending); }

        // Not in the mempool. Only expired if the nonce was spent by something else.
        if let Some(nonce) = signed.nonce {
            let count = self.call_rpc_fallback(
                "eth_getTransactionCount",
                json!([signed.from, "latest"]),
            ).await?;
            let count = u64::from_str_radix(
                count.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16,
            ).unwrap_or(0);
            if count > nonce { return Ok(TransferStatus::Expired); }
        }
        Ok(TransferStatus::Unknown)
    }

    // --- BATCHED WATCHING METHODS ---

    async fn spin_up(&self, pool: &PgPool) -> Result<(), String> {
        println!("EVMNetwork::spin_up initializing for {} ({})",
                 self.network_name, self.chain_id);

        // 0. Code and DB must agree on the scheme before anything derives.
        assert_scheme(pool, &self.derivation_scheme()).await?;

        // 1. Main wallet + gas feeder for every merchant missing either.
        ensure_merchant_wallets(pool, self).await?;

        let (addresses_res, logs_res) =
            tokio::join!(self.watch_addresses(pool), self.watch_logs(pool));
        addresses_res?;
        logs_res?;
        Ok(())
    }
}


fn rlp_bytes(b: &[u8], out: &mut Vec<u8>) {
    match b.len() {
        1 if b[0] < 0x80 => out.push(b[0]),
        n if n < 56 => { out.push(0x80 + n as u8); out.extend_from_slice(b); }
        n => {
            let len = n.to_be_bytes();
            let len = &len[len.iter().position(|&x| x != 0).unwrap()..];
            out.push(0xb7 + len.len() as u8);
            out.extend_from_slice(len);
            out.extend_from_slice(b);
        }
    }
}

fn rlp_uint(v: u128, out: &mut Vec<u8>) {
    if v == 0 { out.push(0x80); return; }
    let be = v.to_be_bytes();
    rlp_bytes(&be[be.iter().position(|&x| x != 0).unwrap()..], out);
}

fn rlp_list(payload: &[u8], out: &mut Vec<u8>) {
    match payload.len() {
        n if n < 56 => out.push(0xc0 + n as u8),
        n => {
            let len = n.to_be_bytes();
            let len = &len[len.iter().position(|&x| x != 0).unwrap()..];
            out.push(0xf7 + len.len() as u8);
            out.extend_from_slice(len);
        }
    }
    out.extend_from_slice(payload);
}

pub struct Eip1559Tx {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee: u128,
    pub max_fee: u128,
    pub gas_limit: u64,
    pub to: [u8; 20],
    pub value: u128,
    pub data: Vec<u8>,
}

impl Eip1559Tx {
    fn fields(&self, out: &mut Vec<u8>) {
        rlp_uint(self.chain_id as u128, out);
        rlp_uint(self.nonce as u128, out);
        rlp_uint(self.max_priority_fee, out);
        rlp_uint(self.max_fee, out);
        rlp_uint(self.gas_limit as u128, out);
        rlp_bytes(&self.to, out);
        rlp_uint(self.value, out);
        rlp_bytes(&self.data, out);
        rlp_list(&[], out); // access list
    }

    /// Returns (raw_signed_tx, tx_hash_hex).
    pub fn sign(&self, key: &SigningKey) -> Result<(Vec<u8>, String), String> {
        let mut payload = Vec::new();
        self.fields(&mut payload);
        let mut unsigned = vec![0x02];
        rlp_list(&payload, &mut unsigned);
        let sighash = Keccak256::digest(&unsigned);

        let (sig, rec) = key
            .sign_prehash_recoverable(&sighash)
            .map_err(|e| format!("secp256k1 sign: {e}"))?;
        let r = sig.r().to_bytes();
        let s = sig.s().to_bytes();

        rlp_uint(rec.to_byte() as u128, &mut payload);
        rlp_bytes(strip(&r), &mut payload);
        rlp_bytes(strip(&s), &mut payload);

        let mut raw = vec![0x02];
        rlp_list(&payload, &mut raw);
        let hash = format!("0x{}", hex::encode(Keccak256::digest(&raw)));
        Ok((raw, hash))
    }
}

fn strip(b: &[u8]) -> &[u8] {
    match b.iter().position(|&x| x != 0) { Some(i) => &b[i..], None => &[] }
}

pub fn parse_address(s: &str) -> Result<[u8; 20], String> {
    let b = hex::decode(s.trim_start_matches("0x")).map_err(|e| format!("address {s}: {e}"))?;
    b.try_into().map_err(|_| format!("address {s}: not 20 bytes"))
}

/// transfer(address,uint256)
pub fn erc20_transfer_calldata(to: &[u8; 20], amount: u128) -> Vec<u8> {
    let mut d = vec![0xa9, 0x05, 0x9c, 0xbb];
    d.extend_from_slice(&[0u8; 12]); d.extend_from_slice(to);
    d.extend_from_slice(&[0u8; 16]); d.extend_from_slice(&amount.to_be_bytes());
    d
}