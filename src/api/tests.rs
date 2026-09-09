use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::AppState;
use crate::tokens::TokenSummary;
use chrono::{DateTime, Utc};

use serde_json::Value as JsonValue;
use std::collections::HashMap;
// ==========================================
// 1. TOKENS ENDPOINT
// ==========================================

/// GET /api/test/tokens
/// Returns all metadata for currently registered token handlers in the system.
// pub async fn list_tokens_test_handler(
//     State(state): State<AppState>,
// ) -> Json<Vec<TokenMetadata>> {
//     let tokens = state.registry.get_metadata();
//     Json(tokens)
// }

pub async fn list_tokens_test_handler(State(state): State<AppState>) -> Json<Vec<TokenSummary>> {
    Json(
        state
            .registry
            .summaries()
            .into_iter()
            .filter(|s| s.capabilities.invoice)
            .collect(),
    )
}

// ==========================================
// 2. NETWORKS ENDPOINT
// ==========================================

#[derive(Serialize)]
pub struct NetworkSummaryResponse {
    pub evm_chain_ids: Vec<u64>,
    pub solana_clusters: Vec<String>,
    pub bitcoin_networks: Vec<String>,
}

/// GET /api/test/networks
/// Returns active blockchain network instances registered from environment configuration.
pub async fn list_networks_test_handler(
    State(state): State<AppState>,
) -> Json<NetworkSummaryResponse> {
    let evm_chain_ids = state.networks.evm.keys().copied().collect();
    let solana_clusters = state
        .networks
        .sol
        .keys()
        .map(|cluster| format!("{:?}", cluster))
        .collect();
    let bitcoin_networks = state
        .networks
        .esplora
        .keys()
        .map(|net| format!("{:?}", net))
        .collect();

    Json(NetworkSummaryResponse {
        evm_chain_ids,
        solana_clusters,
        bitcoin_networks,
    })
}

// ==========================================
// 3. MERCHANTS ENDPOINT
// ==========================================

#[derive(Serialize)]
pub struct MerchantSummaryResponse {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub api_key_id: String,
    pub webhook_url: Option<String>,
}

/// GET /api/test/merchants
/// Fetches registered merchant accounts from the database for quick inspection.
pub async fn list_merchants_test_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<MerchantSummaryResponse>>, (StatusCode, String)> {
    let merchants = sqlx::query_as!(
        MerchantSummaryResponse,
        r#"
        SELECT id, name, slug, api_key_id, webhook_url
        FROM merchants
        ORDER BY id DESC
        "#
    )
        .fetch_all(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to list merchants: {e}")))?;

    Ok(Json(merchants))
}









#[derive(Debug, Deserialize)]
pub struct LedgerQuery {
    pub merchant_id: Option<Uuid>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct LedgerOverviewResponse {
    pub generated_at: DateTime<Utc>,
    pub merchant_id: Option<Uuid>,
    pub positions: Vec<PositionRow>,
    pub accounts: Vec<AccountBalanceRow>,
    pub journals: Vec<JournalRow>,
    pub sweep_backlog: Vec<SweepBacklogRow>,
    pub reconciliation: Vec<ReconciliationRow>,
}

#[derive(Serialize)]
pub struct PositionRow {
    pub merchant_id: Uuid,
    pub asset_id: Uuid,
    pub network_type: String,
    pub chain_ref: String,
    pub symbol: Option<String>,
    pub decimals: i32,
    pub asset_registered: bool,
    pub unswept: String,
    pub treasury: String,
    pub gas: String,
    pub unsupported: String,
    pub owed_to_merchant: String,
    pub fees_owed_by_merchant: String,
    pub gas_advanced: String,
    pub unexplained: String,
}

#[derive(Serialize)]
pub struct AccountBalanceRow {
    pub account_id: Uuid,
    pub merchant_id: Uuid,
    pub kind: String,
    pub asset_id: Uuid,
    pub network_type: String,
    pub chain_ref: String,
    pub asset_kind: String,
    pub asset_address: Option<String>,
    pub symbol: Option<String>,
    pub decimals: i32,
    pub asset_registered: bool,
    pub balance: String,
    pub entry_count: i64,
    pub last_activity_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct JournalRow {
    pub id: Uuid,
    pub kind: String,
    pub dedupe_key: String,
    pub merchant_id: Uuid,
    pub tx_id: Option<Uuid>,
    pub payment_id: Option<Uuid>,
    pub reverses: Option<Uuid>,
    pub metadata: JsonValue,
    pub occurred_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub tx_hash: Option<String>,
    pub network_type: Option<String>,
    pub chain_ref: Option<String>,
    pub entries: Vec<EntryRow>,
}

#[derive(Serialize)]
pub struct EntryRow {
    #[serde(skip_serializing)]
    pub journal_id: Uuid,
    pub entry_no: i64,
    pub account_id: Uuid,
    pub account_kind: String,
    pub asset_id: Uuid,
    pub symbol: Option<String>,
    pub decimals: i32,
    pub network_type: String,
    pub chain_ref: String,
    pub amount: String,
}

#[derive(Serialize)]
pub struct SweepBacklogRow {
    pub merchant_id: Uuid,
    pub network_type: String,
    pub chain_ref: String,
    pub asset_id: Uuid,
    pub symbol: Option<String>,
    pub decimals: i32,
    pub custody_address: String,
    pub custody_kind: String,
    pub authority_address: Option<String>,
    pub movement_count: i64,
    pub total_amount: String,
    pub oldest_enqueued_at: Option<DateTime<Utc>>,
    pub next_available_at: Option<DateTime<Utc>>,
    pub max_attempts: i32,
}

#[derive(Serialize)]
pub struct ReconciliationRow {
    pub merchant_id: Uuid,
    pub asset_id: Uuid,
    pub symbol: Option<String>,
    pub decimals: i32,
    pub network_type: String,
    pub chain_ref: String,
    pub ledger_unswept: String,
    pub queue_active: String,
    pub queue_abandoned: String,
    pub drift: String,
}

/// GET /api/test/ledger
/// Snapshot of accounts, journals, positions and sweep backlog for inspection.
pub async fn ledger_overview_test_handler(
    State(state): State<AppState>,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<LedgerOverviewResponse>, (StatusCode, String)> {
    let merchant_id = q.merchant_id;
    let limit = q.limit.unwrap_or(60).clamp(1, 500);

    let db = |e: sqlx::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("Ledger read failed: {e}"));

    // ---- positions -------------------------------------------------------
    let positions = sqlx::query_as!(
        PositionRow,
        r#"
        SELECT
            merchant_id            AS "merchant_id!",
            asset_id               AS "asset_id!",
            network_type           AS "network_type!",
            chain_ref              AS "chain_ref!",
            symbol                 AS "symbol?",
            decimals               AS "decimals!",
            asset_registered       AS "asset_registered!",
            unswept::text          AS "unswept!",
            treasury::text         AS "treasury!",
            gas::text              AS "gas!",
            unsupported::text      AS "unsupported!",
            owed_to_merchant::text AS "owed_to_merchant!",
            fees_owed_by_merchant::text AS "fees_owed_by_merchant!",
            gas_advanced::text     AS "gas_advanced!",
            unexplained::text      AS "unexplained!"
        FROM v_merchant_positions
        WHERE ($1::uuid IS NULL OR merchant_id = $1)
        ORDER BY network_type, chain_ref, symbol NULLS LAST
        "#,
        merchant_id
    )
        .fetch_all(&state.pool)
        .await
        .map_err(db)?;

    // ---- account balances ------------------------------------------------
    let accounts = sqlx::query_as!(
        AccountBalanceRow,
        r#"
        SELECT
            account_id       AS "account_id!",
            merchant_id      AS "merchant_id!",
            kind             AS "kind!",
            asset_id         AS "asset_id!",
            network_type     AS "network_type!",
            chain_ref        AS "chain_ref!",
            asset_kind       AS "asset_kind!",
            asset_address    AS "asset_address?",
            symbol           AS "symbol?",
            decimals         AS "decimals!",
            asset_registered AS "asset_registered!",
            balance::text    AS "balance!",
            entry_count      AS "entry_count!",
            last_activity_at AS "last_activity_at?"
        FROM v_ledger_balances
        WHERE ($1::uuid IS NULL OR merchant_id = $1)
        ORDER BY network_type, chain_ref, symbol NULLS LAST, kind
        "#,
        merchant_id
    )
        .fetch_all(&state.pool)
        .await
        .map_err(db)?;

    // ---- journals --------------------------------------------------------
    struct JournalHead {
        id: Uuid,
        kind: String,
        dedupe_key: String,
        merchant_id: Uuid,
        tx_id: Option<Uuid>,
        payment_id: Option<Uuid>,
        reverses: Option<Uuid>,
        metadata: JsonValue,
        occurred_at: DateTime<Utc>,
        created_at: DateTime<Utc>,
        tx_hash: Option<String>,
        network_type: Option<String>,
        chain_ref: Option<String>,
    }

    let heads = sqlx::query_as!(
        JournalHead,
        r#"
        SELECT
            j.id            AS "id!",
            j.kind          AS "kind!",
            j.dedupe_key    AS "dedupe_key!",
            j.merchant_id   AS "merchant_id!",
            j.tx_id         AS "tx_id?",
            j.payment_id    AS "payment_id?",
            j.reverses      AS "reverses?",
            COALESCE(j.metadata, '{}'::jsonb) AS "metadata!",
            j.occurred_at   AS "occurred_at!",
            j.created_at    AS "created_at!",
            t.tx_hash       AS "tx_hash?",
            t.network_type  AS "network_type?",
            t.chain_ref     AS "chain_ref?"
        FROM ledger_journals j
        LEFT JOIN chain_transactions t ON t.id = j.tx_id
        WHERE ($1::uuid IS NULL OR j.merchant_id = $1)
        ORDER BY j.occurred_at DESC, j.created_at DESC
        LIMIT $2
        "#,
        merchant_id,
        limit
    )
        .fetch_all(&state.pool)
        .await
        .map_err(db)?;

    let journal_ids: Vec<Uuid> = heads.iter().map(|h| h.id).collect();

    let entry_rows = sqlx::query_as!(
        EntryRow,
        r#"
        SELECT
            e.entry_no    AS "entry_no!",
            e.journal_id  AS "journal_id!",
            e.account_id  AS "account_id!",
            a.kind        AS "account_kind!",
            e.asset_id    AS "asset_id!",
            ast.symbol    AS "symbol?",
            ast.decimals  AS "decimals!",
            ast.network_type AS "network_type!",
            ast.chain_ref AS "chain_ref!",
            e.amount::text AS "amount!"
        FROM ledger_entries e
        JOIN ledger_accounts a ON a.id = e.account_id
        JOIN assets ast        ON ast.id = e.asset_id
        WHERE e.journal_id = ANY($1)
        ORDER BY e.journal_id, e.entry_no
        "#,
        &journal_ids
    )
        .fetch_all(&state.pool)
        .await
        .map_err(db)?;

    let mut by_journal: HashMap<Uuid, Vec<EntryRow>> = HashMap::new();
    for row in entry_rows {
        by_journal.entry(row.journal_id).or_default().push(row);
    }

    let journals = heads
        .into_iter()
        .map(|h| JournalRow {
            entries: by_journal.remove(&h.id).unwrap_or_default(),
            id: h.id,
            kind: h.kind,
            dedupe_key: h.dedupe_key,
            merchant_id: h.merchant_id,
            tx_id: h.tx_id,
            payment_id: h.payment_id,
            reverses: h.reverses,
            metadata: h.metadata,
            occurred_at: h.occurred_at,
            created_at: h.created_at,
            tx_hash: h.tx_hash,
            network_type: h.network_type,
            chain_ref: h.chain_ref,
        })
        .collect();

    // ---- sweep backlog ---------------------------------------------------
    let sweep_backlog = sqlx::query_as!(
        SweepBacklogRow,
        r#"
        SELECT
            merchant_id        AS "merchant_id!",
            network_type       AS "network_type!",
            chain_ref          AS "chain_ref!",
            asset_id           AS "asset_id!",
            symbol             AS "symbol?",
            decimals           AS "decimals!",
            custody_address    AS "custody_address!",
            custody_kind       AS "custody_kind!",
            authority_address  AS "authority_address?",
            movement_count     AS "movement_count!",
            total_amount::text AS "total_amount!",
            oldest_enqueued_at AS "oldest_enqueued_at?",
            next_available_at  AS "next_available_at?",
            max_attempts       AS "max_attempts!"
        FROM v_sweep_backlog
        WHERE ($1::uuid IS NULL OR merchant_id = $1)
        ORDER BY oldest_enqueued_at
        "#,
        merchant_id
    )
        .fetch_all(&state.pool)
        .await
        .map_err(db)?;

    // ---- reconciliation --------------------------------------------------
    let reconciliation = sqlx::query_as!(
        ReconciliationRow,
        r#"
        SELECT
            r.merchant_id          AS "merchant_id!",
            r.asset_id             AS "asset_id!",
            r.symbol               AS "symbol?",
            a.decimals             AS "decimals!",
            r.network_type         AS "network_type!",
            r.chain_ref            AS "chain_ref!",
            r.ledger_unswept::text AS "ledger_unswept!",
            r.queue_active::text   AS "queue_active!",
            r.queue_abandoned::text AS "queue_abandoned!",
            r.drift::text          AS "drift!"
        FROM v_unswept_reconciliation r
        JOIN assets a ON a.id = r.asset_id
        WHERE ($1::uuid IS NULL OR r.merchant_id = $1)
        ORDER BY r.network_type, r.chain_ref, r.symbol NULLS LAST
        "#,
        merchant_id
    )
        .fetch_all(&state.pool)
        .await
        .map_err(db)?;

    Ok(Json(LedgerOverviewResponse {
        generated_at: Utc::now(),
        merchant_id,
        positions,
        accounts,
        journals,
        sweep_backlog,
        reconciliation,
    }))
}