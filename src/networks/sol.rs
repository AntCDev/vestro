use super::{decrypt_data, enqueue_webhook, Amount, NetworkClient, PaymentWatch, SolanaCluster};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{sleep};
use std::time::Instant;

use ed25519_dalek::SigningKey;
use hmac::{Hmac, KeyInit, Mac}; // Added KeyInit here
use sha2::{Sha256, Sha512, Digest};
use sqlx::PgPool;

use bip39::Mnemonic;
use curve25519_dalek::edwards::CompressedEdwardsY;
use rust_decimal::Decimal;



use futures::stream::{self, StreamExt};
use serde_json::{json, Map, Value};
use tokio::sync::RwLock;

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
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Deserialize)]
struct SolBalanceValue {
    value: u64, // Lamports
}

#[derive(Deserialize)]
struct SolTokenAccountsValue {
    value: Vec<serde_json::Value>,
}


type HmacSha512 = Hmac<Sha512>;
const SOLANA_HARDENED_OFFSET: u32 = 0x8000_0000;
const PDA_MARKER: &[u8] = b"ProgramDerivedAddress";
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

// ---------- derivation path ----------

/// Phantom/Solflare-style hardened path: m/44'/501'/{index}'/0'
fn get_solana_derivation_path(index: u32) -> String {
    format!("m/44'/501'/{}'/0'", index)
}

/// SLIP-0010 only supports hardened derivation for ed25519, so every
/// segment must end in `'`. Returns the raw (unhardened) u32 for each segment.
fn parse_hardened_path(path: &str) -> Result<Vec<u32>, String> {
    path.trim_start_matches("m/")
        .split('/')
        .map(|segment| {
            if !segment.ends_with('\'') {
                return Err(format!(
                    "SLIP-0010 ed25519 requires hardened segments, got: {}",
                    segment
                ));
            }
            let raw = segment
                .trim_end_matches('\'')
                .parse::<u32>()
                .map_err(|_| format!("Invalid path segment: {}", segment))?;
            if raw >= SOLANA_HARDENED_OFFSET {
                return Err(format!("Path segment out of range: {}", segment));
            }
            Ok(raw)
        })
        .collect()
}

// ---------- SLIP-0010 ed25519 key derivation ----------

fn slip10_master_key(seed: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut mac = HmacSha512::new_from_slice(b"ed25519 seed")
        .expect("HMAC accepts a key of any size");
    mac.update(seed);
    let result = mac.finalize().into_bytes();

    let mut key = [0u8; 32];
    let mut chain_code = [0u8; 32];
    key.copy_from_slice(&result[0..32]);
    chain_code.copy_from_slice(&result[32..64]);
    (key, chain_code)
}

fn slip10_derive_child(key: &[u8; 32], chain_code: &[u8; 32], index: u32) -> ([u8; 32], [u8; 32]) {
    let hardened_index = index | SOLANA_HARDENED_OFFSET;
    let mut mac = HmacSha512::new_from_slice(chain_code)
        .expect("HMAC accepts a key of any size");
    mac.update(&[0u8]); // ed25519 SLIP-0010: 0x00 || private_key || ser32(index)
    mac.update(key);
    mac.update(&hardened_index.to_be_bytes());
    let result = mac.finalize().into_bytes();

    let mut child_key = [0u8; 32];
    let mut child_chain_code = [0u8; 32];
    child_key.copy_from_slice(&result[0..32]);
    child_chain_code.copy_from_slice(&result[32..64]);
    (child_key, child_chain_code)
}

/// Walks m/44'/501'/{index}'/0' via SLIP-0010 and returns the ed25519 signing key.
fn derive_solana_signing_key(mnemonic: &str, index: u32) -> Result<SigningKey, String> {
    let mnemonic_parsed = Mnemonic::parse(mnemonic).map_err(|e| format!("Invalid mnemonic: {}", e))?;
    let seed = mnemonic_parsed.to_seed("");

    let path_str = get_solana_derivation_path(index);
    let segments = parse_hardened_path(&path_str)?;

    let (mut key, mut chain_code) = slip10_master_key(&seed);
    for segment in segments {
        let (child_key, child_chain_code) = slip10_derive_child(&key, &chain_code, segment);
        key = child_key;
        chain_code = child_chain_code;
    }

    Ok(SigningKey::from_bytes(&key))
}

/// Derives the base58-encoded wallet (owner) address for a given index.
fn derive_solana_address(mnemonic: &str, index: u32) -> Result<String, String> {
    let signing_key = derive_solana_signing_key(mnemonic, index)?;
    let public_key_bytes = signing_key.verifying_key().to_bytes();
    Ok(bs58::encode(public_key_bytes).into_string())
}

// ---------- ATA / PDA derivation (no solana-* crates) ----------

fn decode_pubkey(address: &str) -> Result<[u8; 32], String> {
    let bytes = bs58::decode(address)
        .into_vec()
        .map_err(|e| format!("Invalid base58 address '{}': {}", address, e))?;
    bytes
        .try_into()
        .map_err(|_| format!("Address '{}' is not a valid 32-byte pubkey", address))
}

/// A candidate PDA is only valid if it does NOT lie on the ed25519 curve.
fn is_on_curve(bytes: &[u8; 32]) -> bool {
    CompressedEdwardsY(*bytes).decompress().is_some()
}

/// Standalone reimplementation of `Pubkey::find_program_address`.
fn find_program_address(seeds: &[&[u8]], program_id: &[u8; 32]) -> Result<([u8; 32], u8), String> {
    for bump in (0..=255u8).rev() {
        let mut hasher = Sha256::new();
        for seed in seeds {
            hasher.update(seed);
        }
        hasher.update(&[bump]);
        hasher.update(program_id);
        hasher.update(PDA_MARKER);
        let hash = hasher.finalize();

        let mut candidate = [0u8; 32];
        candidate.copy_from_slice(&hash);

        if !is_on_curve(&candidate) {
            return Ok((candidate, bump));
        }
    }
    Err("Unable to find a valid program derived address".to_string())
}

/// Derives the Associated Token Account address for `owner_address` + `mint_address`.
fn derive_associated_token_address(
    owner_address: &str,
    mint_address: &str,
    token_program_id: &str,
) -> Result<String, String> {
    let owner_bytes = decode_pubkey(owner_address)?;
    let mint_bytes = decode_pubkey(mint_address)?;
    let token_program_bytes = decode_pubkey(token_program_id)?;
    let associated_token_program_bytes = decode_pubkey(ASSOCIATED_TOKEN_PROGRAM_ID)?;

    // Identical for legacy SPL and Token-2022: the program id is a seed, so the
    // derivation needs no special casing. Token-2022 extensions (transfer fees,
    // hooks) are ignored beyond this point by design — balance deltas already
    // report what actually landed, whatever was skimmed on the way.
    let seeds: [&[u8]; 3] = [&owner_bytes, &token_program_bytes, &mint_bytes];
    let (ata_bytes, _bump) = find_program_address(&seeds, &associated_token_program_bytes)?;

    Ok(bs58::encode(ata_bytes).into_string())
}

fn get_derivation_path(index: u32) -> String {
    format!("m/44'/501'/{}'/0'", index)
}
fn parse_derivation_path(path: &str) -> Result<Vec<u32>, String> {
    if !path.starts_with("m/") {
        return Err("Path must start with 'm/'".to_string());
    }
    let mut indices = Vec::new();
    for part in path["m/".len()..].split('/') {
        if part.is_empty() { continue; }
        let is_hardened = part.ends_with('\'');
        let num_str = if is_hardened {
            &part[..part.len() - 1]
        } else {
            part
        };
        let val: u32 = num_str.parse().map_err(|e| format!("Invalid path segment: {}", e))?;
        if is_hardened {
            indices.push(val | 0x8000_0000);
        } else {
            indices.push(val);
        }
    }
    Ok(indices)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tunables
// ─────────────────────────────────────────────────────────────────────────────

/// The one network string. `invoices.network_type`, `merchant_wallets.network_type`,
/// `merchant_network_indices.network` and `network_address_cursors.network_type`
/// all use it. Never 'sol', never 'SOL'.
const NETWORK_TYPE: &str = "solana";

const POLL_INTERVAL_SECS: u64 = 2;
const SIG_PAGE_LIMIT: usize = 1000;

/// Raised from 5. Truncation is no longer silent (see `SigScan::complete`), but
/// a truncated scan still leaves an unreachable gap below the oldest signature
/// fetched, so the cap wants headroom over any plausible burst.
const MAX_SIG_PAGES_PER_ADDRESS: usize = 20;

const MAX_TX_PER_BATCH: usize = 50;
const MAX_STATUS_PER_BATCH: usize = 256;
const ADDRESS_CONCURRENCY: usize = 8;

/// Hard ceiling on how many invoices one tick will watch. Two addresses per
/// invoice at worst, so this is the real bound on RPC calls per tick.
const MAX_WATCHED_INVOICES: i64 = 5_000;

/// Slack on the `created_block` floor. The finalized slot read at invoice
/// creation can still sit ahead of where the payer's transaction lands if the
/// payer's wallet submitted through a node on a different fork tip. Without
/// margin those payments are thrown away as "older than the invoice".
const CREATED_SLOT_MARGIN: i64 = 64;

/// Signatures newer than this many slots are still in the node's recent-status
/// cache, so `searchTransactionHistory` is unnecessary (and expensive) for them.
const RECENT_STATUS_WINDOW_SLOTS: i64 = 300;

const CONF_DETECTED: i64 = 1;
const CONF_CONFIRMED: i64 = 16;
const CONF_FINALIZED: i64 = 32;

const DETECT_COMMITMENT: &str = "confirmed";
const FINALIZED_COMMITMENT: &str = "finalized";

// ─────────────────────────────────────────────────────────────────────────────
// Blockhash
// ─────────────────────────────────────────────────────────────────────────────
pub const BLOCKHASH_COMMITMENT: &str = "confirmed";
const BLOCKHASH_CACHE_TTL: Duration = Duration::from_secs(3);
#[derive(Clone, Debug)]
pub struct RecentBlockhash {
    pub blockhash: String,
    pub last_valid_block_height: u64,
}
// ─────────────────────────────────────────────────────────────────────────────
// Confirmation levels
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum ConfirmLevel {
    Detected = 0,
    Confirmed = 1,
    Finalized = 2,
}

impl ConfirmLevel {
    fn from_required(required: i64) -> Self {
        if required >= CONF_FINALIZED {
            Self::Finalized
        } else if required >= 2 {
            Self::Confirmed
        } else {
            Self::Detected
        }
    }

    fn from_rpc(s: &str) -> Option<Self> {
        match s {
            "processed" => Some(Self::Detected),
            "confirmed" => Some(Self::Confirmed),
            "finalized" => Some(Self::Finalized),
            _ => None,
        }
    }

    fn as_confirmations(self) -> i64 {
        match self {
            Self::Detected => CONF_DETECTED,
            Self::Confirmed => CONF_CONFIRMED,
            Self::Finalized => CONF_FINALIZED,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Watched state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct WatchedInvoice {
    invoice_id: Uuid,
    merchant_id: Uuid,
    /// Naive-path target. Native: the HD-derived pubkey. SPL: that pubkey's ATA
    /// for `mint`, which is what the payer's wallet will build on its side.
    deposit_address: String,
    /// Smart-path key: the HD-derived pubkey, attached read-only. For a native
    /// invoice this is byte-identical to `deposit_address`; that is intentional
    /// and every place it matters is handled explicitly below.
    payment_reference: String,
    /// Smart-path credit target: merchant main wallet (native) or that wallet's
    /// ATA for `mint` (SPL). Empty => reference path disabled for this invoice.
    merchant_target: String,
    amount_requested: Decimal,
    /// None => native SOL.
    mint: Option<String>,
    level: ConfirmLevel,
    created_slot: Option<i64>,
}

impl WatchedInvoice {
    /// Address feeds that can carry money for this invoice, deduped.
    ///
    /// Native invoices collapse to a single address, so we don't poll the same
    /// feed twice and don't double-list it in the prune whitelist.
    ///
    /// `merchant_target` is deliberately absent. The smart path is discovered
    /// through the reference key, which the payer's transaction names; polling
    /// the merchant's main wallet instead would drag every unrelated movement on
    /// that wallet through this loop.
    fn watch_addresses(&self) -> Vec<&str> {
        let mut v: Vec<&str> = Vec::with_capacity(2);
        if !self.deposit_address.is_empty() {
            v.push(&self.deposit_address);
        }
        if !self.payment_reference.is_empty() && self.payment_reference != self.deposit_address {
            v.push(&self.payment_reference);
        }
        v
    }

    /// A transaction older than the invoice cannot be paying it. Margin absorbs
    /// slot skew between whatever node stamped `created_block` and whatever node
    /// the payer's wallet submitted through.
    fn slot_is_plausible(&self, slot: i64) -> bool {
        match self.created_slot {
            Some(c) => slot >= c - CREATED_SLOT_MARGIN,
            None => true,
        }
    }
}

/// Address -> invoice lookups, built once per tick.
///
/// `classify` used to scan the whole watch set for every transaction it fetched.
/// At the 5 000-invoice cap that is 5 000 string comparisons per transaction per
/// tick for no reason: a transaction can only pay an invoice whose address it
/// actually names, and it names at most a few dozen accounts.
struct WatchIndex<'a> {
    by_id: HashMap<Uuid, &'a WatchedInvoice>,
    by_deposit: HashMap<&'a str, Vec<&'a WatchedInvoice>>,
    by_reference: HashMap<&'a str, Vec<&'a WatchedInvoice>>,
}

impl<'a> WatchIndex<'a> {
    fn build(watched: &'a [WatchedInvoice]) -> Self {
        let mut by_id = HashMap::with_capacity(watched.len());
        let mut by_deposit: HashMap<&str, Vec<&WatchedInvoice>> = HashMap::new();
        let mut by_reference: HashMap<&str, Vec<&WatchedInvoice>> = HashMap::new();

        for inv in watched {
            by_id.insert(inv.invoice_id, inv);

            if !inv.deposit_address.is_empty() {
                by_deposit.entry(inv.deposit_address.as_str()).or_default().push(inv);
            }
            // No merchant_target means the reference path is dead for this
            // invoice, so it never belongs in the reference index.
            if !inv.payment_reference.is_empty() && !inv.merchant_target.is_empty() {
                by_reference.entry(inv.payment_reference.as_str()).or_default().push(inv);
            }
        }

        Self { by_id, by_deposit, by_reference }
    }
}

/// A fetched transaction reduced to balance movements.
///
/// Deltas rather than parsed instructions: a payer can move tokens with
/// `transfer`, `transferChecked`, a CPI from an aggregator, or several
/// instructions at once. Pre/post balances cover all of them and can't be
/// spoofed by instruction shape.
struct TxView {
    signature: String,
    slot: i64,
    failed: bool,
    account_keys: HashSet<String>,
    /// Keys that signed. A Solana Pay `reference` is always read-only and
    /// non-signing, so this is what separates "a payer referenced our HD key"
    /// from "we swept our own HD key into the merchant wallet".
    signers: HashSet<String>,
    native_delta: HashMap<String, i128>,
    token_delta: HashMap<(String, String), i128>,
}

impl TxView {
    fn native_credit(&self, address: &str) -> i128 {
        self.native_delta.get(address).copied().unwrap_or(0)
    }

    fn token_credit(&self, address: &str, mint: &str) -> i128 {
        self.token_delta
            .get(&(address.to_string(), mint.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn credit(&self, address: &str, mint: Option<&str>) -> i128 {
        match mint {
            None => self.native_credit(address),
            Some(m) => self.token_credit(address, m),
        }
    }
}

#[derive(Clone)]
struct SigRef {
    signature: String,
    slot: i64,
    failed: bool,
}

/// Result of one address scan.
///
/// `complete` is false when paging stopped on `MAX_SIG_PAGES_PER_ADDRESS`
/// instead of on the cursor or the slot floor. The signatures we did get are
/// still processed, but the cursor must not move: advancing it would step over
/// the older signatures we never listed and lose them permanently.
struct SigScan {
    sigs: Vec<SigRef>,
    complete: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// ### NETWORK IMPLEMENTATION ###
// ═══════════════════════════════════════════════════════════════════════════

pub struct SolanaNetwork {
    cluster: SolanaCluster,
    rpc_urls: Vec<String>,
    pub network_name: String,
    client: reqwest::Client,
    blockhash_cache: RwLock<Option<(RecentBlockhash, Instant)>>,
}

impl SolanaNetwork {
    pub fn new(cluster: SolanaCluster, rpc_urls: Vec<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "SolanaNetwork requires at least one RPC URL");
        let network_name = format!("SOL_{:?}", cluster);

        Self {
            cluster,
            rpc_urls,
            network_name,
            client: reqwest::Client::new(),
            blockhash_cache: RwLock::new(None),
        }
    }

    /// Single source of truth for `chain_ref`. The invoice creation path calls
    /// this too rather than hardcoding a cluster name, or invoices land with a
    /// chain_ref the watcher's WHERE clause never matches.
    pub fn chain_ref(&self) -> String {
        match self.cluster {
            SolanaCluster::MainnetBeta => "mainnet-beta",
            SolanaCluster::Testnet     => "testnet",
            SolanaCluster::Devnet      => "devnet",
        }
            .to_string()
    }

    pub fn cluster(&self) -> SolanaCluster {
        self.cluster
    }

    /// Standard Solana cluster moniker. Safe to show a payer (e.g. a "Devnet"
    /// banner) — unlike rpc_urls, this reveals nothing about infrastructure.
    pub fn cluster_label(&self) -> &'static str {
        match self.cluster {
            SolanaCluster::MainnetBeta => "mainnet-beta",
            SolanaCluster::Testnet => "testnet",
            SolanaCluster::Devnet => "devnet",
        }
    }
    
    // ── RPC ──────────────────────────────────────────────────────────────────

    async fn rpc(&self, method: &'static str, params: Value) -> Result<Value, String> {
        let mut last_err = String::new();
        for url in &self.rpc_urls {
            match self.call_rpc_single_json(url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = e,
            }
        }
        Err(format!("[{}] all endpoints failed for {method}: {last_err}", self.network_name))
    }

    async fn rpc_batch(
        &self,
        calls: &[(&'static str, Value)],
    ) -> Result<Vec<Result<Value, String>>, String> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let payload: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| {
                json!({ "jsonrpc": "2.0", "id": i, "method": method, "params": params })
            })
            .collect();

        let mut last_err = String::new();
        for url in &self.rpc_urls {
            let resp = match self.client.post(url).json(&payload).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("HTTP request to {url} failed: {e}");
                    continue;
                }
            };

            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    last_err = format!("Failed to parse batch response from {url}: {e}");
                    continue;
                }
            };

            let Some(entries) = body.as_array() else {
                last_err = format!("Non-array batch response from {url}: {body}");
                continue;
            };

            let mut out: Vec<Result<Value, String>> =
                (0..calls.len()).map(|i| Err(format!("no response for batch id {i}"))).collect();

            for entry in entries {
                let Some(id) = entry.get("id").and_then(|v| v.as_u64()) else { continue };
                let idx = id as usize;
                if idx >= out.len() {
                    continue;
                }
                out[idx] = if let Some(err) = entry.get("error") {
                    Err(format!("RPC error from {url}: {err}"))
                } else {
                    Ok(entry.get("result").cloned().unwrap_or(Value::Null))
                };
            }

            return Ok(out);
        }

        Err(format!("[{}] all endpoints failed for batch: {last_err}", self.network_name))
    }

    async fn call_rpc_single_json(
        &self,
        url: &str,
        method: &'static str,
        params: Value,
    ) -> Result<Value, String> {
        let payload = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.get("error") {
            return Err(format!("RPC Error from {url}: {err}"));
        }

        rpc_res
            .get("result")
            .cloned()
            .ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    async fn get_slot(&self, commitment: &str) -> Result<i64, String> {
        let v = self.rpc("getSlot", json!([{ "commitment": commitment }])).await?;
        v.as_i64().ok_or_else(|| format!("getSlot returned non-integer: {v}"))
    }

    pub async fn get_recent_blockhash(&self) -> Result<RecentBlockhash, String> {
        {
            let guard = self.blockhash_cache.read().await;
            if let Some((cached, fetched_at)) = guard.as_ref() {
                if fetched_at.elapsed() < BLOCKHASH_CACHE_TTL {
                    return Ok(cached.clone());
                }
            }
        }

        let res = self
            .rpc("getLatestBlockhash", json!([{ "commitment": BLOCKHASH_COMMITMENT }]))
            .await?;

        // getLatestBlockhash wraps its payload in { context, value }.
        let value = res.get("value").unwrap_or(&res);

        let blockhash = value
            .get("blockhash")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("getLatestBlockhash returned no blockhash: {res}"))?
            .to_string();

        let last_valid_block_height = value
            .get("lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("getLatestBlockhash returned no lastValidBlockHeight: {res}"))?;

        let fresh = RecentBlockhash { blockhash, last_valid_block_height };
        *self.blockhash_cache.write().await = Some((fresh.clone(), Instant::now()));
        Ok(fresh)
    }

    // ── The service loop ─────────────────────────────────────────────────────

    pub async fn watch_addresses(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "SolanaNetwork::watch_addresses service started for {} ({}/{})",
            self.network_name,
            NETWORK_TYPE,
            self.chain_ref()
        );

        loop {
            if let Err(e) = self.tick(pool).await {
                eprintln!(
                    "SolanaNetwork::watch_addresses tick failed [{}]: {e}",
                    self.network_name
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    async fn tick(&self, pool: &PgPool) -> Result<(), String> {
        // Expiry first: an invoice whose timer ran out this tick shouldn't get
        // one more round of polling before it drops out of `watched`.
        self.expire_invoices(pool).await?;

        let watched = self.load_watched_invoices(pool).await?;

        if watched.is_empty() {
            // Nothing live. Deliberately do NOT prune here — with an empty
            // whitelist the `<> ALL` predicate matches every row and would wipe
            // the cursor table the moment the merchant has a quiet minute.
            return Ok(());
        }

        let index = WatchIndex::build(&watched);
        let finalized_slot = self.get_slot(FINALIZED_COMMITMENT).await?;
        let invoice_ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();

        // ── 1. Address set ───────────────────────────────────────────────────
        let mut addresses: HashSet<String> = HashSet::new();
        for w in &watched {
            for addr in w.watch_addresses() {
                addresses.insert(addr.to_string());
            }
        }
        let addresses: Vec<String> = addresses.into_iter().collect();

        // ── 2. Discover new signatures, one cursor per address ───────────────
        let discovered: Vec<(String, Result<SigScan, String>)> = stream::iter(addresses)
            .map(|addr| async move {
                let res = self.discover_signatures(pool, &addr).await;
                (addr, res)
            })
            .buffer_unordered(ADDRESS_CONCURRENCY)
            .collect()
            .await;

        let mut sig_slots: HashMap<String, SigRef> = HashMap::new();
        let mut per_address: HashMap<String, SigScan> = HashMap::new();

        for (addr, res) in discovered {
            match res {
                Ok(scan) => {
                    for s in &scan.sigs {
                        sig_slots.entry(s.signature.clone()).or_insert_with(|| s.clone());
                    }
                    per_address.insert(addr, scan);
                }
                Err(e) => {
                    eprintln!("[{}] signature scan failed for {addr}: {e}", self.network_name);
                }
            }
        }

        // ── 3. Skip anything already booked ──────────────────────────────────
        //
        // The unfinalized window (~32 slots) is re-listed every tick by design.
        // Without this filter that means re-fetching and re-parsing the same
        // transaction bodies every 2 seconds forever, which is most of the RPC
        // bill. Safe to skip because `apply_transaction` is all-or-nothing
        // across every invoice a transaction touches: if the signature is on
        // record and not orphaned, every credit it produced is on record too.
        //
        // Bounded by the candidate list as well as the invoice list. Without the
        // tx_hash predicate this walked every payment row belonging to every
        // watched invoice on every tick; with it, `payments_txhash_idx` does the
        // work and the result set is at most one row per candidate signature.
        let candidate_sigs: Vec<String> = sig_slots.keys().cloned().collect();

        let already: HashSet<String> = if candidate_sigs.is_empty() {
            HashSet::new()
        } else {
            sqlx::query_scalar::<_, String>(
                r#"
				SELECT DISTINCT tx_hash
				  FROM payments
				 WHERE invoice_id = ANY($1)
				   AND tx_hash = ANY($2)
				   AND status <> 'orphaned'
				"#,
            )
                .bind(&invoice_ids)
                .bind(&candidate_sigs)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("load applied signatures: {e}"))?
                .into_iter()
                .collect()
        };

        let mut to_fetch: Vec<SigRef> = sig_slots
            .into_values()
            .filter(|s| !s.failed && !already.contains(&s.signature))
            .collect();
        to_fetch.sort_by_key(|s| s.slot);

        // Seed with `already` so the cursor logic below treats known signatures
        // as satisfied rather than as holes it must stop at.
        let mut applied: HashSet<String> = already;
        let mut touched: HashSet<Uuid> = HashSet::new();

        // ── 4. Fetch bodies in batches, oldest first ─────────────────────────
        for chunk in to_fetch.chunks(MAX_TX_PER_BATCH) {
            let calls: Vec<(&'static str, Value)> = chunk
                .iter()
                .map(|s| {
                    (
                        "getTransaction",
                        json!([
							s.signature,
							{
								"encoding": "jsonParsed",
								"commitment": DETECT_COMMITMENT,
								"maxSupportedTransactionVersion": 0
							}
						]),
                    )
                })
                .collect();

            let results = self.rpc_batch(&calls).await?;

            for (sig_ref, result) in chunk.iter().zip(results) {
                let raw = match result {
                    // Listed by the index but not servable at this commitment on
                    // the node we hit. The cursor won't pass it; next tick.
                    Ok(Value::Null) => continue,
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "[{}] getTransaction {} failed: {e}",
                            self.network_name, sig_ref.signature
                        );
                        continue;
                    }
                };

                let tx = match parse_tx_view(&sig_ref.signature, &raw) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!(
                            "[{}] could not parse tx {}: {e}",
                            self.network_name, sig_ref.signature
                        );
                        continue;
                    }
                };

                if tx.failed {
                    applied.insert(tx.signature.clone());
                    continue;
                }

                let hit = self.apply_transaction(pool, &tx, &index).await?;
                touched.extend(hit);
                applied.insert(tx.signature.clone());
            }
        }

        // ── 5. Recompute each affected invoice exactly once ──────────────────
        for invoice_id in &touched {
            self.recompute_invoice_totals(pool, *invoice_id, &index).await?;
        }

        // ── 6. Advance cursors, capped at the finalized watermark ────────────
        //
        // The cursor is a resume point, not a high-water mark. Parking it at the
        // newest finalized signature means a restart never resumes from a slot
        // that can still fork away, and the unfinalized window is re-listed each
        // tick so a re-landed transaction is picked up with no reorg machinery.
        for (addr, scan) in &per_address {
            if !scan.complete {
                // Paging stopped on the page cap, so there are older signatures
                // on this feed we never listed. Moving the cursor forward would
                // make them unreachable. Leave it and shout.
                eprintln!(
                    "[{}] signature scan for {addr} hit the {MAX_SIG_PAGES_PER_ADDRESS}-page \
					 cap without reaching its floor. Cursor NOT advanced; older signatures on \
					 this feed are not being read. Raise MAX_SIG_PAGES_PER_ADDRESS or shard.",
                    self.network_name
                );
                continue;
            }

            let mut best: Option<&SigRef> = None;
            for s in &scan.sigs {
                if s.slot > finalized_slot {
                    break;
                }
                // Never step over a hole.
                if !s.failed && !applied.contains(&s.signature) {
                    break;
                }
                best = Some(s);
            }
            if let Some(s) = best {
                self.save_address_cursor(pool, addr, &s.signature, s.slot).await?;
            }
        }

        // ── 7. Promote / orphan everything in flight ─────────────────────────
        self.reconcile_statuses(pool, finalized_slot, &index).await?;

        // ── 8. Housekeeping ──────────────────────────────────────────────────
        self.prune_address_cursors(pool, &watched).await?;

        Ok(())
    }

    // ── Discovery ────────────────────────────────────────────────────────────

    /// Everything new on this address's feed, oldest first.
    ///
    /// `getSignaturesForAddress` is newest-first and stops at `until`. We page
    /// backwards until the cursor, then reverse. Two stop conditions, because
    /// `until` alone isn't safe: if the cursor signature becomes unreachable
    /// (pruned history on a non-archival node, or a fork that dropped it),
    /// `until` never matches and we'd page back toward genesis. The slot floor
    /// catches that.
    ///
    /// The floor comparison is strictly `<`. Re-listing the cursor's own slot
    /// costs nothing (the tx_hash filter in `tick` drops the ones already
    /// booked) and losing one costs a payment.
    ///
    /// A third exit — running out of pages — is reported rather than swallowed.
    /// It used to look identical to a clean finish, and the caller would then
    /// advance the cursor past signatures that had never been listed.
    async fn discover_signatures(
        &self,
        pool: &PgPool,
        address: &str,
    ) -> Result<SigScan, String> {
        let cursor = self.load_address_cursor(pool, address).await?;

        let floor_slot = match &cursor {
            Some((_, slot)) => *slot,
            // Cold start: an invoice can't be paid before it existed, so
            // anything older belongs to somebody else's history.
            None => self.cold_start_floor(pool, address).await?,
        };
        let until_sig = cursor.as_ref().map(|(s, _)| s.clone());

        let mut out: Vec<SigRef> = Vec::new();
        let mut before: Option<String> = None;
        let mut complete = false;

        for _page in 0..MAX_SIG_PAGES_PER_ADDRESS {
            let mut opts = Map::new();
            opts.insert("limit".into(), json!(SIG_PAGE_LIMIT));
            opts.insert("commitment".into(), json!(DETECT_COMMITMENT));
            if let Some(u) = &until_sig {
                opts.insert("until".into(), json!(u));
            }
            if let Some(b) = &before {
                opts.insert("before".into(), json!(b));
            }

            let page = self
                .rpc("getSignaturesForAddress", json!([address, Value::Object(opts)]))
                .await?;

            let entries = page
                .as_array()
                .ok_or_else(|| "getSignaturesForAddress returned non-array".to_string())?;

            if entries.is_empty() {
                complete = true;
                break;
            }

            let mut hit_floor = false;
            let mut last_sig = None;

            for e in entries {
                let signature = e
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "signature entry missing `signature`".to_string())?
                    .to_string();
                let slot = e.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);

                last_sig = Some(signature.clone());

                if slot < floor_slot {
                    hit_floor = true;
                    break;
                }

                // The resume point itself. Everything before it is applied;
                // everything after it in the same slot still has to be read.
                if until_sig.as_deref() == Some(signature.as_str()) {
                    hit_floor = true;
                    break;
                }

                out.push(SigRef {
                    signature,
                    slot,
                    failed: e.get("err").map(|v| !v.is_null()).unwrap_or(false),
                });
            }

            if hit_floor || entries.len() < SIG_PAGE_LIMIT {
                complete = true;
                break;
            }
            before = last_sig;
        }

        out.reverse(); // oldest first — money must be credited in order
        Ok(SigScan { sigs: out, complete })
    }

    async fn cold_start_floor(&self, pool: &PgPool, address: &str) -> Result<i64, String> {
        let floor = sqlx::query_scalar::<_, Option<i64>>(
            r#"
			SELECT MIN(i.created_block)
			  FROM invoices i
			 WHERE i.network_type = $1
			   AND i.chain_ref = $2
			   AND (i.wallet_address = $3 OR i.payment_reference = $3)
			"#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(address)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("cold_start_floor: {e}"))?;

        // Same margin as the per-invoice plausibility check, for the same
        // reason. No created_block recorded (older rows) => take the whole feed,
        // capped by MAX_SIG_PAGES_PER_ADDRESS.
        Ok((floor.unwrap_or(0) - CREATED_SLOT_MARGIN).max(0))
    }

    // ── Attribution ──────────────────────────────────────────────────────────

    /// Which invoices does this transaction pay, and by which path?
    ///
    /// Direct (naive) path first: a positive delta on the invoice's own deposit
    /// address in the invoice's own asset is unambiguous. Nobody else is paid at
    /// that address, and a native transfer can never satisfy a token invoice
    /// because we read a different balance table entirely. One extra condition:
    /// the deposit address must not have signed. Closing an ATA refunds its rent
    /// to the owner, which shows up as a positive lamport delta on the HD key in
    /// a transaction that key authorized — a false credit on native invoices.
    /// An ATA can never sign, so the guard is free on the SPL side.
    ///
    /// Reference (smart) path second, and only under these conditions:
    ///
    ///   1. The reference key did NOT sign. A Solana Pay reference is a
    ///      read-only non-signer by construction. This is the guard that stops
    ///      the merchant's own sweep — HD address -> merchant wallet — from
    ///      being re-credited as a payment. It also stops a third party from
    ///      steering attribution by naming our HD key in a transaction of their
    ///      own.
    ///   2. The reference account's own lamports didn't go down. Belt and
    ///      braces for the same class of outbound flow.
    ///   3. The invoice wasn't already credited on the direct path in this same
    ///      transaction.
    ///   4. No other invoice is claiming the *same* `(merchant_target, mint)`
    ///      credit. Two claimants on one credit can't be split, so neither is
    ///      taken and it goes to manual review — but two references pointing at
    ///      different merchants, or at different mints on the same merchant, are
    ///      separate credits and both settle normally. The old check refused the
    ///      whole transaction whenever it carried more than one reference, which
    ///      also caught every batch payment across two of one merchant's native
    ///      invoices, because there the reference and the deposit address are the
    ///      same key.
    fn classify<'a>(
        &self,
        tx: &TxView,
        index: &WatchIndex<'a>,
    ) -> Vec<(&'a WatchedInvoice, Decimal, &'static str)> {
        let mut out: Vec<(&WatchedInvoice, Decimal, &'static str)> = Vec::new();
        let mut credited: HashSet<Uuid> = HashSet::new();

        // ── direct ──
        for key in &tx.account_keys {
            let Some(invs) = index.by_deposit.get(key.as_str()) else { continue };

            for inv in invs {
                if credited.contains(&inv.invoice_id) {
                    continue;
                }
                if tx.signers.contains(key) {
                    continue;
                }
                if !inv.slot_is_plausible(tx.slot) {
                    continue;
                }

                let credit = tx.credit(&inv.deposit_address, inv.mint.as_deref());
                if credit <= 0 {
                    continue;
                }

                match i128_to_decimal(credit) {
                    Ok(d) => {
                        credited.insert(inv.invoice_id);
                        out.push((*inv, d, "direct"));
                    }
                    Err(e) => eprintln!(
                        "[{}] amount overflow on {}: {e}",
                        self.network_name, tx.signature
                    ),
                }
            }
        }

        // ── reference ──
        // Grouped by the credit each claimant is pointing at, so ambiguity is
        // scoped to the credit that's actually contested.
        let mut claims: HashMap<(&str, &str), Vec<&WatchedInvoice>> = HashMap::new();

        for key in &tx.account_keys {
            let Some(invs) = index.by_reference.get(key.as_str()) else { continue };

            for inv in invs {
                if credited.contains(&inv.invoice_id) {
                    // Already booked on the direct path. Expected whenever the
                    // payer took the naive route on a native invoice, because
                    // there the reference key and the deposit address are one
                    // account. Only worth a word if money ALSO moved into the
                    // merchant wallet in the same transaction.
                    let split = tx.credit(&inv.merchant_target, inv.mint.as_deref());
                    if split > 0 {
                        eprintln!(
                            "[{}] tx {} paid invoice {} on the direct path AND moved {} into \
							 the merchant wallet {}. Only the direct leg is credited — a \
							 single transaction that splits across both paths needs manual \
							 review.",
                            self.network_name,
                            tx.signature,
                            inv.invoice_id,
                            split,
                            inv.merchant_target
                        );
                    }
                    continue;
                }

                if tx.signers.contains(key) {
                    continue;
                }
                if tx.native_credit(key) < 0 {
                    continue;
                }
                if !inv.slot_is_plausible(tx.slot) {
                    continue;
                }

                let mint_key = inv.mint.as_deref().unwrap_or("");
                claims
                    .entry((inv.merchant_target.as_str(), mint_key))
                    .or_default()
                    .push(*inv);
            }
        }

        for ((target, mint_key), claimants) in claims {
            if claimants.len() > 1 {
                eprintln!(
                    "[{}] tx {} carries {} of our references all pointing at the same credit \
					 ({} / {}): invoices {}. Refusing to attribute a credit that can't be \
					 split. Manual review required.",
                    self.network_name,
                    tx.signature,
                    claimants.len(),
                    target,
                    if mint_key.is_empty() { "SOL" } else { mint_key },
                    claimants.iter().map(|i| i.invoice_id.to_string()).collect::<Vec<_>>().join(", ")
                );
                continue;
            }

            let inv = claimants[0];
            let merchant_credit = tx.credit(target, inv.mint.as_deref());

            if merchant_credit > 0 {
                match i128_to_decimal(merchant_credit) {
                    Ok(d) => out.push((inv, d, "reference")),
                    Err(e) => eprintln!(
                        "[{}] amount overflow on {}: {e}",
                        self.network_name, tx.signature
                    ),
                }
            } else {
                // Reference present but the merchant's balance in the expected
                // asset didn't move: wrong mint, wrong destination, or memo
                // only. Never credit on the reference alone.
                println!(
                    "[{}] tx {} carries reference for invoice {} but no {} credit to {} — ignored",
                    self.network_name,
                    tx.signature,
                    inv.invoice_id,
                    inv.mint.as_deref().unwrap_or("SOL"),
                    target
                );
            }
        }

        out
    }

    /// Record every credit this transaction produced, and return the invoices it
    /// touched so the caller can recompute their totals once.
    ///
    /// One database transaction for the whole signature, not one per credit.
    /// That matters because `tick` skips re-fetching signatures that are already
    /// on record: a half-committed transaction would leave one invoice credited
    /// and the other permanently skipped.
    ///
    /// Idempotent on (invoice_id, tx_hash): the every-tick rescan of the
    /// unfinalized window hits ON CONFLICT and does nothing. The only UPDATE
    /// that can fire is the un-orphan case, for a transaction that was dropped
    /// and then re-landed.
    async fn apply_transaction(
        &self,
        pool: &PgPool,
        tx: &TxView,
        index: &WatchIndex<'_>,
    ) -> Result<Vec<Uuid>, String> {
        let credits = self.classify(tx, index);
        if credits.is_empty() {
            return Ok(Vec::new());
        }

        let mut db_tx = pool
            .begin()
            .await
            .map_err(|e| format!("apply_transaction begin tx: {e}"))?;

        let mut touched = Vec::with_capacity(credits.len());

        for (inv, amount, path) in credits {
            let inserted = sqlx::query(
                r#"
				INSERT INTO payments
					(invoice_id, tx_hash, amount, block_number, block_hash,
					 confirmations, status, payment_path)
				VALUES ($1, $2, $3, $4, '', $5, 'detected', $6)
				ON CONFLICT (invoice_id, tx_hash) DO NOTHING
				"#,
            )
                .bind(inv.invoice_id)
                .bind(&tx.signature)
                .bind(amount)
                .bind(tx.slot)
                .bind(CONF_DETECTED)
                .bind(path)
                .execute(&mut *db_tx)
                .await
                .map_err(|e| format!("insert payment: {e}"))?
                .rows_affected()
                == 1;

            if inserted {
                println!(
                    "[{}] detected {} {} via {} path -> invoice {} (merchant {}, sig {}, slot {})",
                    self.network_name,
                    amount,
                    inv.mint.as_deref().unwrap_or("lamports"),
                    path,
                    inv.invoice_id,
                    inv.merchant_id,
                    tx.signature,
                    tx.slot
                );

                let mut fields = Map::new();
                fields.insert("Signature".into(), json!(tx.signature));
                fields.insert("AmountBaseUnits".into(), json!(amount.to_string()));
                fields.insert("Slot".into(), json!(tx.slot));
                fields.insert("Mint".into(), json!(inv.mint));
                fields.insert("PaymentPath".into(), json!(path));
                fields.insert("Confirmations".into(), json!(CONF_DETECTED));
                fields.insert("ConfirmationLevel".into(), json!("detected"));

                // webhook_events is UNIQUE (merchant_id, dedupe_key). The bare
                // signature collides when one transaction pays two invoices
                // belonging to the same merchant — the second event was being
                // swallowed. Scope every key to its event type and its subject.
                let dedupe_key = format!("payment.detected:{}:{}", inv.invoice_id, tx.signature);

                enqueue_webhook(
                    &mut db_tx,
                    inv.invoice_id,
                    "payment.detected",
                    &dedupe_key,
                    fields,
                )
                    .await?;
            } else {
                // Seen before. The only thing worth writing is a resurrection: a
                // transaction we orphaned that has since re-landed. The amount is
                // never rewritten — the same signature always moved the same
                // money, and letting it change would let a replay inflate a total.
                sqlx::query(
                    r#"
					UPDATE payments
					   SET block_number = $2,
						   status = 'detected',
						   confirmations = $3,
						   payment_path = COALESCE(payment_path, $5),
						   updated_at = now()
					 WHERE invoice_id = $1
					   AND tx_hash = $4
					   AND status = 'orphaned'
					"#,
                )
                    .bind(inv.invoice_id)
                    .bind(tx.slot)
                    .bind(CONF_DETECTED)
                    .bind(&tx.signature)
                    .bind(path)
                    .execute(&mut *db_tx)
                    .await
                    .map_err(|e| format!("relocate payment: {e}"))?;
            }

            touched.push(inv.invoice_id);
        }

        db_tx
            .commit()
            .await
            .map_err(|e| format!("apply_transaction commit: {e}"))?;

        Ok(touched)
    }

    // ── Confirmation / finality ──────────────────────────────────────────────

    /// The Solana analogue of `refresh_confirmations` + `handle_reorg`, folded
    /// into one pass, because here they're the same question: what does the
    /// cluster currently think of this signature?
    ///
    ///   status == null && slot <= finalized  -> dropped for good  -> orphaned
    ///   status.err != null                   -> landed but failed -> orphaned
    ///   confirmationStatus                   -> the level it reached
    ///
    /// A re-landed transaction keeps its signature and is simply found again;
    /// there's no EVM-style "re-mined into another block" case, because the
    /// signature is the identity, not the (block, index) pair.
    ///
    /// Only orphans trigger a recompute. Promotion moves a payment between
    /// 'detected', 'merchant_confirmed' and 'system_confirmed', all of which
    /// count toward `amount_received` identically, so recomputing after one was
    /// a full re-aggregation per payment per tick that could never change a
    /// number.
    async fn reconcile_statuses(
        &self,
        pool: &PgPool,
        finalized_slot: i64,
        index: &WatchIndex<'_>,
    ) -> Result<(), String> {
        let ids: Vec<Uuid> = index.by_id.keys().copied().collect();
        if ids.is_empty() {
            return Ok(());
        }

        let thresholds: HashMap<Uuid, ConfirmLevel> =
            index.by_id.iter().map(|(id, w)| (*id, w.level)).collect();

        // `system_confirmed` is finalized and irreversible — never looked at
        // again. That's what keeps this bounded.
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, i64, String)>(
            r#"
			SELECT p.id, p.invoice_id, p.tx_hash, p.block_number, p.status
			  FROM payments p
			 WHERE p.invoice_id = ANY($1)
			   AND p.status IN ('detected', 'merchant_confirmed')
			 ORDER BY p.block_number ASC
			"#,
        )
            .bind(&ids)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("reconcile_statuses select: {e}"))?;

        if rows.is_empty() {
            return Ok(());
        }

        // `searchTransactionHistory` makes the node fall back to the ledger/
        // bigtable, which several providers bill separately and rate-limit
        // hard. Recent signatures are in the status cache, so split the batches
        // by age instead of paying for the expensive lookup every 2 seconds.
        let (recent, old): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|r| r.3 > finalized_slot - RECENT_STATUS_WINDOW_SLOTS);

        let mut orphaned_invoices: HashSet<Uuid> = HashSet::new();

        for (group, search_history) in [(recent, false), (old, true)] {
            for chunk in group.chunks(MAX_STATUS_PER_BATCH) {
                let sigs: Vec<&str> = chunk.iter().map(|r| r.2.as_str()).collect();

                let res = self
                    .rpc(
                        "getSignatureStatuses",
                        json!([sigs, { "searchTransactionHistory": search_history }]),
                    )
                    .await?;

                let statuses = res
                    .get("value")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| "getSignatureStatuses returned no value array".to_string())?;

                for ((payment_id, invoice_id, signature, slot, status), st) in
                    chunk.iter().zip(statuses)
                {
                    let Some(level_target) = thresholds.get(invoice_id).copied() else { continue };

                    // ── dropped ──
                    if st.is_null() {
                        // Below the finalized root and the cluster has never
                        // heard of it: gone, can't come back. Above the root it
                        // may just be propagating — leave it. When we only asked
                        // the status cache, a null is not evidence of anything,
                        // so only trust it on the history-searching pass.
                        if search_history && *slot <= finalized_slot {
                            let did = self
                                .orphan_payment(
                                    pool,
                                    *payment_id,
                                    *invoice_id,
                                    signature,
                                    *slot,
                                    status,
                                    "dropped before finalization",
                                )
                                .await?;
                            if did {
                                orphaned_invoices.insert(*invoice_id);
                            }
                        }
                        continue;
                    }

                    // ── landed but the transaction itself failed ──
                    if st.get("err").map(|v| !v.is_null()).unwrap_or(false) {
                        let did = self
                            .orphan_payment(
                                pool,
                                *payment_id,
                                *invoice_id,
                                signature,
                                *slot,
                                status,
                                "transaction failed on-chain",
                            )
                            .await?;
                        if did {
                            orphaned_invoices.insert(*invoice_id);
                        }
                        continue;
                    }

                    let Some(reached) = st
                        .get("confirmationStatus")
                        .and_then(|v| v.as_str())
                        .and_then(ConfirmLevel::from_rpc)
                    else {
                        continue;
                    };

                    self.promote_payment(
                        pool,
                        *payment_id,
                        *invoice_id,
                        signature,
                        *slot,
                        status,
                        reached,
                        level_target,
                    )
                        .await?;
                }
            }
        }

        for invoice_id in orphaned_invoices {
            self.recompute_invoice_totals(pool, invoice_id, index).await?;
        }

        Ok(())
    }

    /// Write the level back and fire the threshold webhooks. Every state change
    /// is a guarded UPDATE, so two workers racing can't both emit.
    #[allow(clippy::too_many_arguments)]
    async fn promote_payment(
        &self,
        pool: &PgPool,
        payment_id: Uuid,
        invoice_id: Uuid,
        signature: &str,
        slot: i64,
        current_status: &str,
        reached: ConfirmLevel,
        required: ConfirmLevel,
    ) -> Result<(), String> {
        let conf = reached.as_confirmations();

        sqlx::query(
            r#"
			UPDATE payments
			   SET confirmations = $2, updated_at = now()
			 WHERE id = $1 AND confirmations <> $2 AND status <> 'orphaned'
			"#,
        )
            .bind(payment_id)
            .bind(conf)
            .execute(pool)
            .await
            .map_err(|e| format!("update confirmations: {e}"))?;

        // ── merchant threshold ───────────────────────────────────────────────
        if current_status == "detected" && reached >= required {
            let promoted = sqlx::query(
                r#"
				UPDATE payments
				   SET status = 'merchant_confirmed', updated_at = now()
				 WHERE id = $1 AND status = 'detected'
				"#,
            )
                .bind(payment_id)
                .execute(pool)
                .await
                .map_err(|e| format!("promote merchant_confirmed: {e}"))?
                .rows_affected()
                == 1;

            if promoted {
                println!(
                    "[{}] payment {} reached {} (merchant requires {}) -> merchant_confirmed",
                    self.network_name,
                    payment_id,
                    reached.label(),
                    required.label()
                );

                let mut tx = pool
                    .begin()
                    .await
                    .map_err(|e| format!("promote begin tx (confirmed): {e}"))?;

                let mut fields = Map::new();
                fields.insert("PaymentId".into(), json!(payment_id));
                fields.insert("Signature".into(), json!(signature));
                fields.insert("Slot".into(), json!(slot));
                fields.insert("Confirmations".into(), json!(conf));
                fields.insert("ConfirmationLevel".into(), json!(reached.label()));
                fields.insert("RequiredConfirmations".into(), json!(required.as_confirmations()));
                fields.insert("RequiredLevel".into(), json!(required.label()));

                // Was the bare payment_id, which collides with the
                // payment.finalized key below on the same merchant — that event
                // was being dropped as a duplicate for every payment.
                let dedupe_key = format!("payment.confirmed:{payment_id}");

                enqueue_webhook(&mut tx, invoice_id, "payment.confirmed", &dedupe_key, fields)
                    .await?;

                tx.commit()
                    .await
                    .map_err(|e| format!("promote commit tx (confirmed): {e}"))?;
            }
        }

        // ── finality ─────────────────────────────────────────────────────────
        // Terminal. Once the cluster roots a slot it can't be undone, so this is
        // also what drops the payment out of `reconcile_statuses` forever. The
        // `status <> 'orphaned'` guard matters: without it a row we orphaned
        // earlier in this same pass could be resurrected by a stale status read.
        if reached == ConfirmLevel::Finalized {
            let finalized = sqlx::query(
                r#"
				UPDATE payments
				   SET status = 'system_confirmed',
					   confirmations = $2,
					   updated_at = now()
				 WHERE id = $1
				   AND status NOT IN ('system_confirmed', 'orphaned')
				"#,
            )
                .bind(payment_id)
                .bind(CONF_FINALIZED)
                .execute(pool)
                .await
                .map_err(|e| format!("promote system_confirmed: {e}"))?
                .rows_affected()
                == 1;

            if finalized {
                println!(
                    "[{}] payment {} finalized at slot {}, no longer polled",
                    self.network_name, payment_id, slot
                );

                let mut tx = pool
                    .begin()
                    .await
                    .map_err(|e| format!("promote begin tx (finalized): {e}"))?;

                let mut fields = Map::new();
                fields.insert("PaymentId".into(), json!(payment_id));
                fields.insert("Signature".into(), json!(signature));
                fields.insert("Slot".into(), json!(slot));
                fields.insert("Confirmations".into(), json!(CONF_FINALIZED));
                fields.insert("ConfirmationLevel".into(), json!("finalized"));

                let dedupe_key = format!("payment.finalized:{payment_id}");

                enqueue_webhook(&mut tx, invoice_id, "payment.finalized", &dedupe_key, fields)
                    .await?;

                tx.commit()
                    .await
                    .map_err(|e| format!("promote commit tx (finalized): {e}"))?;
            }
        }

        Ok(())
    }

    /// The one case that always notifies the merchant: money we told them about
    /// is gone, so anything they shipped against it has to be walked back.
    ///
    /// Returns whether this call is the one that flipped the row, so the caller
    /// only recomputes invoices whose totals actually moved.
    #[allow(clippy::too_many_arguments)]
    async fn orphan_payment(
        &self,
        pool: &PgPool,
        payment_id: Uuid,
        invoice_id: Uuid,
        signature: &str,
        slot: i64,
        previous_status: &str,
        reason: &str,
    ) -> Result<bool, String> {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| format!("orphan_payment begin tx: {e}"))?;

        let orphaned = sqlx::query(
            r#"
			UPDATE payments
			   SET status = 'orphaned', confirmations = 0, updated_at = now()
			 WHERE id = $1 AND status <> 'orphaned'
			"#,
        )
            .bind(payment_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("orphan update: {e}"))?
            .rows_affected()
            == 1;

        if !orphaned {
            tx.rollback().await.ok();
            return Ok(false);
        }

        println!(
            "[{}] payment {} orphaned ({}), sig {} at slot {}, prev status {}",
            self.network_name, payment_id, reason, signature, slot, previous_status
        );

        let mut fields = Map::new();
        fields.insert("PaymentId".into(), json!(payment_id));
        fields.insert("Signature".into(), json!(signature));
        fields.insert("Slot".into(), json!(slot));
        fields.insert("PreviousStatus".into(), json!(previous_status));
        fields.insert("Reason".into(), json!(reason));

        // Slot is in the key because a payment can be orphaned, re-land, and be
        // orphaned again; on the bare payment_id the second event would be
        // deduped away. A re-drop at the identical slot would still collapse —
        // acceptable, since it's the same fact.
        let dedupe_key = format!("payment.orphaned:{payment_id}:{slot}");

        // TODO: skip entirely once merchant webhook settings exist and this
        //       merchant has opted out of orphaned notifications.
        enqueue_webhook(&mut tx, invoice_id, "payment.orphaned", &dedupe_key, fields).await?;

        tx.commit()
            .await
            .map_err(|e| format!("orphan_payment commit tx: {e}"))?;

        Ok(true)
    }

    // ── Invoice totals ───────────────────────────────────────────────────────

    /// Rebuild invoices.amount_received / status / tx_hash from the non-orphaned
    /// payments. Always a full recompute, never a delta, so rescans, duplicate
    /// ticks and dropped transactions all converge on the same number.
    ///
    /// This is also where multi-transfer underpayment resolves itself: three
    /// partial sends to the deposit address are three payment rows summing to the
    /// requested amount, and the invoice flips 'underpaid' -> 'paid' on the third
    /// with no special casing. The reference path is expected to be a single
    /// wallet transaction, but it goes through identical arithmetic, so a partial
    /// there behaves the same instead of being a code path nobody tested.
    async fn recompute_invoice_totals(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
        index: &WatchIndex<'_>,
    ) -> Result<(), String> {
        let Some(inv) = index.by_id.get(&invoice_id).copied() else {
            return Ok(());
        };

        let (received, first_tx) = sqlx::query_as::<_, (Decimal, Option<String>)>(
            r#"
			SELECT COALESCE(SUM(amount), 0),
				   (SELECT tx_hash
					  FROM payments
					 WHERE invoice_id = $1 AND status <> 'orphaned'
					 ORDER BY block_number ASC, created_at ASC
					 LIMIT 1)
			  FROM payments
			 WHERE invoice_id = $1 AND status <> 'orphaned'
			"#,
        )
            .bind(invoice_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("sum payments: {e}"))?;

        let new_status = if received >= inv.amount_requested {
            if received > inv.amount_requested { "overpaid" } else { "paid" }
        } else if received > Decimal::ZERO {
            "underpaid"
        } else {
            "pending"
        };

        let status_change = sqlx::query_as::<_, (String, String)>(
            r#"
			WITH old AS (SELECT status AS prev FROM invoices WHERE id = $1)
			UPDATE invoices i
			   SET amount_received = $2,
				   tx_hash = $4,
				   status = CASE WHEN i.status = 'expired' THEN i.status ELSE $3 END,
				   updated_at = now()
			  FROM old
			 WHERE i.id = $1
			   AND (
					 i.amount_received <> $2
				  OR i.tx_hash IS DISTINCT FROM $4
				  OR (i.status <> $3 AND i.status <> 'expired')
				   )
			RETURNING old.prev, i.status
			"#,
        )
            .bind(invoice_id)
            .bind(received)
            .bind(new_status)
            .bind(&first_tx)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("update invoice totals: {e}"))?;

        let Some((prev, current)) = status_change else { return Ok(()) };

        let was_settled = matches!(prev.as_str(), "paid" | "overpaid");
        let is_settled = matches!(current.as_str(), "paid" | "overpaid");

        if is_settled && !was_settled {
            println!(
                "[{}] invoice {} settled: received {} / requested {} ({})",
                self.network_name, invoice_id, received, inv.amount_requested, current
            );

            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("recompute begin tx: {e}"))?;

            let mut fields = Map::new();
            fields.insert("AmountReceived".into(), json!(received.to_string()));
            fields.insert("AmountRequested".into(), json!(inv.amount_requested.to_string()));
            fields.insert("Overpaid".into(), json!(current == "overpaid"));
            fields.insert("Mint".into(), json!(inv.mint));
            fields.insert("TxHash".into(), json!(first_tx));

            let dedupe_key = format!("payment.finished:{}:{}", invoice_id, current);
            enqueue_webhook(&mut tx, invoice_id, "payment.finished", &dedupe_key, fields).await?;

            tx.commit()
                .await
                .map_err(|e| format!("recompute commit tx: {e}"))?;

            // TODO: make the trigger policy a merchant setting: 'on_detected'
            //       (current), 'on_confirmed' (every contributing payment must be
            //       merchant_confirmed first), or 'on_finalized'.
            // TODO: underpaid tolerance (dust / rounding) belongs here too;
            //       currently strict >=.
        } else if !is_settled && was_settled {
            // A dropped transaction clawed us back below the requested amount.
            // payment.orphaned already explained why, so no second event.
            println!(
                "[{}] invoice {} fell back to {} after an orphan (received {})",
                self.network_name, invoice_id, current, received
            );
        }

        Ok(())
    }

    // ── Loading / scan state ─────────────────────────────────────────────────

    /// Live invoices, plus dead invoices that still have payments counting
    /// toward a level.
    ///
    /// `merchant_target` is not a column: it's the merchant's main Solana wallet
    /// (native) or that wallet's ATA for the mint (SPL). The wallet comes from
    /// `merchant_wallets`, which stores network_type = 'solana' — the same
    /// string `invoices.network_type` uses, so one constant covers both.
    ///
    /// The ATA derivation is memoized. `find_program_address` is up to 255
    /// SHA-256 rounds; at the watch cap that was 5 000 of them every 2 seconds,
    /// nearly all recomputing the same handful of (wallet, mint) pairs.
    async fn load_watched_invoices(&self, pool: &PgPool) -> Result<Vec<WatchedInvoice>, String> {
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,           // id
                Uuid,           // merchant_id
                String,         // wallet_address (deposit target)
                Option<String>, // payment_reference
                Option<String>, // merchant main wallet
                Decimal,        // amount_requested
                Option<String>, // token_address (mint; null/"0"/"" => native)
                Option<String>, // token_program (owner program of the mint)
                Option<i16>,    // required_confirmations
                Option<i64>,    // created_block
            ),
        >(
            r#"
			SELECT i.id,
				   i.merchant_id,
				   i.wallet_address,
				   i.payment_reference,
				   mw.address,
				   i.amount_requested,
				   i.token_address,
				   i.token_program,
				   i.required_confirmations,
				   i.created_block
			  FROM invoices i
			  LEFT JOIN merchant_wallets mw
					 ON mw.merchant_id = i.merchant_id
					AND mw.network_type = $1
			 WHERE i.network_type = $1
			   AND i.chain_ref = $2
			   AND i.wallet_address IS NOT NULL
			   AND i.wallet_address <> ''
			   AND (
					 (i.status IN ('pending','underpaid') AND i.expires_at > now())
				  OR EXISTS (
					   SELECT 1 FROM payments p
						WHERE p.invoice_id = i.id
						  AND p.status IN ('detected','merchant_confirmed')
					 )
				   )
			 ORDER BY i.created_at ASC
			 LIMIT $3
			"#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(MAX_WATCHED_INVOICES)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("load_watched_invoices: {e}"))?;

        if rows.len() as i64 == MAX_WATCHED_INVOICES {
            eprintln!(
                "[{}] watch set hit the {MAX_WATCHED_INVOICES} cap — the oldest invoices are \
				 being polled and newer ones are starved. Shard the watcher or raise the cap.",
                self.network_name
            );
        }

        let mut ata_cache: HashMap<(String, String, String), String> = HashMap::new();
        let mut out = Vec::with_capacity(rows.len());

        for (
            invoice_id,
            merchant_id,
            deposit_address,
            payment_reference,
            merchant_wallet,
            amount_requested,
            mint_raw,
            token_program_raw,
            required_confirmations,
            created_slot,
        ) in rows
        {
            // Creation stores NULL for native; be defensive about "0"/"" too.
            let mint = mint_raw.filter(|m| !m.is_empty() && m != "0");
            let token_program = token_program_raw.filter(|p| !p.is_empty() && p != "0");
            let payment_reference = payment_reference.unwrap_or_default();

            // A mint with no program on the row predates the token_program column or
            // was written by a broken creation path. Skipping the invoice entirely is
            // wrong (the direct path may still be collecting), but we refuse to guess
            // the program: a legacy-SPL default would silently produce a merchant ATA
            // nobody credits for a Token-2022 mint.
            if mint.is_some() && token_program.is_none() {
                eprintln!(
                    "[{}] invoice {}: token_address is set but token_program is NULL; \
					 reference path disabled until the row is backfilled",
                    self.network_name, invoice_id
                );
            }

            let merchant_target = match (&merchant_wallet, &mint, &token_program) {
                (Some(w), None, _) => w.clone(),
                (Some(w), Some(m), Some(p)) => {
                    let key = (w.clone(), m.clone(), p.clone());
                    match ata_cache.get(&key) {
                        Some(cached) => cached.clone(),
                        None => match derive_associated_token_address(w, m, p) {
                            Ok(ata) => {
                                ata_cache.insert(key, ata.clone());
                                ata
                            }
                            Err(e) => {
                                eprintln!(
                                    "[{}] invoice {}: could not derive merchant ATA for wallet \
									 {w} / mint {m} / program {p}: {e}",
                                    self.network_name, invoice_id
                                );
                                String::new()
                            }
                        },
                    }
                }
                // Mint present, program missing — already logged above.
                (Some(_), Some(_), None) => String::new(),
                (None, _, _) => String::new(),
            };

            if merchant_wallet.is_none() {
                eprintln!(
                    "[{}] invoice {} has no resolvable merchant target (no merchant_wallets row \
					 with network_type = '{}'?); reference path disabled for it",
                    self.network_name, invoice_id, NETWORK_TYPE
                );
            }

            out.push(WatchedInvoice {
                invoice_id,
                merchant_id,
                deposit_address,
                payment_reference,
                merchant_target,
                amount_requested,
                mint,
                level: ConfirmLevel::from_required(required_confirmations.unwrap_or(1) as i64),
                created_slot,
            });
        }

        Ok(out)
    }

    async fn load_address_cursor(
        &self,
        pool: &PgPool,
        address: &str,
    ) -> Result<Option<(String, i64)>, String> {
        sqlx::query_as::<_, (String, i64)>(
            r#"
			SELECT last_signature, last_slot
			  FROM network_address_cursors
			 WHERE network_type = $1 AND chain_ref = $2 AND address = $3
			"#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(address)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load_address_cursor({address}): {e}"))
    }

    /// Monotonic by slot, so a stale worker can't walk the cursor backwards and
    /// cause a rescan storm. `<=` rather than `<` because progress *within* a
    /// slot has to be persistable — Solana packs many transactions per slot, and
    /// with a strict `<` the cursor would stick to the first signature in the
    /// slot and re-list the rest on every tick, forever.
    ///
    /// The cost of `<=` is that two workers on the same slot can swap the stored
    /// signature for an earlier one in that slot. Harmless: it re-lists a few
    /// signatures the tx_hash filter then drops.
    async fn save_address_cursor(
        &self,
        pool: &PgPool,
        address: &str,
        signature: &str,
        slot: i64,
    ) -> Result<(), String> {
        sqlx::query(
            r#"
			INSERT INTO network_address_cursors
				(network_type, chain_ref, address, last_signature, last_slot, updated_at)
			VALUES ($1, $2, $3, $4, $5, now())
			ON CONFLICT (network_type, chain_ref, address) DO UPDATE
			   SET last_signature = EXCLUDED.last_signature,
				   last_slot = EXCLUDED.last_slot,
				   updated_at = now()
			 WHERE network_address_cursors.last_slot <= EXCLUDED.last_slot
			"#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(address)
            .bind(signature)
            .bind(slot)
            .execute(pool)
            .await
            .map_err(|e| format!("save_address_cursor({address}): {e}"))?;
        Ok(())
    }

    /// Drop cursors no live invoice cares about. Without this the table grows one
    /// row per invoice forever.
    ///
    /// Bails on an empty whitelist. `address <> ALL('{}')` is TRUE for every row,
    /// so calling this during a quiet period with nothing watched would delete
    /// the entire cursor table and force a cold-start rescan of every address.
    async fn prune_address_cursors(
        &self,
        pool: &PgPool,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let mut live: Vec<String> = Vec::with_capacity(watched.len() * 2);
        for w in watched {
            for a in w.watch_addresses() {
                live.push(a.to_string());
            }
        }

        if live.is_empty() {
            return Ok(());
        }

        sqlx::query(
            r#"
			DELETE FROM network_address_cursors
			 WHERE network_type = $1
			   AND chain_ref = $2
			   AND NOT (address = ANY($3))
			   AND updated_at < now() - interval '7 days'
			"#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(&live)
            .execute(pool)
            .await
            .map_err(|e| format!("prune_address_cursors: {e}"))?;
        Ok(())
    }

    /// Wall-clock expiry. Nothing chain-derived: an invoice expires when its
    /// timer runs out, and any payment already recorded keeps counting toward
    /// finality regardless.
    ///
    /// Covers 'underpaid' as well as 'pending', so a partially-paid invoice
    /// leaves the watch set instead of being polled every 2 seconds forever.
    async fn expire_invoices(&self, pool: &PgPool) -> Result<(), String> {
        let expired = sqlx::query_as::<_, (Uuid, Decimal, Decimal, String)>(
            r#"
			UPDATE invoices
			   SET status = 'expired', updated_at = now()
			 WHERE network_type = $1
			   AND chain_ref = $2
			   AND status IN ('pending', 'underpaid')
			   AND expires_at <= now()
			RETURNING id, amount_received, amount_requested, status
			"#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .fetch_all(pool)
            .await
            .map_err(|e| format!("expire_invoices: {e}"))?;

        for (invoice_id, received, requested, _) in expired {
            println!(
                "[{}] invoice {} expired (received {} / requested {})",
                self.network_name, invoice_id, received, requested
            );

            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("expire_invoices begin tx: {e}"))?;

            let mut fields = Map::new();
            fields.insert("InvoiceId".into(), json!(invoice_id));
            fields.insert("AmountReceived".into(), json!(received.to_string()));
            fields.insert("AmountRequested".into(), json!(requested.to_string()));
            fields.insert("PartiallyPaid".into(), json!(received > Decimal::ZERO));

            let dedupe_key = format!("invoice.expired:{invoice_id}");

            enqueue_webhook(&mut tx, invoice_id, "invoice.expired", &dedupe_key, fields).await?;

            tx.commit()
                .await
                .map_err(|e| format!("expire_invoices commit tx: {e}"))?;
        }

        Ok(())
    }

    pub async fn get_finalized_block(&self) -> Result<u64, String> {
        let slot = self.get_slot(FINALIZED_COMMITMENT).await?;
        u64::try_from(slot).map_err(|_| "getSlot returned negative".to_string())
    }

    async fn ensure_merchant_wallets(&self, pool: &PgPool) -> Result<(), String> {
        let master_key_hex = std::env::var("MASTER_KEY")
            .map_err(|_| "MASTER_KEY environment variable not set".to_string())?;

        let master_key_vec = hex::decode(&master_key_hex)
            .map_err(|_| "MASTER_KEY must be a valid hex string".to_string())?;

        let master_key: &[u8; 32] = master_key_vec
            .as_slice()
            .try_into()
            .map_err(|_| "MASTER_KEY must be exactly 32 bytes".to_string())?;

        let network_type_key = NETWORK_TYPE.to_lowercase();

        // Find merchants missing a Solana wallet record
        let uninitialized_merchants = sqlx::query!(
            r#"
            SELECT m.id, km.encrypted_secret, km.encryption_nonce
            FROM merchants m
            JOIN merchant_key_material km ON m.id = km.merchant_id
            WHERE km.key_family = 'bip39'
              AND NOT EXISTS (
                  SELECT 1 FROM merchant_wallets mw
                  WHERE mw.merchant_id = m.id AND LOWER(mw.network_type) = $1
              )
            "#,
            network_type_key
        )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("Failed to query merchants missing SOL wallets: {e}"))?;

        for record in uninitialized_merchants {
            let decrypted_mnemonic_bytes = decrypt_data(master_key, &record.encrypted_secret, &record.encryption_nonce)
                .map_err(|e| format!("Failed to decrypt mnemonic for merchant {}: {e}", record.id))?;

            let mnemonic_phrase = String::from_utf8(decrypted_mnemonic_bytes)
                .map_err(|e| format!("Invalid UTF-8 mnemonic for merchant {}: {e}", record.id))?;

            // Derive Solana address at index 0
            let sol_address = derive_solana_address(&mnemonic_phrase, 0)
                .map_err(|e| format!("Failed to derive Solana wallet for merchant {}: {e}", record.id))?;

            sqlx::query!(
                r#"
                INSERT INTO merchant_wallets (merchant_id, network_type, address)
                VALUES ($1, $2, $3)
                ON CONFLICT (merchant_id, network_type) DO NOTHING
                "#,
                record.id,
                network_type_key,
                sol_address
            )
                .execute(pool)
                .await
                .map_err(|e| format!("Failed to save derived SOL wallet for merchant {}: {e}", record.id))?;

            println!("Initialized SOL wallet ({}) for merchant {}", sol_address, record.id);
        }

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Transaction parsing
// ═══════════════════════════════════════════════════════════════════════════

/// Reduce a `jsonParsed` transaction to balance deltas plus its signer set.
///
/// Deliberately does not read instructions. Pre/post balances are the only thing
/// that can't lie about how much money moved: they cover `transfer`,
/// `transferChecked`, CPIs from routers and aggregators, several transfers in one
/// transaction, and create-ATA-plus-transfer in one shot. Parsing instruction
/// shapes would mean maintaining a list of every way somebody can send a token.
/// It is also what makes Token-2022 transfer fees a non-issue for now: whatever
/// the extension skimmed, the post balance is what actually arrived.
///
/// The signer set is the one thing we *do* need beyond balances, because it's
/// what distinguishes an inbound payment that references our HD key from an
/// outbound transaction our HD key authorized.
///
/// `jsonParsed` merges address-lookup-table accounts into `message.accountKeys`
/// in balance-index order, so v0 transactions need no special handling — but the
/// key list is validated against the balance array length anyway, because a
/// provider that skips the merge would otherwise silently misattribute deltas.
fn parse_tx_view(signature: &str, raw: &Value) -> Result<TxView, String> {
    let slot = raw
        .get("slot")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "transaction missing slot".to_string())?;

    // `.get` returns Some(Value::Null) for an explicit null, which then made the
    // `err` probe below read `false` — a transaction with no meta was being
    // treated as successful until it failed on preBalances several lines later.
    let meta = raw
        .get("meta")
        .filter(|m| !m.is_null())
        .ok_or_else(|| "transaction missing meta".to_string())?;

    let failed = meta.get("err").map(|v| !v.is_null()).unwrap_or(false);

    let message = raw
        .get("transaction")
        .and_then(|t| t.get("message"))
        .ok_or_else(|| "transaction missing message".to_string())?;

    // Cheap guard against a batch response being mis-zipped: the body we parse
    // must be the body we asked for, or every delta lands on the wrong invoice.
    if let Some(returned) = raw
        .get("transaction")
        .and_then(|t| t.get("signatures"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        if returned != signature {
            return Err(format!(
                "response signature mismatch: asked for {signature}, got {returned}"
            ));
        }
    }

    let keys_raw = message
        .get("accountKeys")
        .and_then(|k| k.as_array())
        .ok_or_else(|| "transaction missing accountKeys".to_string())?;

    let mut ordered_keys: Vec<String> = Vec::with_capacity(keys_raw.len());
    let mut signers: HashSet<String> = HashSet::new();
    let mut saw_signer_flag = false;

    for k in keys_raw {
        let pk = match k {
            // jsonParsed: { pubkey, signer, writable, source }
            Value::Object(o) => {
                let pk = o
                    .get("pubkey")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "accountKey object missing pubkey".to_string())?;
                if let Some(true) = o.get("signer").and_then(|v| v.as_bool()) {
                    saw_signer_flag = true;
                    signers.insert(pk.to_string());
                }
                pk
            }
            // base58 encoding fallback — no per-key signer flag
            Value::String(s) => s.as_str(),
            _ => return Err("unexpected accountKey shape".to_string()),
        };
        ordered_keys.push(pk.to_string());
    }

    // Fallback for encodings that don't flag signers per key: by protocol the
    // first `numRequiredSignatures` keys are the signers.
    if !saw_signer_flag {
        if let Some(n) = message
            .get("header")
            .and_then(|h| h.get("numRequiredSignatures"))
            .and_then(|v| v.as_u64())
        {
            for k in ordered_keys.iter().take(n as usize) {
                signers.insert(k.clone());
            }
        }
    }

    let account_keys: HashSet<String> = ordered_keys.iter().cloned().collect();

    // ── native lamport deltas ──
    let pre = meta
        .get("preBalances")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "meta missing preBalances".to_string())?;
    let post = meta
        .get("postBalances")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "meta missing postBalances".to_string())?;

    if pre.len() != post.len() {
        return Err(format!(
            "pre/postBalances length mismatch on {signature}: {} vs {}",
            pre.len(),
            post.len()
        ));
    }
    if pre.len() > ordered_keys.len() {
        // Balances for accounts the provider didn't name. Refusing is the only
        // safe move: guessing the alignment credits the wrong address.
        return Err(format!(
            "{signature}: {} balances but only {} account keys (lookup tables not \
			 expanded by this provider)",
            pre.len(),
            ordered_keys.len()
        ));
    }

    let mut native_delta: HashMap<String, i128> = HashMap::new();
    for (i, key) in ordered_keys.iter().enumerate().take(pre.len()) {
        let before = pre[i].as_i64().unwrap_or(0) as i128;
        let after = post[i].as_i64().unwrap_or(0) as i128;
        let d = after - before;
        if d != 0 {
            *native_delta.entry(key.clone()).or_insert(0) += d;
        }
    }

    // ── SPL token deltas ──
    // Keyed on (token account address, mint) rather than owner, because a
    // transfer names the ATA, not the wallet behind it — and the ATA is what the
    // invoice stores and watches. The mint in the key is what makes a
    // wrong-token transfer into some other ATA structurally uncreditable.
    let mut token_delta: HashMap<(String, String), i128> = HashMap::new();

    let mut fold = |arr: Option<&Value>, sign: i128| -> Result<(), String> {
        let Some(entries) = arr.and_then(|v| v.as_array()) else { return Ok(()) };
        for e in entries {
            let idx = e.get("accountIndex").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as usize;
            let Some(addr) = ordered_keys.get(idx) else { continue };
            let Some(mint) = e.get("mint").and_then(|v| v.as_str()) else { continue };
            let amount_str = e
                .get("uiTokenAmount")
                .and_then(|u| u.get("amount"))
                .and_then(|v| v.as_str())
                .unwrap_or("0");
            let amount: i128 = amount_str
                .parse()
                .map_err(|_| format!("{signature}: unparseable token amount {amount_str}"))?;
            *token_delta.entry((addr.clone(), mint.to_string())).or_insert(0) += sign * amount;
        }
        Ok(())
    };

    fold(meta.get("preTokenBalances"), -1)?;
    fold(meta.get("postTokenBalances"), 1)?;

    token_delta.retain(|_, v| *v != 0);

    Ok(TxView {
        signature: signature.to_string(),
        slot,
        failed,
        account_keys,
        signers,
        native_delta,
        token_delta,
    })
}

fn i128_to_decimal(v: i128) -> Result<Decimal, String> {
    // Base units, scale 0 — decimals are a presentation-layer concern.
    Decimal::try_from_i128_with_scale(v, 0).map_err(|e| format!("amount out of Decimal range: {e}"))
}
#[async_trait]
impl NetworkClient for SolanaNetwork {
    // --- WALLET METHODS ---
    async fn get_derive_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<(String, u32, Option<String>), String> {
        // NOTE: this read-back is the only reason `create_invoice_payment` has to
        // commit token_address/token_program *before* calling here. If the trait
        // signature is ever free to change, passing them in as arguments removes
        // both the round trip and the ordering constraint.
        let invoice = sqlx::query!(
        r#"
        SELECT token_address, token_program
        FROM invoices
        WHERE id = $1
        "#,
        invoice_id
    )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to load invoice {invoice_id} for derivation: {e}"))?;

        let mint = invoice.token_address.filter(|m| !m.is_empty() && m != "0");
        let token_program = invoice.token_program.filter(|p| !p.is_empty() && p != "0");

        // Fail loudly rather than defaulting to the legacy program. A wrong default
        // produces a valid-looking address that nobody will ever pay into.
        if mint.is_some() && token_program.is_none() {
            return Err(format!(
                "invoice {invoice_id}: token_address is set but token_program is NULL; \
             refusing to guess the token program"
            ));
        }

        let row = sqlx::query!(
        r#"
        INSERT INTO merchant_network_indices (merchant_id, network, account_index, next_index)
        VALUES ($1, $2, 0, 1)
        ON CONFLICT (merchant_id, network, account_index)
        DO UPDATE SET
            next_index = merchant_network_indices.next_index + 1,
            updated_at = CURRENT_TIMESTAMP
        RETURNING next_index
        "#,
        merchant_id,
        NETWORK_TYPE
    )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to update merchant network index: {e}"))?;

        // Directly use row.next_index so the sequence starts at 1
        let index = u32::try_from(row.next_index)
            .map_err(|_| format!("Invalid wallet index: {}", row.next_index))?;

        // The HD-derived owner pubkey. This is simultaneously:
        //   - the deposit address, when the invoice is for native SOL
        //   - the ATA *owner*, when the invoice is for an SPL / Token-2022 mint
        //   - the Solana Pay `reference` in both cases
        let owner_address = derive_solana_address(mnemonic, index)?;

        let deposit_address = match (&mint, &token_program) {
            (Some(mint), Some(program)) => {
                derive_associated_token_address(&owner_address, mint, program)?
            }
            _ => owner_address.clone(),
        };

        Ok((deposit_address, index, Some(owner_address)))
    }

    fn validate_address(&self, address: &str) -> bool {
        todo!()
    }

    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        todo!()
    }

    async fn get_token_balance(&self, token_address: &str, address: &str, decimals: u8) -> Result<Amount, String> {
        todo!()
    }

    /// Current slot at detection commitment. Stored as invoices.created_block so
    /// cold-start scans never page below the invoice's own birth.
    async fn get_current_block(&self) -> Result<u64, String> {
        let slot = self.get_slot(DETECT_COMMITMENT).await?;
        u64::try_from(slot).map_err(|_| "getSlot returned negative".to_string())
    }

    async fn spin_up(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "SolanaNetwork::spin_up initializing for {} ({}/{})",
            self.network_name,
            NETWORK_TYPE,
            self.chain_ref()
        );

        // 1. Backfill missing Solana addresses for all merchants at index 0
        self.ensure_merchant_wallets(pool).await?;

        // 2. Run Solana address watcher service
        self.watch_addresses(pool).await
    }
}