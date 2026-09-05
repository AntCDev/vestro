use serde::{Deserialize, Serialize};
use uuid::Uuid;
use rust_decimal::Decimal;
use crate::AppState;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Redirect,
    Json,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use crate::tokens::{CheckoutContext, PresignContext, StatusContext};

#[derive(Deserialize)]
pub struct CreateInvoiceRequest {
    pub merchant_id: Uuid,
    pub token_id: String,
    pub amount_requested: Decimal,
    pub data: Option<String>,
}

#[derive(Serialize)]
pub struct CreateInvoiceResponse {
    pub url: String,
    pub invoice_id: Uuid,
}

/// POST /api/invoices
/// Accepts payload and delegates execution to the orchestrator layer
pub async fn create_invoice_handler(
    State(state): State<AppState>,
    Json(payload): Json<CreateInvoiceRequest>,
) -> Result<Json<CreateInvoiceResponse>, (StatusCode, String)> {

    let invoice_id = state
        .orchestrator
        .create_invoice(
            payload.merchant_id,
            &payload.token_id,
            payload.amount_requested,
            payload.data,
        )
        .await
        .map_err(|err_msg| (StatusCode::INTERNAL_SERVER_ERROR, err_msg))?;

    let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| {
        let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());

        let display_host = if host == "0.0.0.0" { "localhost" } else { &host };
        format!("http://{}:{}", display_host, port)
    });

    let invoice_url = format!("{}/invoice?id={}", base_url, invoice_id);

    Ok(Json(CreateInvoiceResponse {
        url: invoice_url,
        invoice_id,
    }))
}


#[derive(Deserialize)]
pub struct InvoiceRedirectQuery {
    pub id: Uuid,
}

const FALLBACK_VIEW_PATH: &str = "/checkout/generic.html";

async fn resolve_view_path(pool: &PgPool, invoice_id: Uuid) -> Option<String> {
    sqlx::query_as::<_, (String,)>(
        r#"
        SELECT cv.path
        FROM invoices i
        JOIN token_checkout_views tcv ON tcv.token_id = i.token_id
        JOIN checkout_views cv        ON cv.id = tcv.view_id
        WHERE i.id = $1
        "#,
    )
        .bind(invoice_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|(path,)| path)
}

/// GET /invoice?id=<invoice_id>
pub async fn invoice_redirect_handler(
    State(state): State<AppState>,
    Query(query): Query<InvoiceRedirectQuery>,
) -> Redirect {
    let path = resolve_view_path(&state.pool, query.id)
        .await
        .unwrap_or_else(|| FALLBACK_VIEW_PATH.to_string());

    let sep = if path.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{}{}id={}", path, sep, query.id))
}
#[derive(sqlx::FromRow)]
struct StatusInvoiceRow {
    token_id: String,
    wallet_address: String,
    payment_reference: Option<String>,
    status: String,
    amount_requested: Decimal,
    amount_received: Decimal,
    required_confirmations: Option<i16>,
    expires_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct PaymentRow {
    tx_hash: String,
    amount: Decimal,
    confirmations: i32,
    status: String,
    payment_path: Option<String>,
}

#[derive(Serialize)]
pub struct PaymentSummary {
    pub tx_hash: String,
    pub amount: String,
    pub confirmations: i32,
    pub status: String,
    pub payment_path: Option<String>,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub status: String,
    pub amount_requested: String,
    pub amount_received: String,
    pub required_confirmations: Option<i16>,
    pub expires_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub payments: Vec<PaymentSummary>,
    /// Handler-defined extras. `null` unless the handler overrides status_data.
    pub data: Value,
}
#[derive(sqlx::FromRow)]
struct CheckoutInvoiceRow {
    merchant_id: Uuid,
    token_id: String,
    token_address: Option<String>,
    token_program: Option<String>,
    token_decimals: Option<i16>,
    amount_requested: Decimal,
    amount_received: Decimal,
    wallet_address: String,
    payment_reference: Option<String>,
    status: String,
    required_confirmations: Option<i16>,
    network_type: Option<String>,
    chain_ref: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    data: Option<String>,
}

#[derive(Serialize)]
pub struct CheckoutResponse {
    pub invoice: CheckoutInvoice,
    pub view: CheckoutViewInfo,
    /// Opaque, handler-defined. The assigned view file is expected to know
    /// this shape; the API makes no guarantees about it.
    pub data: Value,
    pub token: CheckoutToken,
}

#[derive(Serialize)]
pub struct CheckoutInvoice {
    pub id: Uuid,
    pub merchant_id: Uuid,
    pub token_id: String,
    pub token_name: String,
    pub token_detail: String,
    pub token_decimals: Option<i16>,
    /// base units, as a string — 78-digit NUMERIC will not survive a f64
    pub amount_requested: String,
    pub amount_received: String,
    pub wallet_address: String,
    pub payment_reference: Option<String>,
    pub status: String,
    pub required_confirmations: Option<i16>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct CheckoutViewInfo {
    pub id: String,
    pub path: String,
}

/// GET /api/invoices/:id/checkout   (called once, on page load)
/// Extra advertised facts the checkout page can now use directly instead of
/// sniffing them out of the handler-defined `data` blob: badge a testnet,
/// show a "work in progress" ribbon, pick an explorer by network+chain.
#[derive(Serialize)]
pub struct CheckoutToken {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub network: String,
    pub chain: String,
    pub testnet: bool,
    pub status: String,
}

pub async fn get_invoice_checkout_handler(
    State(state): State<AppState>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<CheckoutResponse>, (StatusCode, String)> {
    let inv = sqlx::query_as::<_, CheckoutInvoiceRow>(
        r#"
        SELECT merchant_id, token_id, token_address, token_program, token_decimals,
               amount_requested, amount_received, wallet_address, payment_reference,
               status, required_confirmations, network_type, chain_ref,
               created_at, expires_at, data
        FROM invoices
        WHERE id = $1
        "#,
    )
        .bind(invoice_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "invoice not found".to_string()))?;

    let handler = state.registry.get_handler(&inv.token_id).ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("no handler registered for token {}", inv.token_id),
    ))?;

    // CHANGED: the invoice exists, so this token could invoice when the row was
    // created. Losing the capability since then is an operator/deploy problem,
    // not a payer problem — but there is genuinely no page to render, so say so
    // plainly rather than serving a checkout that cannot be completed.
    let invoicer = handler.invoicer().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!(
            "token {} no longer supports invoicing; invoice {} cannot be displayed",
            inv.token_id, invoice_id
        ),
    ))?;

    // DB is authoritative; fall back to the compiled default if the mapping row
    // is missing (token registered after the last sync, etc).
    let view = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT cv.id, cv.path
        FROM token_checkout_views tcv
        JOIN checkout_views cv ON cv.id = tcv.view_id
        WHERE tcv.token_id = $1
        "#,
    )
        .bind(&inv.token_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(|(id, path)| CheckoutViewInfo { id, path })
        .unwrap_or_else(|| {
            // CHANGED: checkout_view() moved to Invoicer.
            let v = invoicer.checkout_view();
            CheckoutViewInfo {
                id: v.id.to_string(),
                path: v.path.to_string(),
            }
        });

    let ctx = CheckoutContext {
        invoice_id,
        merchant_id: inv.merchant_id,
        token_id: inv.token_id.clone(),
        token_address: inv.token_address.clone(),
        token_program: inv.token_program.clone(),
        token_decimals: inv.token_decimals,
        amount_requested: inv.amount_requested,
        amount_received: inv.amount_received,
        wallet_address: inv.wallet_address.clone(),
        payment_reference: inv.payment_reference.clone(),
        status: inv.status.clone(),
        required_confirmations: inv.required_confirmations,
        network_type: inv.network_type.clone(),
        chain_ref: inv.chain_ref.clone(),
        created_at: inv.created_at,
        expires_at: inv.expires_at,
        data: inv.data.clone(),
    };

    // CHANGED: checkout_data() moved to Invoicer.
    let data = invoicer
        .checkout_data(&state.pool, &ctx)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // CHANGED: was a linear scan of get_metadata() to find this token's name
    // and detail. We already hold the handler, so ask it directly.
    let d = handler.descriptor();
    let token = CheckoutToken {
        id: d.id.clone(),
        name: d.name.clone(),
        detail: d.detail.clone(),
        network: d.network.clone(),
        chain: d.chain.clone(),
        testnet: d.testnet,
        status: d.status.as_str().to_string(),
    };

    Ok(Json(CheckoutResponse {
        invoice: CheckoutInvoice {
            id: invoice_id,
            merchant_id: inv.merchant_id,
            token_id: inv.token_id,
            token_name: token.name.clone(),
            token_detail: token.detail.clone(),
            token_decimals: inv.token_decimals,
            amount_requested: inv.amount_requested.to_string(),
            amount_received: inv.amount_received.to_string(),
            wallet_address: inv.wallet_address,
            payment_reference: inv.payment_reference,
            status: inv.status,
            required_confirmations: inv.required_confirmations,
            created_at: inv.created_at,
            expires_at: inv.expires_at,
        },
        // NEW field on CheckoutResponse. token_name / token_detail stay where
        // they are so the existing checkout pages keep working.
        token,
        view,
        data,
    }))
}

/// GET /api/invoices/:id/status   (polled)
pub async fn get_invoice_status_handler(
    State(state): State<AppState>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let inv = sqlx::query_as::<_, StatusInvoiceRow>(
        r#"
        SELECT token_id, wallet_address, payment_reference, status,
               amount_requested, amount_received, required_confirmations,
               expires_at, updated_at
        FROM invoices
        WHERE id = $1
        "#,
    )
        .bind(invoice_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "invoice not found".to_string()))?;

    let payments = sqlx::query_as::<_, PaymentRow>(
        r#"
        SELECT tx_hash, amount, confirmations, status, payment_path
        FROM payments
        WHERE invoice_id = $1
        ORDER BY created_at ASC
        "#,
    )
        .bind(invoice_id)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // A failing status hook must not break polling — the generic half of this
    // response is what actually drives the "paid" transition in the UI.
    let data = match state.registry.get_handler(&inv.token_id) {
        Some(handler) => match handler.invoicer() {
            Some(invoicer) => {
                let ctx = StatusContext {
                    invoice_id,
                    token_id: inv.token_id.clone(),
                    wallet_address: inv.wallet_address.clone(),
                    payment_reference: inv.payment_reference.clone(),
                    status: inv.status.clone(),
                    amount_requested: inv.amount_requested,
                    amount_received: inv.amount_received,
                    expires_at: inv.expires_at,
                };
                invoicer
                    .status_data(&state.pool, &ctx)
                    .await
                    .unwrap_or_else(|e| {
                        eprintln!("⚠️  status_data failed for {}: {}", inv.token_id, e);
                        Value::Null
                    })
            }
            None => Value::Null,
        },
        None => Value::Null,
    };

    Ok(Json(StatusResponse {
        status: inv.status,
        amount_requested: inv.amount_requested.to_string(),
        amount_received: inv.amount_received.to_string(),
        required_confirmations: inv.required_confirmations,
        expires_at: inv.expires_at,
        updated_at: inv.updated_at,
        payments: payments
            .into_iter()
            .map(|p| PaymentSummary {
                tx_hash: p.tx_hash,
                amount: p.amount.to_string(),
                confirmations: p.confirmations,
                status: p.status,
                payment_path: p.payment_path,
            })
            .collect(),
        data,
    }))
}

/// GET /api/invoices/:id/solana/blockhash
///
/// Read-only. Returns chain state so the browser can finish a transaction it
/// built itself. No key material, no signing, no relay.
#[derive(sqlx::FromRow)]
struct PresignInvoiceRow {
    token_id: String,
    status: String,
    expires_at: DateTime<Utc>,
}
const CLOSED: &[&str] = &["paid", "confirmed", "expired", "cancelled", "refunded"];

pub async fn get_invoice_blockhash_handler(
    State(state): State<AppState>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let inv = sqlx::query_as::<_, PresignInvoiceRow>(
        r#"SELECT token_id, status, expires_at FROM invoices WHERE id = $1"#,
    )
        .bind(invoice_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "invoice not found".to_string()))?;

    if CLOSED.contains(&inv.status.as_str()) {
        return Err((StatusCode::CONFLICT, format!("invoice is {}", inv.status)));
    }
    if inv.expires_at <= Utc::now() {
        return Err((StatusCode::CONFLICT, "invoice expired".to_string()));
    }

    let handler = state.registry.get_handler(&inv.token_id).ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("no handler registered for token {}", inv.token_id),
    ))?;

    // CHANGED. "No invoicer" and "invoicer with no pre-sign step" are different
    // failures — one is a misconfigured deployment, the other is a normal fact
    // about the token — so they get different codes.
    let invoicer = handler.invoicer().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("token {} no longer supports invoicing", inv.token_id),
    ))?;

    let ctx = PresignContext {
        invoice_id,
        token_id: inv.token_id.clone(),
        status: inv.status,
        expires_at: inv.expires_at,
    };

    let data = invoicer
        .presign_data(&state.pool, &ctx)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    if data.is_null() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("token {} has no pre-sign step", inv.token_id),
        ));
    }

    Ok(Json(data))
}