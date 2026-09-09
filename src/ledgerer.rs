//! Ledgerer — the only thing that writes to chain_transactions, chain_movements,
//! sweep_queue, ledger_journals and ledger_entries.
//!
//! Network implementations never touch those tables directly. They observe a
//! transfer, decide what it means (which invoice, which path, which addresses),
//! and hand that to the Ledgerer inside the same DB transaction that moves the
//! `payments` row. The Ledgerer knows nothing about slots, blocks, RPCs or
//! token handlers; it knows assets, accounts and journals.
//!
//! Companion to LEDGER.md §5 (recognition) and §10 (ordering hazards).
//! migrations/010_ledger.sql is authoritative for column names and constraints.

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{json, Value};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Vocabulary
// ─────────────────────────────────────────────────────────────────────────────

/// Which chain. `chain_ref` must be whatever the network client's
/// `chain_ref()` returned — never a literal (LEDGER.md §1.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainRef {
    pub network_type: String,
    pub chain_ref: String,
}

impl ChainRef {
    pub fn new(network_type: impl Into<String>, chain_ref: impl Into<String>) -> Self {
        Self { network_type: network_type.into(), chain_ref: chain_ref.into() }
    }
}

/// How strong "final" is on this chain. Decides whether `orphan()` may write a
/// reversal journal or must raise (LEDGER.md §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    /// A rooted slot cannot be undone. A reversal of a recognized journal is
    /// impossible by construction; if one is requested, something upstream lied.
    Absolute,
    /// N confirmations is a probability. Reversals are a live path.
    Probabilistic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetKind {
    Native,
    Contract,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::Native => "native",
            AssetKind::Contract => "contract",
        }
    }
}

/// Identity of an asset, mirroring `assets_identity`. `address` is `None` iff
/// `kind == Native`. The address must already be canonical (lowercase hex for
/// EVM, validated base58 for Solana) — the Ledgerer looks it up, it does not
/// normalise it, because it cannot know the rules for a network it has never
/// seen.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssetKey {
    pub chain: ChainRef,
    pub kind: AssetKind,
    pub address: Option<String>,
}

impl AssetKey {
    pub fn native(chain: ChainRef) -> Self {
        Self { chain, kind: AssetKind::Native, address: None }
    }
    pub fn contract(chain: ChainRef, address: impl Into<String>) -> Self {
        Self { chain, kind: AssetKind::Contract, address: Some(address.into()) }
    }
}

/// `chain_movements.from_kind` / `to_kind`. Classifies the ADDRESS, not the
/// intent (LEDGER.md §2.2, §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressKind {
    External,
    DepositAddress,
    Vault,
    MerchantMain,
    Gas,
    Operator,
}

impl AddressKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AddressKind::External => "external",
            AddressKind::DepositAddress => "deposit_address",
            AddressKind::Vault => "vault",
            AddressKind::MerchantMain => "merchant_main",
            AddressKind::Gas => "gas",
            AddressKind::Operator => "operator",
        }
    }
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "external" => Some(AddressKind::External),
            "deposit_address" => Some(AddressKind::DepositAddress),
            "vault" => Some(AddressKind::Vault),
            "merchant_main" => Some(AddressKind::MerchantMain),
            "gas" => Some(AddressKind::Gas),
            "operator" => Some(AddressKind::Operator),
            _ => None,
        }
    }
    /// Which custody account value at this kind of address lands in.
    fn custody_account(self) -> Option<&'static str> {
        match self {
            AddressKind::DepositAddress | AddressKind::Vault => Some("custody_unswept"),
            AddressKind::MerchantMain => Some("custody_treasury"),
            AddressKind::Gas => Some("custody_gas"),
            AddressKind::External | AddressKind::Operator => None,
        }
    }
}

/// Mirrors `payments.payment_path`. Says how the payment was *identified*:
/// `Direct` = it hit a per-invoice deposit address, `Reference` = it carried
/// an invoice identifier (vault log, Solana reference key). It says nothing
/// about where the value sits — that is `to_kind` on the movement, and the
/// Ledgerer books custody from that, never from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentPath {
    Direct,
    Reference,
}
impl PaymentPath {
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "direct" => Some(PaymentPath::Direct),
            "reference" => Some(PaymentPath::Reference),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            PaymentPath::Direct => "direct",
            PaymentPath::Reference => "reference",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Inputs
// ─────────────────────────────────────────────────────────────────────────────

/// One value-moving event inside an observed transaction. Becomes one
/// `chain_movements` row.
#[derive(Debug, Clone)]
pub struct ObservedTransfer {
    /// Connector-assigned stable ordinal (LEDGER.md §2.3).
    pub event_index: i32,
    /// Native identifier verbatim: "log:12", "tokbal:8", "vout:1".
    pub event_ref: Option<String>,

    pub asset: AssetKey,
    pub amount: Decimal,

    pub from_address: Option<String>,
    pub from_kind: Option<AddressKind>,
    pub to_address: Option<String>,
    pub to_kind: Option<AddressKind>,

    pub merchant_id: Option<Uuid>,
    pub invoice_id: Option<Uuid>,
    pub payment_id: Option<Uuid>,
    /// The route that detected this, if any. Recorded, never keyed on.
    pub token_id: Option<String>,
}

/// An inbound transaction the watcher just saw, with every transfer in it that
/// concerns us. Idempotent: replaying the same transaction is a no-op except
/// for resurrecting an orphaned row.
#[derive(Debug, Clone)]
pub struct ObservedInbound {
    pub chain: ChainRef,
    pub tx_hash: String,
    pub block_number: Option<i64>,
    pub block_hash: Option<String>,
    /// From the block. `None` is allowed (pruned Solana txs); recognition
    /// falls back and records `occurred_at_exact: false`.
    pub block_time: Option<DateTime<Utc>>,
    pub merchant_id: Option<Uuid>,
    pub token_id: Option<String>,
    pub transfers: Vec<ObservedTransfer>,
}

/// Where recognized value physically sits and who can sign for it
/// (LEDGER.md §2.8). Only needed for `PaymentPath::Direct`.
/// Only needed when the value landed in custody_unswept (deposit address or vault). and MissingCustody Display: "payment {payment_id} landed in custody_unswept but no Custody was supplied".
#[derive(Debug, Clone)]
pub struct Custody {
    pub address: String,
    pub kind: AddressKind, // DepositAddress | Vault
    pub authority_address: String,
    pub authority_ref: Option<String>,
    pub sweep_params: Value,
}

#[derive(Debug, Clone)]
pub struct RecognizeInput {
    pub chain: ChainRef,
    pub tx_hash: String,
    pub payment_id: Uuid,
    pub invoice_id: Uuid,
    pub merchant_id: Uuid,
    /// The route the invoice was created with. Used for fee-rate resolution
    /// and snapshotted into journal metadata — never as an identity.
    pub token_id: String,
    pub path: PaymentPath,
    /// Overrides what `record_detected` stamped, if the watcher has a better
    /// value now (e.g. blockTime became available at finality).
    pub block_time: Option<DateTime<Utc>>,
    pub custody: Option<Custody>,
    /// §10.1: the sweeper already moved this value before it finalized.
    /// Books straight into treasury, enqueues nothing. `false` until
    /// `payments.swept_by_tx_id` exists.
    pub already_swept: bool,
}

#[derive(Debug, Clone)]
pub struct OrphanInput {
    pub chain: ChainRef,
    pub tx_hash: String,
    pub payment_id: Uuid,
    pub finality: Finality,
    /// Free text for journal metadata: "signature not found", "block hash changed".
    pub reason: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Outcomes and errors
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RecognitionOutcome {
    pub tx_id: Uuid,
    /// `None` if the latch already held (journal existed).
    pub journal_id: Option<Uuid>,
    pub amount: Decimal,
    pub fee: Decimal,
    pub fee_bps: i32,
    pub sweep_rows_enqueued: usize,
}

#[derive(Debug, Clone)]
pub enum OrphanOutcome {
    /// Transaction was never recognized; chain layer flipped, queue rows dropped.
    ChainOnly { tx_id: Uuid },
    /// A recognized journal existed and was reversed.
    Reversed { tx_id: Uuid, reversal_journal_id: Uuid },
    /// Nothing to do: chain layer already orphaned and no live journal.
    NoOp,
}

#[derive(Debug)]
pub enum LedgerError {
    Db(sqlx::Error),
    UnknownAsset(AssetKey),
    /// The payment has no movement attached to this tx. Detection never
    /// called `record_detected`, or called it without a payment_id.
    NoMovementForPayment { payment_id: Uuid, tx_hash: String },
    /// Movements for one payment named more than one asset. One invoice, one
    /// asset — this is a classifier bug, not a ledger case.
    MixedAssets { payment_id: Uuid },
    /// `PaymentPath::Direct` without `Custody`: nowhere to enqueue the sweep.
    MissingCustody { payment_id: Uuid },
    /// A recognized journal exists on a chain whose finality is absolute.
    /// The confirmation ladder is wrong or an RPC lied. Alarm, don't reverse.
    ImpossibleReversal { payment_id: Uuid, journal_id: Uuid },
    /// A sweep row for this value is claimed or broadcast. Value just vanished
    /// under a sweep in flight; this is not something to clean up quietly.
    SweepInFlight { payment_id: Uuid, rows: i64 },
    /// Movements for one payment landed at more than one kind of address.
    MixedCustody { payment_id: Uuid },
    /// The movement's `to_kind` has no custody account (external/operator/NULL).
    /// The connector classified the destination wrong.
    NoCustodyAccount { payment_id: Uuid, to_kind: Option<String> },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Db(e) => write!(f, "ledger db error: {e}"),
            LedgerError::UnknownAsset(k) => write!(
                f,
                "asset not registered: {}/{}/{}/{}",
                k.chain.network_type,
                k.chain.chain_ref,
                k.kind.as_str(),
                k.address.as_deref().unwrap_or("-")
            ),
            LedgerError::NoMovementForPayment { payment_id, tx_hash } => write!(
                f,
                "payment {payment_id} has no chain_movement on tx {tx_hash}; was record_detected called?"
            ),
            LedgerError::MixedAssets { payment_id } => {
                write!(f, "payment {payment_id} movements span more than one asset")
            }
            LedgerError::MissingCustody { payment_id } => {
                write!(f, "payment {payment_id} is direct-path but no Custody was supplied")
            }
            LedgerError::ImpossibleReversal { payment_id, journal_id } => write!(
                f,
                "ALARM: payment {payment_id} orphaned on an absolute-finality chain but journal {journal_id} already exists"
            ),
            LedgerError::SweepInFlight { payment_id, rows } => write!(
                f,
                "ALARM: payment {payment_id} orphaned while {rows} sweep row(s) are claimed/broadcast"
            ),
            LedgerError::MixedCustody { payment_id } => {
                write!(f, "payment {payment_id} movements landed at more than one address kind")
            }
            LedgerError::NoCustodyAccount { payment_id, to_kind } => write!(
                f,
                "payment {payment_id} landed at to_kind {to_kind:?}, which has no custody account"
            ),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<sqlx::Error> for LedgerError {
    fn from(e: sqlx::Error) -> Self {
        LedgerError::Db(e)
    }
}

/// Lets `?` flow into the `Result<_, String>` the network code uses today.
impl From<LedgerError> for String {
    fn from(e: LedgerError) -> Self {
        e.to_string()
    }
}

pub type LedgerResult<T> = Result<T, LedgerError>;

// ─────────────────────────────────────────────────────────────────────────────
// The Ledgerer
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Ledgerer;

#[derive(Debug, Clone)]
struct AssetRow {
    id: Uuid,
    kind: String,
    registered: bool,
}

#[derive(Debug, Clone)]
struct MovementRow {
    id: Uuid,
    asset_id: Uuid,
    amount: Decimal,
    to_kind: Option<String>,
}

#[derive(Debug, Clone)]
struct FeeRate {
    bps: i32,
    source: &'static str,
}

impl Ledgerer {
    pub fn new() -> Self {
        Self
    }

    // ── Write point 1: detection ────────────────────────────────────────────

    /// Upsert the transaction as `detected` and append its movements.
    /// Nothing enters the ledger here (LEDGER.md §5.2 row 1).
    ///
    /// Re-running on the same tx is a no-op except: an `orphaned` transaction
    /// that re-lands is flipped back to `detected` with its new block. A
    /// transaction already at `confirmed`/`final` is never regressed.
    ///
    /// Returns the `chain_transactions.id`.
    pub async fn record_detected(
        &self,
        conn: &mut PgConnection,
        obs: &ObservedInbound,
    ) -> LedgerResult<Uuid> {
        let tx_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO chain_transactions
                (network_type, chain_ref, tx_hash, intent, merchant_id, token_id,
                 block_number, block_hash, block_time, status)
            VALUES ($1, $2, $3, 'inbound', $4, $5, $6, $7, $8, 'detected')
            ON CONFLICT (network_type, chain_ref, tx_hash) DO UPDATE
               SET block_number = COALESCE(EXCLUDED.block_number, chain_transactions.block_number),
                   block_hash   = COALESCE(EXCLUDED.block_hash,   chain_transactions.block_hash),
                   block_time   = COALESCE(EXCLUDED.block_time,   chain_transactions.block_time),
                   status       = CASE WHEN chain_transactions.status = 'orphaned'
                                       THEN 'detected'
                                       ELSE chain_transactions.status END
            RETURNING id
            "#,
        )
            .bind(&obs.chain.network_type)
            .bind(&obs.chain.chain_ref)
            .bind(&obs.tx_hash)
            .bind(obs.merchant_id)
            .bind(&obs.token_id)
            .bind(obs.block_number)
            .bind(&obs.block_hash)
            .bind(obs.block_time)
            .fetch_one(&mut *conn)
            .await?;

        for t in &obs.transfers {
            let asset = self.find_asset(conn, &t.asset).await?;

            // Append-only table: ON CONFLICT DO NOTHING is the only legal
            // conflict action. A re-landed tx has the same hash and therefore
            // the same transfers, so DO NOTHING is also correct.
            sqlx::query(
                r#"
                INSERT INTO chain_movements
                    (tx_id, network_type, chain_ref, event_index, event_ref,
                     merchant_id, invoice_id, payment_id,
                     asset_id, token_id, amount,
                     from_address, from_kind, to_address, to_kind)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
                ON CONFLICT (tx_id, event_index) DO NOTHING
                "#,
            )
                .bind(tx_id)
                .bind(&obs.chain.network_type)
                .bind(&obs.chain.chain_ref)
                .bind(t.event_index)
                .bind(&t.event_ref)
                .bind(t.merchant_id)
                .bind(t.invoice_id)
                .bind(t.payment_id)
                .bind(asset.id)
                .bind(&t.token_id)
                .bind(t.amount)
                .bind(&t.from_address)
                .bind(t.from_kind.map(AddressKind::as_str))
                .bind(&t.to_address)
                .bind(t.to_kind.map(AddressKind::as_str))
                .execute(&mut *conn)
                .await?;
        }

        Ok(tx_id)
    }

    // ── Write point 2: merchant threshold ───────────────────────────────────

    /// `detected` → `confirmed`. Chain layer only; the ledger does not move.
    pub async fn mark_confirmed(
        &self,
        conn: &mut PgConnection,
        chain: &ChainRef,
        tx_hash: &str,
    ) -> LedgerResult<()> {
        sqlx::query(
            r#"
            UPDATE chain_transactions
               SET status = 'confirmed'
             WHERE network_type = $1 AND chain_ref = $2 AND tx_hash = $3
               AND status = 'detected'
            "#,
        )
            .bind(&chain.network_type)
            .bind(&chain.chain_ref)
            .bind(tx_hash)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    // ── Write point 3: recognition ──────────────────────────────────────────

    /// Value enters the ledger. MUST run in the same transaction as the
    /// guarded `UPDATE payments SET status = 'system_confirmed'` (§5.4).
    ///
    /// - chain_transactions → `final`, block_time stamped
    /// - one `payment_recognized` journal behind `payment_recognized:<payment_id>`
    /// - value legs into the custody account implied by the movement's
    ///   `to_kind` (deposit_address/vault → custody_unswept,
    ///   merchant_main → custody_treasury), against payable_to_merchant.
    ///   `payment_path` is snapshotted into metadata only.
    /// - fee legs at the route's resolved rate, if the fee rounds above zero
    /// - one sweep_queue row per movement whenever value landed in custody_unswept
    ///   and was not already swept
    pub async fn recognize_payment(
        &self,
        conn: &mut PgConnection,
        input: &RecognizeInput,
    ) -> LedgerResult<RecognitionOutcome> {
        // 1. Chain layer to final.
        let row = sqlx::query(
            r#"
            UPDATE chain_transactions
               SET status     = 'final',
                   block_time = COALESCE($4, block_time)
             WHERE network_type = $1 AND chain_ref = $2 AND tx_hash = $3
               AND status <> 'orphaned'
            RETURNING id, block_time, block_number
            "#,
        )
            .bind(&input.chain.network_type)
            .bind(&input.chain.chain_ref)
            .bind(&input.tx_hash)
            .bind(input.block_time)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| LedgerError::NoMovementForPayment {
                payment_id: input.payment_id,
                tx_hash: input.tx_hash.clone(),
            })?;

        let tx_id: Uuid = row.get("id");
        let block_time: Option<DateTime<Utc>> = row.get("block_time");
        let block_number: Option<i64> = row.get("block_number");

        // 2. The movements this payment is built from.
        let movements = self.movements_for_payment(conn, tx_id, input.payment_id).await?;
        if movements.is_empty() {
            return Err(LedgerError::NoMovementForPayment {
                payment_id: input.payment_id,
                tx_hash: input.tx_hash.clone(),
            });
        }
        let asset_id = movements[0].asset_id;
        if movements.iter().any(|m| m.asset_id != asset_id) {
            return Err(LedgerError::MixedAssets { payment_id: input.payment_id });
        }
        let amount: Decimal = movements.iter().map(|m| m.amount).sum();



        // 3. Custody account: where does the value physically sit? Decided by
        //    the movement's to_kind, never by payment_path. A vault Payment log
        //    and a deposit-address transfer are both "unswept" — the vault
        //    holds `_vault[token][merchant]` until sweep(token) is called. A
        //    Solana reference payment straight into the merchant's ATA is
        //    "treasury".
        let to_kind = movements[0].to_kind.as_deref();
        if movements.iter().any(|m| m.to_kind.as_deref() != to_kind) {
            return Err(LedgerError::MixedCustody { payment_id: input.payment_id });
        }
        let landed_in = to_kind
            .and_then(AddressKind::from_db)
            .and_then(AddressKind::custody_account)
            .ok_or_else(|| LedgerError::NoCustodyAccount {
                payment_id: input.payment_id,
                to_kind: to_kind.map(str::to_owned),
            })?;

        let asset_registered: bool =
            sqlx::query_scalar("SELECT registered FROM assets WHERE id = $1")
                .bind(asset_id)
                .fetch_one(&mut *conn)
                .await?;

        let (custody_kind, enqueue) = if !asset_registered {
            ("custody_unsupported", false) // §3.1: owed, not movable
        } else if input.already_swept {
            ("custody_treasury", false) // §10.1: already in treasury
        } else {
            (landed_in, landed_in == "custody_unswept")
        };

        if enqueue && input.custody.is_none() {
            return Err(LedgerError::MissingCustody { payment_id: input.payment_id });
        }


        // 4. Fee rate, resolved at occurred_at, snapshotted into metadata.
        let (occurred_at, exact) = match block_time {
            Some(t) => (t, true),
            None => (Utc::now(), false),
        };
        let rate = self
            .resolve_fee_rate(conn, input.merchant_id, &input.token_id, occurred_at)
            .await?;
        let fee = fee_half_up(amount, rate.bps);

        // 5. The latch. If it already held, someone recognized this before
        //    us — possibly, on a probabilistic chain, then reversed it and the
        //    tx re-landed. That case gets a fresh key; the plain duplicate
        //    returns without writing.
        let dedupe_key = self
            .recognition_dedupe_key(conn, input.payment_id, block_number, &input.tx_hash)
            .await?;

        let metadata = json!({
            "fee_bps": rate.bps,
            "rate_source": rate.source,
            "token_id": input.token_id,
            "payment_path": input.path.as_str(),
            "already_swept": input.already_swept,
            "asset_registered": asset_registered,
            "occurred_at_exact": exact,
        });

        let Some(journal_id) = self
            .insert_journal(
                conn,
                "payment_recognized",
                &dedupe_key,
                Some(input.merchant_id),
                Some(tx_id),
                Some(input.payment_id),
                None,
                metadata,
                occurred_at,
            )
            .await?
        else {
            return Ok(RecognitionOutcome {
                tx_id,
                journal_id: None,
                amount,
                fee,
                fee_bps: rate.bps,
                sweep_rows_enqueued: 0,
            });
        };

        // 6. Legs.
        let custody = self.account(conn, Some(input.merchant_id), custody_kind, asset_id).await?;
        let payable = self
            .account(conn, Some(input.merchant_id), "payable_to_merchant", asset_id)
            .await?;

        self.post(conn, journal_id, custody, asset_id, amount).await?;
        self.post(conn, journal_id, payable, asset_id, -amount).await?;

        if fee > Decimal::ZERO {
            let recv = self
                .account(conn, Some(input.merchant_id), "fees_receivable", asset_id)
                .await?;
            let revenue = self.account(conn, None, "fee_revenue", asset_id).await?;
            self.post(conn, journal_id, recv, asset_id, fee).await?;
            self.post(conn, journal_id, revenue, asset_id, -fee).await?;
        }

        // 7. Sweep queue, one row per movement.
        let mut enqueued = 0usize;
        if enqueue {
            let c = input.custody.as_ref().expect("checked above");
            for m in &movements {
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO sweep_queue
                        (movement_id, merchant_id, asset_id, amount,
                         network_type, chain_ref,
                         custody_address, custody_kind,
                         authority_address, authority_ref,
                         token_id, sweep_params)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL, $11)
                    ON CONFLICT (movement_id) DO NOTHING
                    "#,
                )
                    .bind(m.id)
                    .bind(input.merchant_id)
                    .bind(asset_id)
                    .bind(m.amount)
                    .bind(&input.chain.network_type)
                    .bind(&input.chain.chain_ref)
                    .bind(&c.address)
                    .bind(c.kind.as_str())
                    .bind(&c.authority_address)
                    .bind(&c.authority_ref)
                    .bind(&c.sweep_params)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected();
                enqueued += inserted as usize;
            }
        }

        Ok(RecognitionOutcome {
            tx_id,
            journal_id: Some(journal_id),
            amount,
            fee,
            fee_bps: rate.bps,
            sweep_rows_enqueued: enqueued,
        })
    }

    // ── Write point 4: orphan ───────────────────────────────────────────────

    /// The transaction fell out of the chain.
    ///
    /// Chain layer → `orphaned`. Pending sweep rows for its movements are
    /// deleted (work queue, not audit trail). If a `payment_recognized`
    /// journal exists: on a `Probabilistic` chain a `reversal` journal is
    /// written; on an `Absolute` chain this returns `ImpossibleReversal` and
    /// the caller should roll back and alarm.
    pub async fn orphan(
        &self,
        conn: &mut PgConnection,
        input: &OrphanInput,
    ) -> LedgerResult<OrphanOutcome> {
        let Some(tx_id) = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE chain_transactions
               SET status = 'orphaned'
             WHERE network_type = $1 AND chain_ref = $2 AND tx_hash = $3
               AND status <> 'orphaned'
            RETURNING id
            "#,
        )
            .bind(&input.chain.network_type)
            .bind(&input.chain.chain_ref)
            .bind(&input.tx_hash)
            .fetch_optional(&mut *conn)
            .await?
        else {
            return Ok(OrphanOutcome::NoOp);
        };

        // Any sweep already touching this value is a real problem.
        let in_flight: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM sweep_queue sq
              JOIN chain_movements m ON m.id = sq.movement_id
             WHERE m.tx_id = $1 AND m.payment_id = $2
               AND sq.status IN ('claimed', 'broadcast')
            "#,
        )
            .bind(tx_id)
            .bind(input.payment_id)
            .fetch_one(&mut *conn)
            .await?;
        if in_flight > 0 {
            return Err(LedgerError::SweepInFlight { payment_id: input.payment_id, rows: in_flight });
        }

        sqlx::query(
            r#"
            DELETE FROM sweep_queue
             WHERE status = 'pending'
               AND movement_id IN (
                   SELECT id FROM chain_movements WHERE tx_id = $1 AND payment_id = $2
               )
            "#,
        )
            .bind(tx_id)
            .bind(input.payment_id)
            .execute(&mut *conn)
            .await?;

        // Live recognition journal, if any (one that has not itself been reversed).
        let live_journal: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(
            r#"
            SELECT j.id, j.occurred_at
              FROM ledger_journals j
             WHERE j.kind = 'payment_recognized'
               AND j.payment_id = $1
               AND NOT EXISTS (SELECT 1 FROM ledger_journals r WHERE r.reverses = j.id)
             ORDER BY j.created_at DESC
             LIMIT 1
            "#,
        )
            .bind(input.payment_id)
            .fetch_optional(&mut *conn)
            .await?;

        let Some((journal_id, _)) = live_journal else {
            return Ok(OrphanOutcome::ChainOnly { tx_id });
        };

        if input.finality == Finality::Absolute {
            return Err(LedgerError::ImpossibleReversal { payment_id: input.payment_id, journal_id });
        }

        let reversal_id = self.reverse_journal(conn, journal_id, &input.reason).await?;
        Ok(OrphanOutcome::Reversed { tx_id, reversal_journal_id: reversal_id })
    }

    // ── Reversal ────────────────────────────────────────────────────────────

    /// Write the exact negation of `original`. The deferred trigger proves it
    /// cancels per (account, asset); `ledger_journals_reverses_uq` proves it
    /// happens once. Returns the existing reversal id if the latch held.
    pub async fn reverse_journal(
        &self,
        conn: &mut PgConnection,
        original: Uuid,
        reason: &str,
    ) -> LedgerResult<Uuid> {
        let dedupe_key = format!("reversal:{original}");

        let orig = sqlx::query(
            "SELECT merchant_id, tx_id, payment_id FROM ledger_journals WHERE id = $1",
        )
            .bind(original)
            .fetch_one(&mut *conn)
            .await?;

        let Some(reversal_id) = self
            .insert_journal(
                conn,
                "reversal",
                &dedupe_key,
                orig.get("merchant_id"),
                orig.get("tx_id"),
                orig.get("payment_id"),
                Some(original),
                json!({ "reason": reason }),
                Utc::now(), // the reversal happened now; the original keeps its block time
            )
            .await?
        else {
            let existing: Uuid =
                sqlx::query_scalar("SELECT id FROM ledger_journals WHERE dedupe_key = $1")
                    .bind(&dedupe_key)
                    .fetch_one(&mut *conn)
                    .await?;
            return Ok(existing);
        };

        sqlx::query(
            r#"
            INSERT INTO ledger_entries (journal_id, account_id, asset_id, amount)
            SELECT $2, account_id, asset_id, -amount
              FROM ledger_entries
             WHERE journal_id = $1
            "#,
        )
            .bind(original)
            .bind(reversal_id)
            .execute(&mut *conn)
            .await?;

        Ok(reversal_id)
    }

    // ── Asset registry helpers ──────────────────────────────────────────────

    /// Look up an asset by identity. Errors if unknown: for the payment path
    /// the asset must have a handler and therefore a row. Reconciliation uses
    /// `ensure_observed_asset` instead.
    async fn find_asset(&self, conn: &mut PgConnection, key: &AssetKey) -> LedgerResult<AssetRow> {
        let row = sqlx::query(
            r#"
            SELECT id, asset_kind, registered
              FROM assets
             WHERE network_type = $1 AND chain_ref = $2 AND asset_kind = $3
               AND address IS NOT DISTINCT FROM $4
            "#,
        )
            .bind(&key.chain.network_type)
            .bind(&key.chain.chain_ref)
            .bind(key.kind.as_str())
            .bind(&key.address)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| LedgerError::UnknownAsset(key.clone()))?;

        Ok(AssetRow {
            id: row.get("id"),
            kind: row.get("asset_kind"),
            registered: row.get("registered"),
        })
    }

    /// For reconciliation (LEDGER.md §3.1): an asset nobody registered.
    /// decimals = 0, symbol = 'UNKNOWN', registered = false. Never overwrites
    /// an existing row.
    pub async fn ensure_observed_asset(
        &self,
        conn: &mut PgConnection,
        key: &AssetKey,
        params: Value,
    ) -> LedgerResult<Uuid> {
        sqlx::query(
            r#"
            INSERT INTO assets
                (network_type, chain_ref, asset_kind, address, decimals, symbol, asset_params, registered)
            VALUES ($1, $2, $3, $4, 0, 'UNKNOWN', $5, false)
            ON CONFLICT (network_type, chain_ref, asset_kind, address) DO NOTHING
            "#,
        )
            .bind(&key.chain.network_type)
            .bind(&key.chain.chain_ref)
            .bind(key.kind.as_str())
            .bind(&key.address)
            .bind(params)
            .execute(&mut *conn)
            .await?;
        Ok(self.find_asset(conn, key).await?.id)
    }

    // ── Internals ───────────────────────────────────────────────────────────

    async fn movements_for_payment(
        &self,
        conn: &mut PgConnection,
        tx_id: Uuid,
        payment_id: Uuid,
    ) -> LedgerResult<Vec<MovementRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, asset_id, amount, to_kind
              FROM chain_movements
             WHERE tx_id = $1 AND payment_id = $2
             ORDER BY event_index
            "#,
        )
            .bind(tx_id)
            .bind(payment_id)
            .fetch_all(&mut *conn)
            .await?;

        Ok(rows
            .into_iter()
            .map(|r| MovementRow {
                id: r.get("id"),
                asset_id: r.get("asset_id"),
                amount: r.get("amount"),
                to_kind: r.get("to_kind"),
            })
            .collect())
    }

    /// merchant override, else operator default, latest effective_from <= at.
    /// No row → 0 bps, source "unconfigured". Never fails recognition.
    async fn resolve_fee_rate(
        &self,
        conn: &mut PgConnection,
        merchant_id: Uuid,
        token_id: &str,
        at: DateTime<Utc>,
    ) -> LedgerResult<FeeRate> {
        let row: Option<(i32, bool)> = sqlx::query_as(
            r#"
            SELECT basis_points, merchant_id IS NOT NULL AS is_override
              FROM fee_rates
             WHERE token_id = $1
               AND (merchant_id = $2 OR merchant_id IS NULL)
               AND effective_from <= $3
             ORDER BY (merchant_id IS NOT NULL) DESC, effective_from DESC
             LIMIT 1
            "#,
        )
            .bind(token_id)
            .bind(merchant_id)
            .bind(at)
            .fetch_optional(&mut *conn)
            .await?;

        Ok(match row {
            Some((bps, true)) => FeeRate { bps, source: "merchant_override" },
            Some((bps, false)) => FeeRate { bps, source: "operator_default" },
            None => FeeRate { bps: 0, source: "unconfigured" },
        })
    }

    /// `payment_recognized:<payment_id>` normally. If that journal exists AND
    /// has been reversed (probabilistic chain: orphaned, then re-landed), the
    /// payment is genuinely being recognized a second time and gets a key
    /// scoped to the block it landed in this time.
    async fn recognition_dedupe_key(
        &self,
        conn: &mut PgConnection,
        payment_id: Uuid,
        block_number: Option<i64>,
        tx_hash: &str,
    ) -> LedgerResult<String> {
        let base = format!("payment_recognized:{payment_id}");
        let reversed: Option<bool> = sqlx::query_scalar(
            r#"
            SELECT EXISTS (SELECT 1 FROM ledger_journals r WHERE r.reverses = j.id)
              FROM ledger_journals j
             WHERE j.dedupe_key = $1
            "#,
        )
            .bind(&base)
            .fetch_optional(&mut *conn)
            .await?;

        Ok(match reversed {
            Some(true) => format!(
                "{base}:relanded:{}:{}",
                block_number.map(|b| b.to_string()).unwrap_or_else(|| "?".into()),
                &tx_hash[..tx_hash.len().min(16)]
            ),
            _ => base,
        })
    }

    /// Get-or-create `(merchant_id, kind, asset_id)`. Concurrent creators
    /// collide on `ledger_accounts_identity`, which ON CONFLICT absorbs.
    async fn account(
        &self,
        conn: &mut PgConnection,
        merchant_id: Option<Uuid>,
        kind: &str,
        asset_id: Uuid,
    ) -> LedgerResult<Uuid> {
        let asset_kind: String = sqlx::query_scalar("SELECT asset_kind FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_one(&mut *conn)
            .await?;

        sqlx::query(
            r#"
            INSERT INTO ledger_accounts (merchant_id, kind, asset_id, asset_kind)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (merchant_id, kind, asset_id) DO NOTHING
            "#,
        )
            .bind(merchant_id)
            .bind(kind)
            .bind(asset_id)
            .bind(&asset_kind)
            .execute(&mut *conn)
            .await?;

        let id: Uuid = sqlx::query_scalar(
            r#"
            SELECT id FROM ledger_accounts
             WHERE merchant_id IS NOT DISTINCT FROM $1 AND kind = $2 AND asset_id = $3
            "#,
        )
            .bind(merchant_id)
            .bind(kind)
            .bind(asset_id)
            .fetch_one(&mut *conn)
            .await?;

        Ok(id)
    }

    /// `Some(id)` if written, `None` if the dedupe_key already existed.
    #[allow(clippy::too_many_arguments)]
    async fn insert_journal(
        &self,
        conn: &mut PgConnection,
        kind: &str,
        dedupe_key: &str,
        merchant_id: Option<Uuid>,
        tx_id: Option<Uuid>,
        payment_id: Option<Uuid>,
        reverses: Option<Uuid>,
        metadata: Value,
        occurred_at: DateTime<Utc>,
    ) -> LedgerResult<Option<Uuid>> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO ledger_journals
                (kind, dedupe_key, merchant_id, tx_id, payment_id, reverses, metadata, occurred_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
            "#,
        )
            .bind(kind)
            .bind(dedupe_key)
            .bind(merchant_id)
            .bind(tx_id)
            .bind(payment_id)
            .bind(reverses)
            .bind(metadata)
            .bind(occurred_at)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(id)
    }

    /// One leg. Zero is skipped: `amount <> 0` would reject it, and a zero
    /// leg records nothing anyway.
    async fn post(
        &self,
        conn: &mut PgConnection,
        journal_id: Uuid,
        account_id: Uuid,
        asset_id: Uuid,
        amount: Decimal,
    ) -> LedgerResult<()> {
        if amount.is_zero() {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO ledger_entries (journal_id, account_id, asset_id, amount) VALUES ($1, $2, $3, $4)",
        )
            .bind(journal_id)
            .bind(account_id)
            .bind(asset_id)
            .bind(amount)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }
}

/// amount × bps / 10 000, rounded half-up to a whole base unit (LEDGER.md §6.2).
pub fn fee_half_up(amount: Decimal, bps: i32) -> Decimal {
    if bps <= 0 {
        return Decimal::ZERO;
    }
    (amount * Decimal::from(bps) / Decimal::from(10_000))
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
}