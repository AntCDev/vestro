//! Esplora (Bitcoin-style UTXO) network client.
//!
//! Naive path only: one HD-derived P2WPKH address per invoice, no memo, no
//! reference, no smart path. The address *is* the correlation key.
//!
//! Two concurrent services, both started by `spin_up`:
//!
//!   tick_chain     — tracks the tip, maintains `network_seen_blocks` +
//!                    `network_scan_state` (scope `chain`), detects reorgs and
//!                    re-verifies any payment that sat above the fork point.
//!                    ~1–3 HTTP requests per tick regardless of load.
//!
//!   tick_addresses — for every watched invoice, reads that address's tx list,
//!                    writes/updates `payments`, promotes confirmations,
//!                    orphans what vanished, recomputes invoice totals.
//!                    1 HTTP request per watched invoice per tick.
//!
//! Esplora has no batch/multicall endpoint. The only real batching primitive it
//! offers is `GET /blocks/:height`, which returns ten block headers in one
//! response; the chain watcher uses it. Everything else is amortised by keeping
//! the per-address call to exactly one request that carries amount, txid,
//! confirmation state and block location together, so detection, confirmation
//! and reorg handling all fall out of the same response.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use bech32::{segwit, Hrp};
use bip32::{DerivationPath, PrivateKey, XPrv};
use bip39::Mnemonic;
use futures::StreamExt;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use ripemd::Ripemd160;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{enqueue_webhook, Amount, BitcoinNetwork, NetworkClient, PaymentWatch};

// ─────────────────────────────────────────────────────────────────────────────
// Tunables
// ─────────────────────────────────────────────────────────────────────────────

/// The one network string. `invoices.network_type`, `merchant_wallets.network_type`,
/// `network_scan_state.network_type` and `network_seen_blocks.network_type` all
/// use it. Never 'btc', never 'bitcoin' — `bitcoin*` is the `chain_ref`.
const NETWORK_TYPE: &str = "esplora";

/// Only one scanner keeps a block cursor here, so one scope.
const SCAN_SCOPE_CHAIN: &str = "chain";

const ADDRESS_POLL_SECS: u64 = 15;
const CHAIN_POLL_SECS: u64 = 20;
const HTTP_TIMEOUT_SECS: u64 = 20;

/// Requests in flight against the Esplora endpoints during one address tick.
/// Public instances rate-limit aggressively; 8 is polite, raise it for a
/// self-hosted electrs/esplora.
const ADDRESS_CONCURRENCY: usize = 8;

/// Hard ceiling on invoices watched in one tick — the real bound on requests
/// per tick, since the naive path is one address per invoice.
const MAX_WATCHED_INVOICES: i64 = 5_000;

/// Depth at which a payment is considered irreversible and stops being polled.
/// Per-invoice `required_confirmations` (merchant depth) comes from the token
/// config and is always <= this in practice.
const FINAL_CONFIRMATIONS: i64 = 6;

/// Used only when `invoices.required_confirmations` is NULL (invoice written by
/// an older build).
const DEFAULT_REQUIRED_CONFIRMATIONS: i64 = 2;

/// How far back the cursor is willing to rewind when the chain disagrees with
/// `network_seen_blocks`. Bitcoin reorgs beyond 2 blocks are historically rare;
/// 12 is generous and still bounded.
const REORG_DEPTH: u64 = 12;

/// Ceiling on catch-up per chain tick, so a long outage doesn't produce one
/// enormous tick.
const MAX_BLOCKS_PER_TICK: u64 = 200;

/// `GET /blocks/:height` returns this many headers, newest first. The batch.
const HEADER_PAGE: u64 = 10;

/// Headers older than this below the tip are pruned from `network_seen_blocks`.
const SEEN_BLOCK_RETENTION: u64 = 500;

/// Whether a payment sitting in the mempool (0 conf) counts toward
/// `invoices.amount_received`. True means the payer sees "paid" the moment the
/// transaction is broadcast, which is the behaviour a checkout page wants, at
/// the cost of a total that can move back down if the tx is RBF'd out (the
/// merchant gets `payment.orphaned` when that happens). Flip to false to only
/// credit mined transactions.
const CREDIT_UNCONFIRMED: bool = true;

/// `payments.block_number` is NOT NULL, so mempool payments are stored at 0.
/// No real payment can land in the genesis block, so 0 is a safe sentinel.
const MEMPOOL_BLOCK_SENTINEL: i64 = 0;

/// Consecutive ticks a known payment must be missing from the chain before it
/// is orphaned. Guards against one lagging or half-synced Esplora endpoint
/// briefly denying a transaction it hasn't indexed yet. In-memory, and losing
/// it on restart only delays an orphan — never causes a false one.
const ORPHAN_STRIKES: u8 = 3;

/// Keep watching an address this long past `expires_at`. Bitcoin is slow enough
/// that a payer can broadcast inside the window and be seen after it; the
/// payment is still recorded (the recompute never resurrects an expired
/// invoice — it lands as a late payment for reconciliation to deal with).
const WATCH_GRACE_MINUTES: i32 = 60;

/// `GET /address/:addr/txs` returns at most 50 entries. At exactly 50 we can't
/// tell "not there" from "fell off the page", so orphan checks are skipped for
/// that address on that tick.
const ADDRESS_TX_PAGE_SATURATED: usize = 50;

// ─────────────────────────────────────────────────────────────────────────────
// Esplora wire types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone, Default)]
pub struct EsploraTxStatus {
    #[serde(default)]
    pub confirmed: bool,
    pub block_height: Option<u64>,
    pub block_hash: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EsploraVout {
    /// Satoshis.
    pub value: u64,
    pub scriptpubkey_address: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EsploraTx {
    pub txid: String,
    pub vout: Vec<EsploraVout>,
    pub status: EsploraTxStatus,
}

#[derive(Deserialize, Debug, Clone)]
struct EsploraBlockHeader {
    id: String,
    height: u64,
    previousblockhash: Option<String>,
}

#[derive(Deserialize)]
struct EsploraAddressStats {
    funded_txo_sum: i128,
    spent_txo_sum: i128,
}

#[derive(Deserialize)]
struct EsploraAddressResponse {
    chain_stats: EsploraAddressStats,
    mempool_stats: EsploraAddressStats,
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP errors
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum ApiError {
    /// Every endpoint that answered said 404. For `/tx/:txid/status` this is the
    /// signal that a transaction is gone — it is load-bearing, not just noise.
    NotFound,
    Transport(String),
    Decode(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::NotFound => write!(f, "not found"),
            ApiError::Transport(e) => write!(f, "transport: {e}"),
            ApiError::Decode(e) => write!(f, "decode: {e}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal row/state types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Watched {
    invoice_id: Uuid,
    address: String,
    amount_requested: u128,
    required_confirmations: i64,
}

#[derive(Debug, Clone)]
struct PaymentRow {
    id: Uuid,
    tx_hash: String,
    amount: u128,
    block_number: i64,
    status: String,
}

#[derive(Debug, Clone, Copy)]
struct InvoiceTotals {
    requested: u128,
    received: u128,
    finished_now: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Network
// ─────────────────────────────────────────────────────────────────────────────

pub struct EsploraNetwork {
    network: BitcoinNetwork,
    api_urls: Vec<String>,
    client: reqwest::Client,

    /// `invoices.chain_ref`. Must equal the token config's `network_label`.
    chain_ref: &'static str,
    /// `merchant_network_indices.network`. Namespaced per chain so mainnet,
    /// testnet4 and signet never share an index counter.
    index_namespace: String,

    coin_type: u32,
    account: u32,
    hrp: &'static str,

    /// Written by the chain watcher, read by the address watcher. A hint in the
    /// sense that a stale value only delays a promotion by one tick.
    tip_height: AtomicU64,
    endpoint_cursor: AtomicUsize,

    /// txid -> consecutive ticks the chain has denied it. Hint only (see
    /// ORPHAN_STRIKES).
    orphan_strikes: Mutex<HashMap<String, u8>>,
    /// Rebuilt from the DB every tick. Never a source of truth.
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

/// The `chain_ref` for a Bitcoin network. The token config's `network_label`
/// must agree with this — ideally by calling it rather than repeating it.
pub fn chain_ref_for(network: BitcoinNetwork) -> &'static str {
    match network {
        BitcoinNetwork::Mainnet => "bitcoin",
        BitcoinNetwork::Testnet4 => "bitcoin_testnet4",
        BitcoinNetwork::Signet => "bitcoin_signet",
    }
}

impl EsploraNetwork {
    pub fn new(network: BitcoinNetwork, api_urls: Vec<String>) -> Self {
        assert!(
            !api_urls.is_empty(),
            "EsploraNetwork requires at least one API URL"
        );

        let chain_ref = chain_ref_for(network);

        // BIP84. testnet4 and signet share coin type 1, so signet is separated
        // by account index instead — otherwise the same merchant seed would
        // derive identical addresses on both, and two invoices on two networks
        // could collide on one address.
        let (coin_type, account, hrp) = match network {
            BitcoinNetwork::Mainnet => (0u32, 0u32, "bc"),
            BitcoinNetwork::Testnet4 => (1u32, 0u32, "tb"),
            BitcoinNetwork::Signet => (1u32, 1u32, "tb"),
        };

        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(HTTP_TIMEOUT_SECS))
            .user_agent("multichain-payment-processor/esplora")
            .build()
            .expect("failed to build reqwest client");

        Self {
            network,
            api_urls,
            client,
            chain_ref,
            index_namespace: format!("{NETWORK_TYPE}:{chain_ref}"),
            coin_type,
            account,
            hrp,
            tip_height: AtomicU64::new(0),
            endpoint_cursor: AtomicUsize::new(0),
            orphan_strikes: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn chain_ref(&self) -> &'static str {
        self.chain_ref
    }

    pub fn network(&self) -> BitcoinNetwork {
        self.network
    }

    // ── HTTP ────────────────────────────────────────────────────────────────
    //
    // Failover, not quorum. Esplora responses are not consensus-comparable:
    // two honest, fully-synced endpoints legitimately disagree about mempool
    // contents, tip height and `/address/:addr/txs` ordering, so byte-equality
    // across endpoints produces constant false disagreement. Correctness comes
    // from idempotent writes plus reorg re-verification, not from agreement at
    // read time.

    async fn get_text(&self, path: &str) -> Result<String, ApiError> {
        let n = self.api_urls.len();
        let start = self.endpoint_cursor.fetch_add(1, Ordering::Relaxed);
        let mut saw_not_found = false;
        let mut errors: Vec<String> = Vec::new();

        for i in 0..n {
            let base = self.api_urls[(start + i) % n].trim_end_matches('/');
            let url = format!("{base}{path}");

            match self.client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::NOT_FOUND {
                        saw_not_found = true;
                        continue;
                    }
                    if !status.is_success() {
                        errors.push(format!("{url}: HTTP {status}"));
                        continue;
                    }
                    match resp.text().await {
                        Ok(body) => return Ok(body),
                        Err(e) => errors.push(format!("{url}: body read failed: {e}")),
                    }
                }
                Err(e) => errors.push(format!("{url}: {e}")),
            }
        }

        if saw_not_found && errors.is_empty() {
            Err(ApiError::NotFound)
        } else if saw_not_found {
            // Mixed 404s and failures: treat as 404 but keep the noise for logs.
            Err(ApiError::NotFound)
        } else {
            Err(ApiError::Transport(errors.join("; ")))
        }
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let body = self.get_text(path).await?;
        serde_json::from_str(&body)
            .map_err(|e| ApiError::Decode(format!("{path}: {e} (body: {})", truncate(&body, 200))))
    }

    async fn fetch_tip_height(&self) -> Result<u64, ApiError> {
        let body = self.get_text("/blocks/tip/height").await?;
        body.trim()
            .parse::<u64>()
            .map_err(|e| ApiError::Decode(format!("tip height: {e}")))
    }

    async fn block_hash_at(&self, height: u64) -> Result<String, ApiError> {
        let body = self.get_text(&format!("/block-height/{height}")).await?;
        let hash = body.trim().to_string();
        if hash.len() != 64 {
            return Err(ApiError::Decode(format!("bad block hash at {height}: {hash}")));
        }
        Ok(hash)
    }

    /// Ten headers in one request, `start_height` down to `start_height - 9`.
    async fn fetch_header_page(&self, start_height: u64) -> Result<Vec<EsploraBlockHeader>, ApiError> {
        self.get_json(&format!("/blocks/{start_height}")).await
    }

    // ── Derivation ──────────────────────────────────────────────────────────

    fn derivation_path(&self, index: u32) -> String {
        format!(
            "m/84'/{}'/{}'/0/{}",
            self.coin_type, self.account, index
        )
    }

    /// P2WPKH (native SegWit v0) from the merchant's own seed.
    pub fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
        let mnemonic_parsed =
            Mnemonic::parse(mnemonic).map_err(|e| format!("Invalid mnemonic: {e}"))?;
        let seed = mnemonic_parsed.to_seed("");

        let path: DerivationPath = self
            .derivation_path(index)
            .parse()
            .map_err(|e| format!("Failed to parse derivation path: {e}"))?;

        let child_xprv = XPrv::derive_from_path(&seed, &path)
            .map_err(|e| format!("Failed to derive child key at path: {e}"))?;

        let public_key = child_xprv.private_key().public_key();
        let point = public_key.to_encoded_point(true);

        let sha = Sha256::digest(point.as_bytes());
        let hash160 = Ripemd160::digest(sha);

        let hrp = Hrp::parse(self.hrp).map_err(|e| format!("Invalid HRP prefix: {e}"))?;
        segwit::encode(hrp, segwit::VERSION_0, &hash160)
            .map_err(|e| format!("Bech32 encoding failed: {e}"))
    }

    // ── Chain watcher ───────────────────────────────────────────────────────

    async fn chain_loop(&self, pool: &PgPool) {
        loop {
            if let Err(e) = self.tick_chain(pool).await {
                eprintln!("[esplora:{}] chain tick failed: {e}", self.chain_ref);
            }
            tokio::time::sleep(StdDuration::from_secs(CHAIN_POLL_SECS)).await;
        }
    }

    async fn tick_chain(&self, pool: &PgPool) -> Result<(), String> {
        let tip = self.fetch_tip_height().await.map_err(|e| e.to_string())?;
        self.tip_height.store(tip, Ordering::Relaxed);

        let cursor = self.load_cursor(pool).await?;

        // Cold start: anchor at the tip. Payments are found by address, not by
        // block, so starting "now" loses nothing — the address watcher picks up
        // anything already sitting at a watched address on its first pass.
        let (mut last_block, mut last_hash) = match cursor {
            Some(c) => c,
            None => {
                let page = self.fetch_header_page(tip).await.map_err(|e| e.to_string())?;
                let header = page
                    .into_iter()
                    .find(|h| h.height == tip)
                    .ok_or_else(|| format!("tip {tip} missing from header page"))?;
                self.record_seen_block(pool, &header).await?;
                self.save_cursor(pool, header.height, &header.id).await?;
                println!(
                    "[esplora:{}] chain watcher anchored at block {tip}",
                    self.chain_ref
                );
                return Ok(());
            }
        };

        if tip < last_block {
            // An endpoint behind our cursor. Don't rewind on this alone.
            eprintln!(
                "[esplora:{}] endpoint tip {tip} is behind cursor {last_block}; skipping tick",
                self.chain_ref
            );
            return Ok(());
        }

        // Is the block we last committed still on the chain we're following?
        let hash_at_cursor = self
            .block_hash_at(last_block)
            .await
            .map_err(|e| e.to_string())?;
        if hash_at_cursor != last_hash {
            let fork = self.find_fork_point(pool, last_block).await?;
            eprintln!(
                "[esplora:{}] reorg detected at {last_block}, rewinding to {fork}",
                self.chain_ref
            );
            self.handle_reorg(pool, fork).await?;
            return Ok(());
        }

        // Walk forward, ten headers per request.
        let target = tip.min(last_block + MAX_BLOCKS_PER_TICK);
        let mut height = last_block + 1;

        while height <= target {
            let page_end = target.min(height + HEADER_PAGE - 1);
            let page = self
                .fetch_header_page(page_end)
                .await
                .map_err(|e| e.to_string())?;
            let mut by_height: HashMap<u64, EsploraBlockHeader> =
                page.into_iter().map(|h| (h.height, h)).collect();

            for h in height..=page_end {
                let Some(header) = by_height.remove(&h) else {
                    // Page didn't cover it (tip moved under us); retry next tick.
                    self.save_cursor(pool, last_block, &last_hash).await?;
                    return Ok(());
                };

                if header.previousblockhash.as_deref() != Some(last_hash.as_str()) {
                    let fork = self.find_fork_point(pool, last_block).await?;
                    eprintln!(
                        "[esplora:{}] parent mismatch at {h}, rewinding to {fork}",
                        self.chain_ref
                    );
                    self.handle_reorg(pool, fork).await?;
                    return Ok(());
                }

                self.record_seen_block(pool, &header).await?;
                last_block = header.height;
                last_hash = header.id;
            }

            // Cursor advances only after the page's headers are committed.
            self.save_cursor(pool, last_block, &last_hash).await?;
            height = page_end + 1;
        }

        self.prune_seen_blocks(pool, tip).await?;
        Ok(())
    }

    async fn load_cursor(&self, pool: &PgPool) -> Result<Option<(u64, String)>, String> {
        let row = sqlx::query!(
            r#"
            SELECT last_block, last_block_hash
            FROM network_scan_state
            WHERE network_type = $1 AND chain_ref = $2 AND scope = $3
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            SCAN_SCOPE_CHAIN
        )
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load_cursor: {e}"))?;

        Ok(row.map(|r| (r.last_block.max(0) as u64, r.last_block_hash)))
    }

    async fn save_cursor(&self, pool: &PgPool, block: u64, hash: &str) -> Result<(), String> {
        sqlx::query!(
            r#"
            INSERT INTO network_scan_state (network_type, chain_ref, scope, last_block, last_block_hash)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (network_type, chain_ref, scope)
            DO UPDATE SET last_block = EXCLUDED.last_block,
                          last_block_hash = EXCLUDED.last_block_hash,
                          updated_at = now()
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            SCAN_SCOPE_CHAIN,
            block as i64,
            hash
        )
            .execute(pool)
            .await
            .map_err(|e| format!("save_cursor: {e}"))?;
        Ok(())
    }

    async fn record_seen_block(
        &self,
        pool: &PgPool,
        header: &EsploraBlockHeader,
    ) -> Result<(), String> {
        sqlx::query!(
            r#"
            INSERT INTO network_seen_blocks
                (network_type, chain_ref, scope, block_number, block_hash, parent_hash)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (network_type, chain_ref, scope, block_number)
            DO UPDATE SET block_hash = EXCLUDED.block_hash,
                          parent_hash = EXCLUDED.parent_hash,
                          seen_at = now()
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            SCAN_SCOPE_CHAIN,
            header.height as i64,
            header.id,
            header.previousblockhash.clone().unwrap_or_default()
        )
            .execute(pool)
            .await
            .map_err(|e| format!("record_seen_block: {e}"))?;
        Ok(())
    }

    async fn prune_seen_blocks(&self, pool: &PgPool, tip: u64) -> Result<(), String> {
        let floor = tip.saturating_sub(SEEN_BLOCK_RETENTION) as i64;
        sqlx::query!(
            r#"
            DELETE FROM network_seen_blocks
            WHERE network_type = $1 AND chain_ref = $2 AND scope = $3 AND block_number < $4
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            SCAN_SCOPE_CHAIN,
            floor
        )
            .execute(pool)
            .await
            .map_err(|e| format!("prune_seen_blocks: {e}"))?;
        Ok(())
    }

    /// Highest height at or below `from` where our remembered hash still matches
    /// the chain. Bounded by REORG_DEPTH.
    async fn find_fork_point(&self, pool: &PgPool, from: u64) -> Result<u64, String> {
        let floor = from.saturating_sub(REORG_DEPTH);
        let mut h = from.saturating_sub(1);

        while h > floor {
            let remembered = sqlx::query_scalar!(
                r#"
                SELECT block_hash FROM network_seen_blocks
                WHERE network_type = $1 AND chain_ref = $2 AND scope = $3 AND block_number = $4
                "#,
                NETWORK_TYPE,
                self.chain_ref,
                SCAN_SCOPE_CHAIN,
                h as i64
            )
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("find_fork_point: {e}"))?;

            match remembered {
                Some(remembered) => {
                    let actual = self.block_hash_at(h).await.map_err(|e| e.to_string())?;
                    if actual == remembered {
                        return Ok(h);
                    }
                }
                // Nothing remembered this deep: nothing to contradict.
                None => return Ok(h),
            }
            h -= 1;
        }

        Ok(floor)
    }

    async fn handle_reorg(&self, pool: &PgPool, fork_height: u64) -> Result<(), String> {
        sqlx::query!(
            r#"
            DELETE FROM network_seen_blocks
            WHERE network_type = $1 AND chain_ref = $2 AND scope = $3 AND block_number > $4
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            SCAN_SCOPE_CHAIN,
            fork_height as i64
        )
            .execute(pool)
            .await
            .map_err(|e| format!("handle_reorg delete: {e}"))?;

        let hash = self
            .block_hash_at(fork_height)
            .await
            .map_err(|e| e.to_string())?;
        self.save_cursor(pool, fork_height, &hash).await?;

        self.reverify_payments_above(pool, fork_height as i64).await
    }

    /// Every non-orphaned payment that claims a block above the fork gets its
    /// status re-read from the chain. This deliberately includes
    /// `system_confirmed` rows: they normally stop being polled, but a reorg
    /// deeper than FINAL_CONFIRMATIONS is exactly the case where that
    /// assumption broke.
    async fn reverify_payments_above(&self, pool: &PgPool, height: i64) -> Result<(), String> {
        let rows = sqlx::query!(
            r#"
            SELECT p.id, p.tx_hash, p.amount, p.block_number, p.status,
                   p.invoice_id, i.amount_requested, i.required_confirmations, i.wallet_address
            FROM payments p
            JOIN invoices i ON i.id = p.invoice_id
            WHERE i.network_type = $1
              AND i.chain_ref = $2
              AND p.block_number > $3
              AND p.status <> 'orphaned'
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            height
        )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("reverify select: {e}"))?;

        if rows.is_empty() {
            return Ok(());
        }

        println!(
            "[esplora:{}] re-verifying {} payment(s) above block {height}",
            self.chain_ref,
            rows.len()
        );

        let tip = self.tip_height.load(Ordering::Relaxed);

        for r in rows {
            let watched = Watched {
                invoice_id: r.invoice_id,
                address: r.wallet_address,
                amount_requested: dec_to_u128(r.amount_requested),
                required_confirmations: r
                    .required_confirmations
                    .map(|v| v as i64)
                    .unwrap_or(DEFAULT_REQUIRED_CONFIRMATIONS),
            };
            let row = PaymentRow {
                id: r.id,
                tx_hash: r.tx_hash.clone(),
                amount: dec_to_u128(r.amount),
                block_number: r.block_number,
                status: r.status,
            };

            let status = match self
                .get_json::<EsploraTxStatus>(&format!("/tx/{}/status", r.tx_hash))
                .await
            {
                Ok(s) => Some(s),
                Err(ApiError::NotFound) => None,
                Err(e) => {
                    eprintln!(
                        "[esplora:{}] reverify {}: {e}",
                        self.chain_ref, r.tx_hash
                    );
                    continue;
                }
            };

            if let Err(e) = self
                .apply_chain_state(pool, &watched, &row, status.as_ref(), tip)
                .await
            {
                eprintln!("[esplora:{}] reverify apply: {e}", self.chain_ref);
            }
        }

        Ok(())
    }

    // ── Address watcher ─────────────────────────────────────────────────────

    async fn address_loop(&self, pool: &PgPool) {
        loop {
            if self.tip_height.load(Ordering::Relaxed) == 0 {
                // Wait for the chain watcher to publish a tip; confirmations are
                // meaningless without one.
                tokio::time::sleep(StdDuration::from_secs(2)).await;
                continue;
            }

            if let Err(e) = self.expire_invoices(pool).await {
                eprintln!("[esplora:{}] expiry sweep failed: {e}", self.chain_ref);
            }
            self.tick_addresses(pool).await;

            tokio::time::sleep(StdDuration::from_secs(ADDRESS_POLL_SECS)).await;
        }
    }

    async fn tick_addresses(&self, pool: &PgPool) {
        let tip = self.tip_height.load(Ordering::Relaxed);

        let watch = match self.load_watch_set(pool).await {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[esplora:{}] load_watch_set: {e}", self.chain_ref);
                return;
            }
        };

        if watch.is_empty() {
            self.pending.lock().await.clear();
            return;
        }

        {
            let mut hint = self.pending.lock().await;
            *hint = watch
                .iter()
                .map(|w| {
                    (
                        w.invoice_id,
                        PaymentWatch {
                            invoice_id: w.invoice_id,
                            address: w.address.clone(),
                            token_address: None,
                            decimals: 8,
                            target_amount: w.amount_requested,
                            required_confirmations: w.required_confirmations as u32,
                            from_block: None,
                        },
                    )
                })
                .collect();
        }

        futures::stream::iter(watch.iter())
            .for_each_concurrent(ADDRESS_CONCURRENCY, |w| async move {
                if let Err(e) = self.process_invoice(pool, w, tip).await {
                    eprintln!(
                        "[esplora:{}] invoice {} ({}): {e}",
                        self.chain_ref, w.invoice_id, w.address
                    );
                }
            })
            .await;
    }

    /// Invoices worth one request this tick: live ones inside their window (plus
    /// grace), and any invoice that still has a payment short of
    /// `system_confirmed` — those need watching to completion regardless of
    /// whether the invoice itself is finished or expired.
    async fn load_watch_set(&self, pool: &PgPool) -> Result<Vec<Watched>, String> {
        let rows = sqlx::query!(
            r#"
            SELECT i.id, i.wallet_address, i.amount_requested, i.required_confirmations
            FROM invoices i
            WHERE i.network_type = $1
              AND i.chain_ref = $2
              AND i.wallet_address <> ''
              AND (
                    (
                      i.status IN ('pending', 'underpaid', 'paid', 'overpaid')
                      AND i.expires_at > now() - ($3::int * interval '1 minute')
                    )
                    OR EXISTS (
                      SELECT 1 FROM payments p
                      WHERE p.invoice_id = i.id
                        AND p.status IN ('detected', 'merchant_confirmed')
                    )
              )
            ORDER BY i.created_at DESC
            LIMIT $4
            "#,
            NETWORK_TYPE,
            self.chain_ref,
            WATCH_GRACE_MINUTES,
            MAX_WATCHED_INVOICES
        )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("watch set: {e}"))?;

        Ok(rows
            .into_iter()
            .map(|r| Watched {
                invoice_id: r.id,
                address: r.wallet_address,
                amount_requested: dec_to_u128(r.amount_requested),
                required_confirmations: r
                    .required_confirmations
                    .map(|v| v as i64)
                    .unwrap_or(DEFAULT_REQUIRED_CONFIRMATIONS),
            })
            .collect())
    }

    /// One request per invoice. The response carries amount, txid, confirmation
    /// state and block location, so detection, promotion and orphaning all come
    /// out of it.
    async fn process_invoice(&self, pool: &PgPool, w: &Watched, tip: u64) -> Result<(), String> {
        let txs: Vec<EsploraTx> = match self
            .get_json(&format!("/address/{}/txs", w.address))
            .await
        {
            Ok(v) => v,
            Err(ApiError::NotFound) => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };

        let saturated = txs.len() >= ADDRESS_TX_PAGE_SATURATED;
        if saturated {
            eprintln!(
                "[esplora:{}] address {} returned a saturated page; skipping orphan checks",
                self.chain_ref, w.address
            );
        }

        let existing = self.load_payments(pool, w.invoice_id).await?;
        let mut seen: HashSet<String> = HashSet::new();

        for tx in &txs {
            // Credit is outputs paying *us*. A sweep spending from this address
            // shows up here too and contributes zero, which is what we want.
            let credited: u128 = tx
                .vout
                .iter()
                .filter(|o| o.scriptpubkey_address.as_deref() == Some(w.address.as_str()))
                .map(|o| o.value as u128)
                .sum();

            if credited == 0 {
                continue;
            }
            seen.insert(tx.txid.clone());

            match existing.get(&tx.txid) {
                None => self.insert_payment(pool, w, tx, credited, tip).await?,
                Some(row) => {
                    self.clear_strike(&tx.txid).await;
                    self.apply_chain_state(pool, w, row, Some(&tx.status), tip)
                        .await?;
                }
            }
        }

        if saturated {
            return Ok(());
        }

        // Anything we hold that the address no longer lists: confirm it really
        // is gone before touching it. RBF replacement and mempool eviction both
        // land here.
        for (txid, row) in existing.iter() {
            if seen.contains(txid) || row.status == "orphaned" {
                continue;
            }

            match self
                .get_json::<EsploraTxStatus>(&format!("/tx/{txid}/status"))
                .await
            {
                Ok(status) => {
                    self.clear_strike(txid).await;
                    self.apply_chain_state(pool, w, row, Some(&status), tip)
                        .await?;
                }
                Err(ApiError::NotFound) => {
                    if self.strike(txid).await >= ORPHAN_STRIKES {
                        self.apply_chain_state(pool, w, row, None, tip).await?;
                        self.clear_strike(txid).await;
                    }
                }
                Err(e) => {
                    eprintln!("[esplora:{}] status {txid}: {e}", self.chain_ref);
                }
            }
        }

        Ok(())
    }

    async fn load_payments(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
    ) -> Result<HashMap<String, PaymentRow>, String> {
        let rows = sqlx::query!(
            r#"
            SELECT id, tx_hash, amount, block_number, status
            FROM payments
            WHERE invoice_id = $1
            "#,
            invoice_id
        )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("load_payments: {e}"))?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.tx_hash.clone(),
                    PaymentRow {
                        id: r.id,
                        tx_hash: r.tx_hash,
                        amount: dec_to_u128(r.amount),
                        block_number: r.block_number,
                        status: r.status,
                    },
                )
            })
            .collect())
    }

    // ── Writes ──────────────────────────────────────────────────────────────

    async fn insert_payment(
        &self,
        pool: &PgPool,
        w: &Watched,
        tx: &EsploraTx,
        credited: u128,
        tip: u64,
    ) -> Result<(), String> {
        let height = tx.status.block_height.unwrap_or(0);
        let confirmed = tx.status.confirmed && height > 0;
        let confirmations = if confirmed { confirmations_for(height, tip) } else { 0 };

        let mut db = pool.begin().await.map_err(|e| e.to_string())?;

        let inserted = sqlx::query!(
            r#"
            INSERT INTO payments
                (invoice_id, tx_hash, amount, block_number, block_hash, confirmations, status, payment_path)
            VALUES ($1, $2, $3, $4, $5, $6, 'detected', 'direct')
            ON CONFLICT (invoice_id, tx_hash) DO NOTHING
            RETURNING id
            "#,
            w.invoice_id,
            tx.txid,
            u128_to_dec(credited),
            if confirmed { height as i64 } else { MEMPOOL_BLOCK_SENTINEL },
            tx.status.block_hash.clone(),
            confirmations as i32
        )
            .fetch_optional(&mut *db)
            .await
            .map_err(|e| format!("insert payment: {e}"))?;

        let Some(inserted) = inserted else {
            // Another tick beat us to it. Nothing to do, nothing to fire.
            db.rollback().await.ok();
            return Ok(());
        };

        let totals = settle_invoice(&mut db, w.invoice_id).await?;

        let mut fields = payment_fields(w, &tx.txid, credited, confirmations, confirmed.then_some(height));
        fields.insert("amount_received".into(), Value::String(totals.received.to_string()));
        fields.insert("amount_expected".into(), Value::String(totals.requested.to_string()));
        enqueue_webhook(&mut db, w.invoice_id, "payment.detected", &tx.txid, fields).await;

        // A transaction can be first seen already deep — promote in the same
        // transaction rather than waiting a tick.
        if confirmed {
            promote(
                &mut db,
                w,
                inserted.id,
                &tx.txid,
                credited,
                confirmations,
                height,
            )
                .await?;
        }

        maybe_finish(&mut db, w, &totals).await;

        db.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Reconcile one stored payment against what the chain currently says.
    /// `status = None` means the chain denies the transaction entirely.
    async fn apply_chain_state(
        &self,
        pool: &PgPool,
        w: &Watched,
        row: &PaymentRow,
        status: Option<&EsploraTxStatus>,
        tip: u64,
    ) -> Result<(), String> {
        match status {
            Some(st) if st.confirmed && st.block_height.is_some() => {
                let height = st.block_height.unwrap();
                let confirmations = confirmations_for(height, tip);

                // Nothing left to do for a final payment still sitting where we
                // left it.
                if row.status == "system_confirmed" && row.block_number == height as i64 {
                    return Ok(());
                }

                let mut db = pool.begin().await.map_err(|e| e.to_string())?;

                // Re-mined after a reorg: back into the flow at `detected`.
                let resurrected = sqlx::query!(
                    r#"
                    UPDATE payments SET status = 'detected', updated_at = CURRENT_TIMESTAMP
                    WHERE id = $1 AND status = 'orphaned'
                    RETURNING id
                    "#,
                    row.id
                )
                    .fetch_optional(&mut *db)
                    .await
                    .map_err(|e| format!("resurrect: {e}"))?
                    .is_some();

                // Location and depth only. The amount is never rewritten.
                sqlx::query!(
                    r#"
                    UPDATE payments
                    SET block_number = $2, block_hash = $3, confirmations = $4,
                        updated_at = CURRENT_TIMESTAMP
                    WHERE id = $1 AND status <> 'orphaned'
                    "#,
                    row.id,
                    height as i64,
                    st.block_hash.clone(),
                    confirmations as i32
                )
                    .execute(&mut *db)
                    .await
                    .map_err(|e| format!("update location: {e}"))?;

                let promoted = promote(
                    &mut db,
                    w,
                    row.id,
                    &row.tx_hash,
                    row.amount,
                    confirmations,
                    height,
                )
                    .await?;

                if resurrected || promoted {
                    let totals = settle_invoice(&mut db, w.invoice_id).await?;
                    maybe_finish(&mut db, w, &totals).await;
                }

                db.commit().await.map_err(|e| e.to_string())?;
                Ok(())
            }

            // Known to the chain but unmined. Fine if it was always a mempool
            // payment; a demotion from a block means it was reorged out.
            Some(_) => {
                if row.block_number > MEMPOOL_BLOCK_SENTINEL {
                    self.orphan_payment(pool, w, row, "reorged into mempool")
                        .await
                } else {
                    Ok(())
                }
            }

            None => self.orphan_payment(pool, w, row, "gone from chain").await,
        }
    }

    async fn orphan_payment(
        &self,
        pool: &PgPool,
        w: &Watched,
        row: &PaymentRow,
        reason: &str,
    ) -> Result<(), String> {
        let mut db = pool.begin().await.map_err(|e| e.to_string())?;

        let orphaned = sqlx::query!(
            r#"
            UPDATE payments SET status = 'orphaned', updated_at = CURRENT_TIMESTAMP
            WHERE id = $1 AND status <> 'orphaned'
            RETURNING id
            "#,
            row.id
        )
            .fetch_optional(&mut *db)
            .await
            .map_err(|e| format!("orphan: {e}"))?;

        if orphaned.is_none() {
            db.rollback().await.ok();
            return Ok(());
        }

        let totals = settle_invoice(&mut db, w.invoice_id).await?;

        let mut fields = payment_fields(w, &row.tx_hash, row.amount, 0, None);
        fields.insert("reason".into(), Value::String(reason.to_string()));
        fields.insert(
            "amount_received".into(),
            Value::String(totals.received.to_string()),
        );
        fields.insert(
            "amount_expected".into(),
            Value::String(totals.requested.to_string()),
        );
        enqueue_webhook(&mut db, w.invoice_id, "payment.orphaned", &row.tx_hash, fields).await;

        db.commit().await.map_err(|e| e.to_string())?;

        println!(
            "[esplora:{}] orphaned {} on invoice {} ({reason})",
            self.chain_ref, row.tx_hash, w.invoice_id
        );
        Ok(())
    }

    /// Expire only invoices that never saw anything. An invoice with any
    /// non-orphaned payment stays in its derived status and is dealt with by
    /// under/overpayment handling, not by the clock.
    async fn expire_invoices(&self, pool: &PgPool) -> Result<(), String> {
        let rows = sqlx::query!(
            r#"
            UPDATE invoices i
            SET status = 'expired', updated_at = CURRENT_TIMESTAMP
            WHERE i.network_type = $1
              AND i.chain_ref = $2
              AND i.status = 'pending'
              AND i.amount_received = 0
              AND i.expires_at < now()
              AND NOT EXISTS (
                    SELECT 1 FROM payments p
                    WHERE p.invoice_id = i.id AND p.status <> 'orphaned'
              )
            RETURNING i.id
            "#,
            NETWORK_TYPE,
            self.chain_ref
        )
            .fetch_all(pool)
            .await
            .map_err(|e| format!("expire_invoices: {e}"))?;

        if !rows.is_empty() {
            println!(
                "[esplora:{}] expired {} invoice(s)",
                self.chain_ref,
                rows.len()
            );
        }
        Ok(())
    }

    // ── Orphan strike bookkeeping (hint only) ───────────────────────────────

    async fn strike(&self, txid: &str) -> u8 {
        let mut map = self.orphan_strikes.lock().await;
        let entry = map.entry(txid.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    async fn clear_strike(&self, txid: &str) {
        let mut map = self.orphan_strikes.lock().await;
        map.remove(txid);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared write helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Both confirmation latches, in order, guarded so each can only fire once.
/// Returns whether either fired.
async fn promote(
    db: &mut Transaction<'_, Postgres>,
    w: &Watched,
    payment_id: Uuid,
    tx_hash: &str,
    amount: u128,
    confirmations: i64,
    height: u64,
) -> Result<bool, String> {
    let mut fired = false;

    if confirmations >= w.required_confirmations {
        let promoted = sqlx::query!(
            r#"
            UPDATE payments
            SET status = 'merchant_confirmed', confirmations = $2, updated_at = CURRENT_TIMESTAMP
            WHERE id = $1 AND status = 'detected'
            RETURNING id
            "#,
            payment_id,
            confirmations as i32
        )
            .fetch_optional(&mut **db)
            .await
            .map_err(|e| format!("promote confirmed: {e}"))?;

        if promoted.is_some() {
            fired = true;
            let fields = payment_fields(w, tx_hash, amount, confirmations, Some(height));
            enqueue_webhook(db, w.invoice_id, "payment.confirmed", tx_hash, fields).await;
        }
    }

    if confirmations >= FINAL_CONFIRMATIONS {
        let finalized = sqlx::query!(
            r#"
            UPDATE payments
            SET status = 'system_confirmed', confirmations = $2, updated_at = CURRENT_TIMESTAMP
            WHERE id = $1 AND status IN ('detected', 'merchant_confirmed')
            RETURNING id
            "#,
            payment_id,
            confirmations as i32
        )
            .fetch_optional(&mut **db)
            .await
            .map_err(|e| format!("promote finalized: {e}"))?;

        if finalized.is_some() {
            fired = true;
            let fields = payment_fields(w, tx_hash, amount, confirmations, Some(height));
            enqueue_webhook(db, w.invoice_id, "payment.finalized", tx_hash, fields).await;
        }
    }

    Ok(fired)
}

/// Full recompute from non-orphaned payments — never a delta. Takes a row lock
/// so the "first time the total reached the requested amount" latch is exact
/// under concurrent ticks.
async fn settle_invoice(
    db: &mut Transaction<'_, Postgres>,
    invoice_id: Uuid,
) -> Result<InvoiceTotals, String> {
    let prev = sqlx::query!(
        r#"
        SELECT amount_requested, amount_received, status
        FROM invoices WHERE id = $1
        FOR UPDATE
        "#,
        invoice_id
    )
        .fetch_one(&mut **db)
        .await
        .map_err(|e| format!("settle select: {e}"))?;

    let requested = dec_to_u128(prev.amount_requested);
    let previous = dec_to_u128(prev.amount_received);

    let total: Decimal = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(amount), 0) AS "total!"
        FROM payments
        WHERE invoice_id = $1
          AND status <> 'orphaned'
          AND (block_number > 0 OR $2::bool)
        "#,
        invoice_id,
        CREDIT_UNCONFIRMED
    )
        .fetch_one(&mut **db)
        .await
        .map_err(|e| format!("settle sum: {e}"))?;

    let received = dec_to_u128(total);

    // 'expired' is terminal: a late payment updates the amount but never
    // resurrects the status.
    let status = if prev.status == "expired" {
        "expired"
    } else if received == 0 {
        "pending"
    } else if received < requested {
        "underpaid"
    } else if received == requested {
        "paid"
    } else {
        "overpaid"
    };

    sqlx::query!(
        r#"
        UPDATE invoices
        SET amount_received = $2, status = $3, updated_at = CURRENT_TIMESTAMP
        WHERE id = $1
        "#,
        invoice_id,
        u128_to_dec(received),
        status
    )
        .execute(&mut **db)
        .await
        .map_err(|e| format!("settle update: {e}"))?;

    Ok(InvoiceTotals {
        requested,
        received,
        finished_now: status != "expired" && previous < requested && received >= requested,
    })
}

async fn maybe_finish(db: &mut Transaction<'_, Postgres>, w: &Watched, totals: &InvoiceTotals) {
    if !totals.finished_now {
        return;
    }
    let mut fields = Map::new();
    fields.insert("network".into(), Value::String(NETWORK_TYPE.into()));
    fields.insert("address".into(), Value::String(w.address.clone()));
    fields.insert(
        "amount_received".into(),
        Value::String(totals.received.to_string()),
    );
    fields.insert(
        "amount_expected".into(),
        Value::String(totals.requested.to_string()),
    );
    enqueue_webhook(db, w.invoice_id, "payment.finished", "invoice", fields).await;
}

fn payment_fields(
    w: &Watched,
    tx_hash: &str,
    amount: u128,
    confirmations: i64,
    height: Option<u64>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("network".into(), Value::String(NETWORK_TYPE.into()));
    m.insert("address".into(), Value::String(w.address.clone()));
    m.insert("tx_hash".into(), Value::String(tx_hash.to_string()));
    // Strings: satoshi totals are small, but every other network on this
    // processor emits base units as strings and merchants shouldn't have to
    // branch per chain.
    m.insert("amount".into(), Value::String(amount.to_string()));
    m.insert("confirmations".into(), Value::Number(confirmations.into()));
    m.insert(
        "required_confirmations".into(),
        Value::Number(w.required_confirmations.into()),
    );
    m.insert(
        "block_height".into(),
        match height {
            Some(h) => Value::Number(h.into()),
            None => Value::Null,
        },
    );
    m
}

// ─────────────────────────────────────────────────────────────────────────────
// Small helpers
// ─────────────────────────────────────────────────────────────────────────────

fn confirmations_for(height: u64, tip: u64) -> i64 {
    if tip >= height {
        (tip - height + 1) as i64
    } else {
        // Our cached tip is behind the transaction; it is mined, so at least one.
        1
    }
}

fn dec_to_u128(d: Decimal) -> u128 {
    d.trunc().to_u128().unwrap_or(0)
}

fn u128_to_dec(v: u128) -> Decimal {
    Decimal::from_i128_with_scale(v as i128, 0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

const B58_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn base58check_decode(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || s.len() > 64 {
        return None;
    }

    let mut out: Vec<u8> = Vec::with_capacity(32);
    for c in s.bytes() {
        let val = B58_ALPHABET.iter().position(|&b| b == c)? as u32;
        let mut carry = val;
        for byte in out.iter_mut().rev() {
            let x = (*byte as u32) * 58 + carry;
            *byte = (x & 0xff) as u8;
            carry = x >> 8;
        }
        while carry > 0 {
            out.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    for c in s.bytes() {
        if c == b'1' {
            out.insert(0, 0);
        } else {
            break;
        }
    }

    if out.len() < 5 {
        return None;
    }
    let (payload, checksum) = out.split_at(out.len() - 4);
    let digest = Sha256::digest(Sha256::digest(payload));
    if &digest[..4] != checksum {
        return None;
    }
    Some(payload.to_vec())
}

// ─────────────────────────────────────────────────────────────────────────────
// NetworkClient
// ─────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl NetworkClient for EsploraNetwork {
    fn network_type(&self) -> &'static str {
        crate::assets::NETWORK_ESPLORA
    }

    fn chain_ref(&self) -> String {
        match self.network {
            BitcoinNetwork::Mainnet => "mainnet".to_string(),
            BitcoinNetwork::Testnet4 => "testnet4".to_string(),
            BitcoinNetwork::Signet => "signet".to_string(),
        }
    }
    async fn get_derive_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        _invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<(String, u32, Option<String>), String> {
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
            self.index_namespace
        )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to update merchant network index: {e}"))?;

        let index = (row.next_index - 1).max(0) as u32;
        let address = self.derive_address(mnemonic, index)?;

        // Bitcoin has no reference/memo primitive. The address is the only
        // correlation key, and there is no smart path to carry anything else.
        Ok((address, index, None))
    }

    fn validate_address(&self, address: &str) -> bool {
        let trimmed = address.trim();
        if trimmed.is_empty() || trimmed.len() > 100 {
            return false;
        }

        // Bech32 / bech32m (checksum variant is enforced by the decoder against
        // the witness version).
        if let Ok((hrp, version, program)) = segwit::decode(trimmed) {
            if hrp.to_string().to_lowercase() != self.hrp {
                return false;
            }
            return match version.to_u8() {
                0 => program.len() == 20 || program.len() == 32,
                1 => program.len() == 32,
                2..=16 => (2..=40).contains(&program.len()),
                _ => false,
            };
        }

        // Legacy P2PKH / P2SH, accepted for merchant destination wallets even
        // though we never derive them.
        if let Some(payload) = base58check_decode(trimmed) {
            if payload.len() != 21 {
                return false;
            }
            let (p2pkh, p2sh) = match self.network {
                BitcoinNetwork::Mainnet => (0x00u8, 0x05u8),
                BitcoinNetwork::Testnet4 | BitcoinNetwork::Signet => (0x6fu8, 0xc4u8),
            };
            return payload[0] == p2pkh || payload[0] == p2sh;
        }

        false
    }

    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        let resp: EsploraAddressResponse = self
            .get_json(&format!("/address/{address}"))
            .await
            .map_err(|e| e.to_string())?;

        let confirmed = resp.chain_stats.funded_txo_sum - resp.chain_stats.spent_txo_sum;
        let unconfirmed = resp.mempool_stats.funded_txo_sum - resp.mempool_stats.spent_txo_sum;
        let total = (confirmed + unconfirmed).max(0);

        Ok(Amount(total as u128))
    }

    async fn get_token_balance(
        &self,
        _token_address: &str,
        _address: &str,
        _decimals: u8,
    ) -> Result<Amount, String> {
        Err("Bitcoin has no token layer; this network only handles native BTC".to_string())
    }

    async fn get_current_block(&self) -> Result<u64, String> {
        self.fetch_tip_height().await.map_err(|e| e.to_string())
    }

    async fn spin_up(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "🟠 Esplora watcher up: chain_ref={} endpoints={} path=m/84'/{}'/{}'/0/*",
            self.chain_ref,
            self.api_urls.len(),
            self.coin_type,
            self.account
        );

        // Both loops are infinite and swallow their own errors; neither can take
        // the other down.
        futures::future::join(self.chain_loop(pool), self.address_loop(pool)).await;
        Ok(())
    }
}