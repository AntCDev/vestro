//! Per-network outbound worker. Claims `outbound_transfers` rows for its own
//! (network_type, chain_ref), and walks each one through:
//!
//!   pending  → build_and_sign → persist hash+raw (COMMIT) → signed
//!   signed   → broadcast → broadcast
//!   broadcast→ poll → confirmed | failed | expired(→ new pending row)
//!
//! Only `expired` may produce a new signature.
//!
//! SQL policy for this module: every statement goes through `sqlx::query!` /
//! `query_as!`, which only accept a string literal and check it against the
//! live schema at compile time. There is no runtime string building, so no
//! value can reach the parser as SQL. Requires `DATABASE_URL` at build time,
//! or a checked-in `.sqlx/` from `cargo sqlx prepare` for CI.

use std::sync::Arc;
use std::time::Duration;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::keys::derivation::KeyRole;
use crate::keys::store::load_merchant_seed;
use crate::ledgerer::{AddressKind, AssetKey, AssetKind, ChainRef, Ledgerer, OutboundSettled};
use crate::networks::transfers::*;
use crate::networks::NetworkClient;

/// Claim lease. Bound as a parameter and parsed server-side; never spliced
/// into the statement text.
const LEASE: &str = "5 minutes";

pub fn spawn(pool: PgPool, net: Arc<dyn NetworkClient>) {
    tokio::spawn(async move {
        let ledger = Ledgerer::new();
        loop {
            if let Err(e) = tick(&pool, net.as_ref(), &ledger).await {
                eprintln!("outbound[{}/{}]: {e}", net.network_type(), net.chain_ref());
            }
            tokio::time::sleep(net.outbound_poll_interval()).await;
        }
    });
}

async fn tick(pool: &PgPool, net: &dyn NetworkClient, ledger: &Ledgerer) -> Result<(), String> {
    // 1. Fresh work.
    while let Some(row) = claim(pool, net, "pending").await? {
        if let Err(e) = sign_and_persist(pool, net, &row).await {
            fail_soft(pool, row.id, &e).await?;
        }
    }
    // 2. Signed but not broadcast (or crashed mid-broadcast). Rebroadcast raw.
    while let Some(row) = claim(pool, net, "signed").await? {
        let signed = row.signed()?;
        match net.broadcast(&signed).await {
            Ok(()) => {
                sqlx::query!(
                    r#"
                    UPDATE outbound_transfers
                       SET status = 'broadcast', claimed_at = NULL, updated_at = now()
                     WHERE id = $1
                    "#,
                    row.id,
                )
                    .execute(pool)
                    .await
                    .map_err(|e| format!("mark broadcast: {e}"))?;

                sqlx::query!(
                    r#"
                    UPDATE sweep_queue
                       SET status = 'broadcast'
                     WHERE transfer_id = $1 AND status = 'claimed'
                    "#,
                    row.id,
                )
                    .execute(pool)
                    .await
                    .map_err(|e| format!("sweep mark broadcast: {e}"))?;
            }
            Err(e) => fail_soft(pool, row.id, &e).await?,
        }
    }
    // 3. In flight.
    for row in load(pool, net, "broadcast").await? {
        let signed = row.signed()?;
        match net.transfer_status(&signed).await? {
            TransferStatus::Unknown | TransferStatus::Pending => {}
            TransferStatus::Confirmed { block, fee_paid } => settle(pool, ledger, &row, &signed, block, fee_paid).await?,
            TransferStatus::Failed { reason } => terminal(pool, &row, "failed", &reason, true).await?,
            TransferStatus::Expired => supersede(pool, &row).await?,
        }
    }
    Ok(())
}

// ─── row handling ──────────────────────────────────────────────────────────

pub struct TransferRow {
    pub id: Uuid,
    pub merchant_id: Uuid,
    pub asset_id: Uuid,
    pub token_id: Option<String>,
    pub intent: String,
    pub req: TransferRequest,
    tx_hash: Option<String>,
    raw: Option<Vec<u8>>,
    amount_resolved: Option<String>,
    nonce: Option<i64>,
    valid_until: Option<i64>,
    fee_estimate: Option<String>,
    signer_address: Option<String>
}

impl TransferRow {
    fn signed(&self) -> Result<SignedTransfer, String> {
        Ok(SignedTransfer {
            tx_hash: self.tx_hash.clone().ok_or("row has no tx_hash")?,
            raw: self.raw.clone().ok_or("row has no raw_tx")?,
            // The account whose nonce this tx consumed. Equal to from_address
            // except on a vault sweep, where from_address is the contract.
            from: self.signer_address.clone()
                .ok_or_else(|| format!("signed row {} has no signer_address", self.id))?,
            amount: self.amount_resolved.as_deref().ok_or("row has no amount_resolved")?
                .parse().map_err(|e| format!("amount_resolved: {e}"))?,
            valid_until: self.valid_until.map(|v| v as u64),
            nonce: self.nonce.map(|v| v as u64),
            fee_estimate: self.fee_estimate.as_deref().and_then(|s| s.parse().ok()),
        })
    }
}

/// Flat mirror of the `outbound_transfers ⋈ assets` projection. `query_as!`
/// fills this **by position**, so the field order here must match the SELECT
/// list in `fetch_by_id` and `load` exactly. Changing either without the
/// other is a compile error, which is the point.
struct TransferDbRow {
    id: Uuid,
    merchant_id: Uuid,
    asset_id: Uuid,
    token_id: Option<String>,
    intent: String,
    network_type: String,
    chain_ref: String,
    from_address: String,
    from_kind: String,
    authority_role: i16,
    authority_index: i32,
    fee_payer_role: Option<i16>,
    fee_payer_index: Option<i32>,
    to_address: String,
    amount_requested: Option<String>,
    amount_resolved: Option<String>,
    params: Value,
    tx_hash: Option<String>,
    raw_tx: Option<Vec<u8>>,
    nonce: Option<i64>,
    valid_until: Option<i64>,
    fee_estimate: Option<String>,
    asset_kind: String,
    asset_address: Option<String>,
    signer_address: Option<String>
}

impl TryFrom<TransferDbRow> for TransferRow {
    type Error = String;

    fn try_from(r: TransferDbRow) -> Result<Self, Self::Error> {
        let chain = ChainRef::new(r.network_type, r.chain_ref);
        let asset = match r.asset_kind.as_str() {
            "native" => AssetKey::native(chain),
            _ => AssetKey::contract_canonical(chain, r.asset_address.unwrap_or_default()),
        };
        let signer = |role: Option<i16>, idx: Option<i32>| -> Result<Option<SignerRef>, String> {
            Ok(match (role, idx) {
                (Some(role), Some(idx)) => Some(SignerRef {
                    role: KeyRole::from_i16(role)?,
                    index: idx as u32,
                }),
                _ => None,
            })
        };
        let amount = match r.amount_requested {
            None => TransferAmount::Max,
            Some(s) => TransferAmount::Exact(s.parse().map_err(|e| format!("amount_requested: {e}"))?),
        };
        // authority_{role,index} are NOT NULL in the schema, so this is always
        // present; the helper is shared with fee_payer, which really is nullable.
        let authority = signer(Some(r.authority_role), Some(r.authority_index))?
            .ok_or("row has no authority signer")?;

        Ok(TransferRow {
            id: r.id,
            merchant_id: r.merchant_id,
            asset_id: r.asset_id,
            token_id: r.token_id,
            intent: r.intent,
            req: TransferRequest {
                id: r.id,
                merchant_id: r.merchant_id,
                asset,
                from: SourceAccount {
                    address: r.from_address,
                    kind: AddressKind::from_db(r.from_kind.as_str()).ok_or("bad from_kind")?,
                    authority,
                },
                to: r.to_address,
                amount,
                fee_payer: signer(r.fee_payer_role, r.fee_payer_index)?,
                params: r.params,
            },
            tx_hash: r.tx_hash,
            raw: r.raw_tx,
            amount_resolved: r.amount_resolved,
            nonce: r.nonce,
            valid_until: r.valid_until,
            fee_estimate: r.fee_estimate,
            signer_address: None,
        })
    }
}

/// Claim one row in `status` for this chain, taking a lease. SKIP LOCKED so
/// two workers on the same chain (multiple processes) don't collide; the
/// lease handles a worker that died holding a claim.
async fn claim(pool: &PgPool, net: &dyn NetworkClient, status: &str) -> Result<Option<TransferRow>, String> {
    let claimed = sqlx::query!(
        r#"
        WITH c AS (
            SELECT id
              FROM outbound_transfers
             WHERE network_type = $1
               AND chain_ref = $2
               AND status = $3
               AND (claimed_at IS NULL OR claimed_at < now() - $4::text::interval)
             ORDER BY created_at
             LIMIT 1
             FOR UPDATE SKIP LOCKED
        )
        UPDATE outbound_transfers t
           SET claimed_at = now(), attempts = attempts + 1, updated_at = now()
          FROM c
         WHERE t.id = c.id
        RETURNING t.id AS "id!"
        "#,
        net.network_type(),
        net.chain_ref(),
        status,
        LEASE,
    )
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("claim {status}: {e}"))?;

    let Some(claimed) = claimed else { return Ok(None) };
    fetch_by_id(pool, claimed.id).await.map(Some)
}

/// The column list below is duplicated in `load`. `query_as!` takes a literal
/// and nothing else, so a shared `const` or `concat!` is not available here —
/// duplication is the price of compile-time checking. If the projection grows,
/// move it into a view and select from that in both places.
async fn fetch_by_id(pool: &PgPool, id: Uuid) -> Result<TransferRow, String> {
    let row = sqlx::query_as!(
        TransferDbRow,
        r#"
        SELECT t.id                     AS "id!",
               t.merchant_id            AS "merchant_id!",
               t.asset_id               AS "asset_id!",
               t.token_id               AS "token_id?",
               t.intent                 AS "intent!",
               t.network_type           AS "network_type!",
               t.chain_ref              AS "chain_ref!",
               t.from_address           AS "from_address!",
               t.from_kind              AS "from_kind!",
               t.authority_role         AS "authority_role!",
               t.authority_index        AS "authority_index!",
               t.fee_payer_role         AS "fee_payer_role?",
               t.fee_payer_index        AS "fee_payer_index?",
               t.to_address             AS "to_address!",
               t.amount_requested::text AS "amount_requested?",
               t.amount_resolved::text  AS "amount_resolved?",
               t.params                 AS "params!",
               t.tx_hash                AS "tx_hash?",
               t.raw_tx                 AS "raw_tx?",
               t.nonce                  AS "nonce?",
               t.valid_until            AS "valid_until?",
               t.signer_address         AS "signer_address?",
               t.fee_estimate::text     AS "fee_estimate?",
               a.asset_kind             AS "asset_kind!",
               a.address                AS "asset_address?"
          FROM outbound_transfers t
          JOIN assets a ON a.id = t.asset_id
         WHERE t.id = $1
        "#,
        id,
    )
        .fetch_one(pool)
        .await
        .map_err(|e| format!("load transfer {id}: {e}"))?;
    row.try_into()
}

async fn load(pool: &PgPool, net: &dyn NetworkClient, status: &str) -> Result<Vec<TransferRow>, String> {
    sqlx::query_as!(
        TransferDbRow,
        r#"
        SELECT t.id                     AS "id!",
               t.merchant_id            AS "merchant_id!",
               t.asset_id               AS "asset_id!",
               t.token_id               AS "token_id?",
               t.intent                 AS "intent!",
               t.network_type           AS "network_type!",
               t.chain_ref              AS "chain_ref!",
               t.from_address           AS "from_address!",
               t.from_kind              AS "from_kind!",
               t.authority_role         AS "authority_role!",
               t.authority_index        AS "authority_index!",
               t.fee_payer_role         AS "fee_payer_role?",
               t.fee_payer_index        AS "fee_payer_index?",
               t.to_address             AS "to_address!",
               t.amount_requested::text AS "amount_requested?",
               t.amount_resolved::text  AS "amount_resolved?",
               t.params                 AS "params!",
               t.tx_hash                AS "tx_hash?",
               t.raw_tx                 AS "raw_tx?",
               t.nonce                  AS "nonce?",
               t.valid_until            AS "valid_until?",
               t.fee_estimate::text     AS "fee_estimate?",
               t.signer_address         AS "signer_address?",
               a.asset_kind             AS "asset_kind!",
               a.address                AS "asset_address?"
          FROM outbound_transfers t
          JOIN assets a ON a.id = t.asset_id
         WHERE t.network_type = $1
           AND t.chain_ref = $2
           AND t.status = $3
         ORDER BY t.created_at
        "#,
        net.network_type(),
        net.chain_ref(),
        status,
    )
        .fetch_all(pool)
        .await
        .map_err(|e| format!("load {status}: {e}"))?
        .into_iter()
        .map(TransferRow::try_from)
        .collect()
}

// ─── transitions ───────────────────────────────────────────────────────────

/// The one transition that must be atomic *with the DB*: the hash exists in
/// our table before any node has seen the bytes.
async fn sign_and_persist(pool: &PgPool, net: &dyn NetworkClient, row: &TransferRow) -> Result<(), String> {
    let mnemonic = load_merchant_seed(pool, row.merchant_id).await?;
    let signed = net.build_and_sign(pool, &mnemonic, &row.req).await?;
    drop(mnemonic);

    let res = sqlx::query!(
        r#"
        UPDATE outbound_transfers
           SET status = 'signed', tx_hash = $2, raw_tx = $3,
               amount_resolved = $4::text::numeric,
               nonce = $5, valid_until = $6, fee_estimate = $7::text::numeric,
               signer_address = $8,
               claimed_at = NULL, last_error = NULL, updated_at = now()
         WHERE id = $1 AND status = 'pending'
        "#,
        row.id,
        signed.tx_hash.as_str(),
        signed.raw.as_slice(),
        signed.amount.to_string(),
        signed.nonce.map(|n| n as i64),
        signed.valid_until.map(|v| v as i64),
        signed.fee_estimate.map(|f| f.to_string()),
        signed.from.as_str(),
    )
        .execute(pool)
        .await;

    match res {
        Ok(r) if r.rows_affected() == 1 => Ok(()),
        other => {
            // The raw bytes die with this function, so the nonce they used will
            // never be broadcast. Hand it back or every later tx from this
            // address stalls behind the gap.
            //
            // On an Err the UPDATE may in fact have committed (lost response).
            // Releasing then lets a later tx reuse the nonce; one of the two
            // mines and the other goes Expired -> supersede. That's recoverable;
            // a gap is not.
            release_nonce(pool, row, &signed).await;
            Err(match other {
                Err(e) => format!("persist signed tx: {e}"),
                Ok(_) => format!(
                    "persist signed tx: row {} no longer pending, hash {} not stored",
                    row.id, signed.tx_hash
                ),
            })
        }
    }
}

/// Return an allocated-but-unused nonce to chain_nonces. Compare-and-swap: only
/// if nothing was allocated after it. If something was, the gap is real and
/// needs filling by hand (a 0-value self-transfer at that nonce).
async fn release_nonce(pool: &PgPool, row: &TransferRow, signed: &SignedTransfer) {
    let Some(nonce) = signed.nonce else { return }; // nonce-less networks
    let chain = &row.req.asset.chain;
    let res = sqlx::query(
        r#"
        UPDATE chain_nonces
           SET next_nonce = $4, updated_at = now()
         WHERE address = $1 AND network_type = $2 AND chain_ref = $3
           AND next_nonce = $4 + 1
        "#,
    )
        .bind(&signed.from)
        .bind(&chain.network_type)
        .bind(&chain.chain_ref)
        .bind(nonce as i64)
        .execute(pool)
        .await;

    match res {
        Ok(r) if r.rows_affected() == 1 => {
            eprintln!("outbound {}: released nonce {nonce} for {}", row.id, signed.from);
        }
        Ok(_) => eprintln!(
            "outbound {}: NONCE GAP — {nonce} for {} abandoned after later nonces were \
             allocated; txs from this address will stall until it is filled",
            row.id, signed.from
        ),
        Err(e) => eprintln!(
            "outbound {}: could not release nonce {nonce} for {}: {e}",
            row.id, signed.from
        ),
    }
}

/// Transient failure: release the lease, keep the status, record the error.
/// Retry is the next tick. No back-off/attempt cap yet — policy layer.
async fn fail_soft(pool: &PgPool, id: Uuid, err: &str) -> Result<(), String> {
    eprintln!("outbound {id}: {err}");
    sqlx::query!(
        r#"
        UPDATE outbound_transfers
           SET last_error = $2, claimed_at = NULL, updated_at = now()
         WHERE id = $1
        "#,
        id,
        err,
    )
        .execute(pool)
        .await
        .map_err(|e| format!("record error: {e}"))?;
    Ok(())
}

/// Terminal without value having moved. Sweep rows go back to pending so a
/// later transfer can pick them up.
async fn terminal(pool: &PgPool, row: &TransferRow, status: &str, reason: &str, release: bool) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    sqlx::query!(
        r#"
        UPDATE outbound_transfers
           SET status = $2, last_error = $3, claimed_at = NULL, updated_at = now()
         WHERE id = $1
        "#,
        row.id,
        status,
        reason,
    )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("mark {status}: {e}"))?;

    if release {
        sqlx::query!(
            r#"
            UPDATE sweep_queue
               SET status = 'pending', transfer_id = NULL
             WHERE transfer_id = $1
            "#,
            row.id,
        )
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("release sweep rows: {e}"))?;
    }

    tx.commit().await.map_err(|e| e.to_string())
}

/// Expired: the old signature can never land. Mark it and clone the intent
/// into a fresh pending row; sweep rows follow the new row.
async fn supersede(pool: &PgPool, row: &TransferRow) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    sqlx::query!(
        r#"
        UPDATE outbound_transfers
           SET status = 'expired', claimed_at = NULL, updated_at = now()
         WHERE id = $1
        "#,
        row.id,
    )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("mark expired: {e}"))?;

    let new = sqlx::query!(
        r#"
        INSERT INTO outbound_transfers
            (merchant_id, network_type, chain_ref, asset_id, token_id, intent,
             from_address, from_kind, authority_role, authority_index, fee_payer_role, fee_payer_index,
             to_address, amount_requested, params, supersedes)
        SELECT merchant_id, network_type, chain_ref, asset_id, token_id, intent,
               from_address, from_kind, authority_role, authority_index, fee_payer_role, fee_payer_index,
               to_address, amount_requested, params, id
          FROM outbound_transfers
         WHERE id = $1
        RETURNING id AS "id!"
        "#,
        row.id,
    )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("clone intent: {e}"))?;

    sqlx::query!(
        r#"
        UPDATE sweep_queue
           SET transfer_id = $2, status = 'claimed'
         WHERE transfer_id = $1
        "#,
        row.id,
        new.id,
    )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("repoint sweep rows: {e}"))?;

    tx.commit().await.map_err(|e| e.to_string())
}

/// Confirmed: mark, close sweep rows, and let the Ledgerer book it — all in
/// one DB transaction.
async fn settle(pool: &PgPool, ledger: &Ledgerer, row: &TransferRow, signed: &SignedTransfer,
                block: u64, fee_paid: u128) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    let n = sqlx::query!(
        r#"
        UPDATE outbound_transfers
           SET status = 'confirmed', block_number = $2, fee_paid = $3::text::numeric,
               claimed_at = NULL, updated_at = now()
         WHERE id = $1 AND status = 'broadcast'
        "#,
        row.id,
        block as i64,
        fee_paid.to_string(),
    )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("mark confirmed: {e}"))?
        .rows_affected();
    if n == 0 { return Ok(()); } // someone else settled it

    sqlx::query!(
        r#"
        UPDATE sweep_queue
           SET status = 'swept'
         WHERE transfer_id = $1
        "#,
        row.id,
    )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("close sweep rows: {e}"))?;

    let fee_from_kind = row.req.params
        .get("fee_from")
        .and_then(Value::as_str)
        .and_then(AddressKind::from_db)
        .unwrap_or(if row.req.fee_payer.is_some() {
            AddressKind::Gas
        } else if row.req.from.kind == AddressKind::Vault {
            AddressKind::MerchantMain // sweep() is paid for by its caller
        } else {
            row.req.from.kind
        });

    // The account that actually spent the gas: the deposit address itself,
    // the merchant wallet that called sweep() — never the vault — or, on
    // Solana SPL sweeps, the gas feeder named in params. `signed.from` there
    // is the deposit ATA, which has no key and pays nothing.
    let fee_from_address = row.req.params
        .get("fee_payer_address")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| signed.from.clone());

    ledger.record_outbound(&mut tx, &OutboundSettled {
        transfer_id: row.id,
        merchant_id: row.merchant_id,
        intent: row.intent.clone(),
        token_id: row.token_id.clone(),
        chain: row.req.asset.chain.clone(),
        asset: row.req.asset.clone(),
        tx_hash: signed.tx_hash.clone(),
        block_number: block as i64,
        from_address: row.req.from.address.clone(),
        from_kind: row.req.from.kind,
        to_address: row.req.to.clone(),
        to_kind: AddressKind::MerchantMain, // sweep destination; intent-specific later
        amount: Decimal::from_str_exact(&signed.amount.to_string()).map_err(|e| e.to_string())?,
        fee_paid: Decimal::from_str_exact(&fee_paid.to_string()).map_err(|e| e.to_string())?,
        fee_from_kind,
        fee_from_address,

    }).await?;

    tx.commit().await.map_err(|e| e.to_string())
}