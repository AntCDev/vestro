//! HTTP routing, split by trust boundary.
//!
//! | Layer      | Prefix        | Auth                  | CSRF                        | CORS                    |
//! |------------|---------------|-----------------------|-----------------------------|-------------------------|
//! | Merchant   | /api/v1       | API key (Bearer)      | n/a (no cookies)            | none (server-to-server) |
//! | Dashboard  | /, /dashboard | cookie session        | same-origin guard + header  | none (same-origin only) |
//! | Public     | /pay, /api/checkout | none (UUID = capability) | n/a (no session)  | open, credential-less   |
//! | Demo       | /api/demo     | none                  | n/a                         | open                    |
//!
//! Each layer owns its own CORS / auth / CSRF policy, so a route's security
//! posture is decided by which router it is registered in, not per handler.

use axum::{
    extract::Request,
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::env;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    services::ServeDir,
};

use crate::api;
use crate::AppState;

/// Assemble the full application router.
pub fn build(state: AppState) -> Router {
    let mut app = Router::new()
        .nest("/api/v1", merchant_api_router(&state))
        .merge(dashboard_router(&state))
        .merge(public_router());

    if demo_mode_enabled() {
        println!("🧪 DEMO_MODE on: /api/demo/* is exposed without authentication.");
        app = app.nest("/api/demo", demo_router());
    }

    app.fallback_service(ServeDir::new("wwwroot"))
        .layer(CompressionLayer::new())
        .with_state(state)
}

// ======================================================================
// 1. MERCHANT API: API key required
// ----------------------------------------------------------------------
// Server-to-server only. Auth via `Authorization: Bearer <key_id>.<secret>`
// (or an X-Api-Key header), checked against merchants.api_key_id /
// api_key_secret_hash.
//
// - No cookies are ever read here, so there is nothing to CSRF.
// - Deliberately NO CorsLayer: browsers can't call it cross-origin, which
//   discourages merchants from shipping their secret key to the frontend.
// ======================================================================
fn merchant_api_router(_state: &AppState) -> Router<AppState> {
    Router::new()
    // Invoices
    // .route("/invoices", post(api::v1::invoices::create).get(api::v1::invoices::list))
    // .route("/invoices/{id}", get(api::v1::invoices::get))
    // .route("/invoices/{id}/cancel", post(api::v1::invoices::cancel))
    //
    // Balances / ledger (scoped to the authenticated merchant)
    // .route("/balances", get(api::v1::ledger::balances))
    // .route("/ledger/entries", get(api::v1::ledger::entries))
    //
    // Outbound transfers
    // .route("/transfers", get(api::v1::transfers::list))
    // .route("/transfers/{id}", get(api::v1::transfers::get))
    //
    // Webhook utilities
    // .route("/webhooks/test", post(api::v1::webhooks::send_test))
    //
    // Resolves the merchant from the key and inserts `AuthedMerchant` into
    // request extensions. `route_layer` so unmatched paths 404, not 401.
    // NOTE: route_layer panics on a router with no routes, so only
    // uncomment this once at least one route above is live.
    // .route_layer(middleware::from_fn_with_state(_state.clone(), auth::require_api_key))
}

// ======================================================================
// 2. DASHBOARD: same-site browser sessions (Askama + htmx)
// ----------------------------------------------------------------------
// Cookie session (tower-sessions, Postgres store):
//   HttpOnly, Secure, SameSite=Strict.
//
// CSRF, defence in depth:
//   a) `same_origin_guard`: rejects mutating requests whose
//      Sec-Fetch-Site / Origin isn't us. Active now.
//   b) Synchronizer token, sent as the `X-CSRF-Token` header. The base
//      template renders it once:
//        <body hx-headers='{"X-CSRF-Token": "{{ csrf_token }}"}'>
//      and htmx attaches it to every request it makes.
//
// Layer order matters: `.layer()` wraps outward, so the last one added
// runs first. Request flow is: same_origin_guard -> session -> csrf -> handler.
//
// No CorsLayer: same-origin only.
// ======================================================================
fn dashboard_router(_state: &AppState) -> Router<AppState> {
    // Pages that must work while logged out.
    let auth_pages = Router::new();
    // .route("/login", get(web::auth::login_page).post(web::auth::login))
    // .route("/signup", get(web::auth::signup_page).post(web::auth::signup))
    // .route("/logout", post(web::auth::logout))

    // Everything behind a login.
    let protected = Router::new();
    // .route("/dashboard", get(web::dashboard::overview))
    // .route("/dashboard/invoices", get(web::dashboard::invoices))
    // .route("/dashboard/invoices/{id}", get(web::dashboard::invoice_detail))
    // .route("/dashboard/balances", get(web::dashboard::balances))
    // .route("/dashboard/transfers", get(web::dashboard::transfers))
    //
    // Settings
    // .route("/dashboard/settings/webhook", get(web::settings::webhook).post(web::settings::update_webhook))
    // .route("/dashboard/settings/api-keys", get(web::settings::api_keys))
    // .route("/dashboard/settings/api-keys/rotate", post(web::settings::rotate_api_key))
    // .route("/dashboard/settings/checkout", get(web::settings::checkout_views).post(web::settings::update_checkout_views))
    // .route("/dashboard/settings/sweeps", get(web::settings::sweep_policy).post(web::settings::update_sweep_policy))
    //
    // Redirects to /login when there is no session user (htmx requests get
    // an `HX-Redirect` header instead of a 302, so the whole page navigates).
    // Same route_layer caveat as above: needs at least one route first.
    // .route_layer(middleware::from_fn(auth::require_login))

    Router::new()
        .merge(auth_pages)
        .merge(protected)
        // Verify X-CSRF-Token against the session on non-GET requests.
        // Needs the session, so it sits inside the session layer.
        // .layer(middleware::from_fn(csrf::verify_token))
        // .layer(session_layer)
        .layer(middleware::from_fn(same_origin_guard))
}

// ======================================================================
// 3. PUBLIC: checkout pages and the APIs they call
// ----------------------------------------------------------------------
// No auth: the invoice UUID is the capability. No session, so no CSRF.
// CORS is open but credential-less, so a merchant can embed or proxy the
// checkout from their own domain.
//
// TODO: rate limiting (tower_governor or similar). These are the only
// non-demo endpoints anyone on the internet can hit.
// ======================================================================
fn public_router() -> Router<AppState> {
    let checkout_cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE]);

    Router::new()
        // Hosted checkout page (Askama shell + the TS wallet island)
        // .route("/pay/{id}", get(web::checkout::page))

        // Existing redirect into the checkout
        .route("/invoice", get(api::invoices::invoice_redirect_handler))

        // Data the checkout page polls / needs
        .route("/api/checkout/{id}", get(api::invoices::get_invoice_checkout_handler))
        .route("/api/checkout/{id}/status", get(api::invoices::get_invoice_status_handler))
        .route(
            "/api/checkout/{id}/solana/blockhash",
            get(api::invoices::get_invoice_blockhash_handler),
        )

        // Liveness for whoever self-hosts this
        .route("/healthz", get(|| async { "ok" }))
        .layer(checkout_cors)
}

// ======================================================================
// 4. DEMO: public, unauthenticated, toggled by DEMO_MODE
// ----------------------------------------------------------------------
// Everything the current demo page uses, plus the manual probe calls.
// Unauthenticated invoice creation, merchant signup, "list every merchant"
// and "POST a sweep" are fine on a demo box and dangerous on a real one,
// hence the toggle.
//
// Mounted at /api/demo.
// ======================================================================
fn demo_router() -> Router<AppState> {
    let demo_cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // Demo flows (unauthenticated versions of the real ones)
        .route("/invoices", post(api::invoices::create_invoice_handler))
        .route("/merchants", post(api::merchants::signup_merchant_handler))

        // Probes: manual pokes to check what the system registered / holds.
        // Not automated tests; these are for a human with curl.
        .route("/probe/tokens", get(api::probe::list_tokens_test_handler))
        .route("/probe/networks", get(api::probe::list_networks_test_handler))
        .route("/probe/merchants", get(api::probe::list_merchants_test_handler))
        .route("/probe/ledger", get(api::probe::ledger_overview_test_handler))

        // Sweeps: manual triggers
        .route(
            "/sweeps",
            get(api::probe::list_sweepable_test_handler)
                .post(api::probe::sweep_address_test_handler),
        )
        .route("/transfers/{id}", get(api::probe::get_transfer_test_handler))
        .layer(demo_cors)
}

/// `DEMO_MODE=true/false` overrides. Unset defaults to on in debug builds
/// and off in release builds, so a self-hoster running `cargo build --release`
/// never exposes the demo routes by accident.
fn demo_mode_enabled() -> bool {
    match env::var("DEMO_MODE") {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => cfg!(debug_assertions),
    }
}

// ======================================================================
// Middleware
// ======================================================================

/// First CSRF layer for the dashboard: refuse state-changing requests that
/// didn't originate from our own pages.
///
/// Modern browsers send `Sec-Fetch-Site` on every request; we fall back to
/// comparing `Origin` with `Host` for older ones. Requests with neither
/// header (curl, scripts) pass here, since they carry no victim's cookies,
/// and are then stopped by the session + token checks.
///
/// Behind a reverse proxy, make sure it forwards the original `Host`,
/// otherwise the Origin fallback will reject legitimate requests.
async fn same_origin_guard(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }

    let headers = req.headers();

    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        // "none" = user typed the URL / bookmark; harmless for a POST.
        return if site == "same-origin" || site == "none" {
            next.run(req).await
        } else {
            (StatusCode::FORBIDDEN, "cross-site request blocked").into_response()
        };
    }

    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        let origin_host = origin.split("://").nth(1);
        return if host.is_some() && origin_host == host {
            next.run(req).await
        } else {
            (StatusCode::FORBIDDEN, "cross-origin request blocked").into_response()
        };
    }

    next.run(req).await
}
