use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::assets::{self, AssetKey, AssetSpec};
use crate::tokens::handler::TokenHandler;
use crate::tokens::TokenRegistry;

pub struct PaymentOrchestrator {
    pool: PgPool,
    registry: Arc<TokenRegistry>,
}

impl PaymentOrchestrator {
    pub fn new(pool: PgPool, registry: Arc<TokenRegistry>) -> Self {
        Self { pool, registry }
    }

    // ---------------------------------------------------------------------
    // Boot-time: publish what the handlers advertise.
    // ---------------------------------------------------------------------

    /// Walk the registry, upsert every advertised asset, and clear `registered`
    /// on anything that lost its last handler. Code is authoritative here — the
    /// DB row is a projection of what is compiled in, so it is overwritten on
    /// every boot. (Contrast with `sync_checkout_views`, where the operator's DB
    /// edit wins.)
    ///
    /// Run this once at startup, after the registry is built and before the
    /// network clients spin up.
    pub async fn sync_assets(&self) -> Result<(), String> {
        println!("\n🧾 Syncing advertised assets...");

        let descriptors = self.registry.descriptors();
        let mut first_writer: HashMap<AssetKey, (String, AssetSpec)> = HashMap::new();
        let mut registered_ids: Vec<Uuid> = Vec::new();

        for d in &descriptors {
            // Two handlers on the same asset is expected and fine — that is the
            // whole point of the asset table. Two handlers *disagreeing* about
            // the asset is a bug, and silently last-write-wins would corrupt
            // every balance denominated in it.
            match first_writer.get(&d.asset.key) {
                Some((first_id, first_spec)) if first_spec.conflicts_with(&d.asset) => {
                    eprintln!(
                        "  ⚠️  {first_id} and {} advertise {} with different facts \
                         ({} @ {}dp vs {} @ {}dp) — last writer wins, fix the config",
                        d.id,
                        d.asset.key,
                        first_spec.symbol,
                        first_spec.decimals,
                        d.asset.symbol,
                        d.asset.decimals,
                    );
                }
                Some(_) => {}
                None => {
                    first_writer.insert(d.asset.key.clone(), (d.id.clone(), d.asset.clone()));
                }
            }

            let id = assets::upsert_registered(&self.pool, &d.asset).await?;
            if !registered_ids.contains(&id) {
                registered_ids.push(id);
                println!("  ✅ {:<22} -> {}", d.id, d.asset.key);
            } else {
                println!("  ↳  {:<22} -> {} (shared)", d.id, d.asset.key);
            }
        }

        let cleared = assets::clear_unadvertised(&self.pool, &registered_ids).await?;
        if cleared > 0 {
            println!(
                "  ⚪ {cleared} asset(s) no longer advertised by any handler — marked \
                 unregistered (rows kept, balances frozen)"
            );
        }

        Ok(())
    }

    // ---------------------------------------------------------------------
    // Invoicing
    // ---------------------------------------------------------------------

    pub async fn create_invoice(
        &self,
        merchant_id: Uuid,
        token_id: &str,
        amount_requested: Decimal,
        data: Option<String>,
    ) -> Result<Uuid, String> {
        // 1. Resolve the handler first — no point inserting a row for a token
        //    nobody can service.
        let handler = self
            .registry
            .get_handler(token_id)
            .ok_or_else(|| format!("No handler registered for token {token_id}"))?;

        let invoicer = handler.invoicer().ok_or_else(|| {
            format!("Token {token_id} advertises itself but cannot create invoices")
        })?;

        let d = handler.descriptor();

        // 2. Insert the skeleton, pre-filled from the advertised asset.
        //
        //    These five columns used to be copy-pasted into every handler's
        //    create_invoice_payment, and base_sepolia simply forgot two of them.
        //    They are facts about the asset, which the descriptor already holds,
        //    so the orchestrator writes them and the handler never touches them.
        //
        //    `token_program` is Solana-shaped but the column already exists on
        //    `invoices`; pulling it out of asset_params by name keeps this
        //    generic — a chain without the concept just yields NULL.
        let default_expiration = Utc::now() + Duration::hours(1);
        let token_address = d.asset.key.address.as_deref();
        let token_program = d.asset.param_str("token_program");
        let token_decimals = d.asset.decimals as i16;

        let row = sqlx::query!(
            r#"
            INSERT INTO invoices (
                merchant_id, token_id, amount_requested,
                wallet_address, wallet_index, expires_at, status, data,
                network_type, chain_ref,
                token_address, token_program, token_decimals
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING id
            "#,
            merchant_id,
            token_id,
            amount_requested,
            "", // placeholder — set by the handler, and what makes the row watched
            0,  // placeholder
            default_expiration,
            "pending",
            data,
            d.network,
            d.chain,
            token_address,
            token_program,
            token_decimals,
        )
            .fetch_one(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        let invoice_id = row.id;

        // 3. Hand off. token_id is passed through so one handler can serve
        //    several tokens (a generic "BASE handler").
        let details = invoicer
            .create_invoice_payment(&self.pool, merchant_id, invoice_id, amount_requested, token_id)
            .await?;

        println!("Invoice provisioning complete: {details:?}");
        Ok(invoice_id)
    }

    // ---------------------------------------------------------------------
    // Sweeping (resolution only — no Sweeper impls exist yet)
    // ---------------------------------------------------------------------

    /// Who can move this asset. The ledger holds an `AssetKey`, never a token
    /// ID, so this is the whole bridge between the two.
    ///
    ///   0 handlers -> the asset is visible and credited, but not withdrawable.
    ///   1 handler  -> sweep automatically.
    ///   2+         -> operator config, or the merchant's "sweep now" dialog,
    ///                 picks. Return them all and let the caller decide.
    pub fn sweep_candidates(&self, asset: &AssetKey) -> Vec<Arc<dyn TokenHandler>> {
        self.registry.sweepers_for_asset(asset)
    }
}