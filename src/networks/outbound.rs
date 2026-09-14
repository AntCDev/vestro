//! Per-network outbound worker. Claims `outbound_transfers` rows for its own
//! (network_type, chain_ref), and walks each one through:
//!
//!   pending  → build_and_sign → persist hash+raw (COMMIT) → signed
//!   signed   → broadcast → broadcast
//!   broadcast→ poll → confirmed | failed | expired(→ new pending row)
//!
//! Only `expired` may produce a new signature.
//!
//! SQL policy for this module: every statement is a `&'static str` literal (or
//! a `concat!` of literals, resolved at compile time). Nothing is built with
//! `format!` at runtime, and every value — including the lease interval —
//! arrives as a bound parameter. That means there is no code path where a
//! value can be parsed as SQL.

use std::sync::Arc;
use std::time::Duration;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::keys::derivation::KeyRole;
use crate::keys::store::load_merchant_seed;
use crate::ledgerer::{AddressKind, AssetKey, AssetKind, ChainRef, Ledgerer, OutboundSettled};
use crate::networks::transfers::*;
use crate::networks::NetworkClient;

/// Claim lease. Bound as a parameter and cast server-side, never interpolated.
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
                sqlx::query(MARK_BROADCAST)
                    .bind(row.id)
                    .execute(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                sqlx::query(SWEEP_MARK_BROADCAST)
                    .bind(row.id)
                    .execute(pool)
                    .await
                    .map_err(|e| e.to_string())?;
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
}

impl TransferRow {
    fn signed(&self) -> Result<SignedTransfer, String> {
        Ok(SignedTransfer {
            tx_hash: self.tx_hash.clone().ok_or("row has no tx_hash")?,
            raw: self.raw.clone().ok_or("row has no raw_tx")?,
            amount: self.amount_resolved.as_deref().ok_or("row has no amount_resolved")?
                .parse().map_err(|e| format!("amount_resolved: {e}"))?,
            valid_until: self.valid_until.map(|v| v as u64),
            nonce: self.nonce.map(|v| v as u64),
            fee_estimate: self.fee_estimate.as_deref().and_then(|s| s.parse().ok()),
        })
    }
}

// ─── statements ────────────────────────────────────────────────────────────
//
// The shared column list lives in a `macro_rules!` rather than a `const` so
// that `concat!` can splice it into complete statements at compile time.
// `concat!` only accepts literals, which is exactly the property we want: the
// full statement text is fixed before the binary is built.

macro_rules! select_base {
    () => {
        r#"
    SELECT t.id, t.merchant_id, t.asset_id, t.token_id, t.intent,
           t.network_type, t.chain_ref, t.from_address, t.from_kind,
           t.authority_role, t.authority_index, t.fee_payer_role, t.fee_payer_index,
           t.to_address, t.amount_requested::text AS amount_requested,
           t.amount_resolved::text AS amount_resolved, t.params,
           t.tx_hash, t.raw_tx, t.nonce, t.valid_until, t.fee_estimate::text AS fee_estimate,
           a.asset_kind, a.address AS asset_address
      FROM outbound_transfers t JOIN assets a ON a.id = t.asset_id
"#
    };
}

const SELECT_BY_ID: &str = concat!(select_base!(), "\n     WHERE t.id = $1");

const SELECT_BY_CHAIN_STATUS: &str = concat!(
select_base!(),
"\n     WHERE t.network_type = $1 AND t.chain_ref = $2 AND t.status = $3\n     ORDER BY t.created_at"
);

/// `$4` is the lease. Cast through `text` so the driver can send it as a plain
/// string parameter and Postgres does the interval parse — same shape as the
/// `$n::text::numeric` casts elsewhere in this module.
const CLAIM: &str = r#"
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
    RETURNING t.id
"#;

const MARK_BROADCAST: &str = r#"
    UPDATE outbound_transfers
       SET status = 'broadcast', claimed_at = NULL, updated_at = now()
     WHERE id = $1
"#;

const SWEEP_MARK_BROADCAST: &str = r#"
    UPDATE sweep_queue
       SET status = 'broadcast'
     WHERE transfer_id = $1 AND status = 'claimed'
"#;

const PERSIST_SIGNED: &str = r#"
    UPDATE outbound_transfers
       SET status = 'signed', tx_hash = $2, raw_tx = $3,
           amount_resolved = $4::text::numeric,
           nonce = $5, valid_until = $6, fee_estimate = $7::text::numeric,
           claimed_at = NULL, last_error = NULL, updated_at = now()
     WHERE id = $1 AND status = 'pending'
"#;

const RECORD_ERROR: &str = r#"
    UPDATE outbound_transfers
       SET last_error = $2, claimed_at = NULL, updated_at = now()
     WHERE id = $1
"#;

const MARK_TERMINAL: &str = r#"
    UPDATE outbound_transfers
       SET status = $2, last_error = $3, claimed_at = NULL, updated_at = now()
     WHERE id = $1
"#;

const SWEEP_RELEASE: &str = r#"
    UPDATE sweep_queue
       SET status = 'pending', transfer_id = NULL
     WHERE transfer_id = $1
"#;

const MARK_EXPIRED: &str = r#"
    UPDATE outbound_transfers
       SET status = 'expired', claimed_at = NULL, updated_at = now()
     WHERE id = $1
"#;

const CLONE_INTENT: &str = r#"
    INSERT INTO outbound_transfers
        (merchant_id, network_type, chain_ref, asset_id, token_id, intent,
         from_address, from_kind, authority_role, authority_index, fee_payer_role, fee_payer_index,
         to_address, amount_requested, params, supersedes)
    SELECT merchant_id, network_type, chain_ref, asset_id, token_id, intent,
           from_address, from_kind, authority_role, authority_index, fee_payer_role, fee_payer_index,
           to_address, amount_requested, params, id
      FROM outbound_transfers
     WHERE id = $1
    RETURNING id
"#;

const SWEEP_REPOINT: &str = r#"
    UPDATE sweep_queue
       SET transfer_id = $2, status = 'claimed'
     WHERE transfer_id = $1
"#;

const MARK_CONFIRMED: &str = r#"
    UPDATE outbound_transfers
       SET status = 'confirmed', block_number = $2, fee_paid = $3::text::numeric,
           claimed_at = NULL, updated_at = now()
     WHERE id = $1 AND status = 'broadcast'
"#;

const SWEEP_MARK_SWEPT: &str = r#"
    UPDATE sweep_queue
       SET status = 'swept'
     WHERE transfer_id = $1
"#;

// ─── mapping ───────────────────────────────────────────────────────────────

fn from_row(r: sqlx::postgres::PgRow) -> Result<TransferRow, String> {
    let chain = ChainRef::new(r.get::<String, _>("network_type"), r.get::<String, _>("chain_ref"));
    let asset = match r.get::<String, _>("asset_kind").as_str() {
        "native" => AssetKey::native(chain),
        _ => {
            let addr = r.get::<Option<String>, _>("asset_address").unwrap_or_default();
            AssetKey::contract_canonical(chain, addr)
        }
    };
    let signer = |role: Option<i16>, idx: Option<i32>| -> Result<Option<SignerRef>, String> {
        Ok(match (role, idx) {
            (Some(r), Some(i)) => Some(SignerRef { role: KeyRole::from_i16(r)?, index: i as u32 }),
            _ => None,
        })
    };
    let amount = match r.get::<Option<String>, _>("amount_requested") {
        None => TransferAmount::Max,
        Some(s) => TransferAmount::Exact(s.parse().map_err(|e| format!("amount_requested: {e}"))?),
    };
    let id: Uuid = r.get("id");
    let merchant_id: Uuid = r.get("merchant_id");
    let authority = signer(r.get("authority_role"), r.get("authority_index"))?
        .ok_or("row has no authority signer")?;
    Ok(TransferRow {
        id, merchant_id,
        asset_id: r.get("asset_id"),
        token_id: r.get("token_id"),
        intent: r.get("intent"),
        req: TransferRequest {
            id, merchant_id, asset,
            from: SourceAccount {
                address: r.get("from_address"),
                kind: AddressKind::from_db(r.get::<String, _>("from_kind").as_str()).ok_or("bad from_kind")?,
                authority,
            },
            to: r.get("to_address"),
            amount,
            fee_payer: signer(r.get("fee_payer_role"), r.get("fee_payer_index"))?,
            params: r.get::<Value, _>("params"),
        },
        tx_hash: r.get("tx_hash"),
        raw: r.get("raw_tx"),
        amount_resolved: r.get("amount_resolved"),
        nonce: r.get("nonce"),
        valid_until: r.get("valid_until"),
        fee_estimate: r.get("fee_estimate"),
    })
}

/// Claim one row in `status` for this chain, taking a lease. SKIP LOCKED so
/// two workers on the same chain (multiple processes) don't collide; the
/// lease handles a worker that died holding a claim.
async fn claim(pool: &PgPool, net: &dyn NetworkClient, status: &str) -> Result<Option<TransferRow>, String> {
    let Some(id) = sqlx::query_scalar::<_, Uuid>(CLAIM)
        .bind(net.network_type())
        .bind(net.chain_ref())
        .bind(status)
        .bind(LEASE)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let row = sqlx::query(SELECT_BY_ID)
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    from_row(row).map(Some)
}

async fn load(pool: &PgPool, net: &dyn NetworkClient, status: &str) -> Result<Vec<TransferRow>, String> {
    sqlx::query(SELECT_BY_CHAIN_STATUS)
        .bind(net.network_type())
        .bind(net.chain_ref())
        .bind(status)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(from_row)
        .collect()
}

// ─── transitions ───────────────────────────────────────────────────────────

/// The one transition that must be atomic *with the DB*: the hash exists in
/// our table before any node has seen the bytes.
async fn sign_and_persist(pool: &PgPool, net: &dyn NetworkClient, row: &TransferRow) -> Result<(), String> {
    let mnemonic = load_merchant_seed(pool, row.merchant_id).await?;
    let signed = net.build_and_sign(pool, &mnemonic, &row.req).await?;
    drop(mnemonic);

    let n = sqlx::query(PERSIST_SIGNED)
        .bind(row.id)
        .bind(&signed.tx_hash)
        .bind(&signed.raw)
        .bind(signed.amount.to_string())
        .bind(signed.nonce.map(|n| n as i64))
        .bind(signed.valid_until.map(|v| v as i64))
        .bind(signed.fee_estimate.map(|f| f.to_string()))
        .execute(pool)
        .await
        .map_err(|e| format!("persist signed tx: {e}"))?
        .rows_affected();
    if n == 0 {
        // Row left `pending` under us. We hold a signature that is recorded
        // nowhere — loudly, so it can't be mistaken for a no-op.
        return Err(format!(
            "persist signed tx: row {} no longer pending, hash {} not stored",
            row.id, signed.tx_hash
        ));
    }
    Ok(())
}

/// Transient failure: release the lease, keep the status, record the error.
/// Retry is the next tick. No back-off/attempt cap yet — policy layer.
async fn fail_soft(pool: &PgPool, id: Uuid, err: &str) -> Result<(), String> {
    eprintln!("outbound {id}: {err}");
    sqlx::query(RECORD_ERROR)
        .bind(id)
        .bind(err)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Terminal without value having moved. Sweep rows go back to pending so a
/// later transfer can pick them up.
async fn terminal(pool: &PgPool, row: &TransferRow, status: &str, reason: &str, release: bool) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(MARK_TERMINAL)
        .bind(row.id)
        .bind(status)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    if release {
        sqlx::query(SWEEP_RELEASE)
            .bind(row.id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())
}

/// Expired: the old signature can never land. Mark it and clone the intent
/// into a fresh pending row; sweep rows follow the new row.
async fn supersede(pool: &PgPool, row: &TransferRow) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(MARK_EXPIRED)
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let new_id: Uuid = sqlx::query_scalar(CLONE_INTENT)
        .bind(row.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(SWEEP_REPOINT)
        .bind(row.id)
        .bind(new_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// Confirmed: mark, close sweep rows, and let the Ledgerer book it — all in
/// one DB transaction.
async fn settle(pool: &PgPool, ledger: &Ledgerer, row: &TransferRow, signed: &SignedTransfer,
                block: u64, fee_paid: u128) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let n = sqlx::query(MARK_CONFIRMED)
        .bind(row.id)
        .bind(block as i64)
        .bind(fee_paid.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
    if n == 0 { return Ok(()); } // someone else settled it

    sqlx::query(SWEEP_MARK_SWEPT)
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

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
        fee_from_kind: if row.req.fee_payer.is_some() { AddressKind::Gas } else { row.req.from.kind },
    }).await?;

    tx.commit().await.map_err(|e| e.to_string())
}