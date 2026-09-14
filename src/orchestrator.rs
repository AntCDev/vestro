use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::assets::{self, AssetKey, AssetSpec};
use crate::tokens::handler::TokenHandler;
use crate::tokens::TokenRegistry;

use crate::keys::derivation::purpose;
use crate::keys::wallets::wallet_address;
use crate::ledgerer::{AddressKind, AssetKind, ChainRef};
use crate::networks::transfers::{SignerRef, TransferAmount};
use crate::tokens::sweeper::SweepDraft;

/// A group of pending sweep rows that would become one transfer.
#[derive(Debug, serde::Serialize)]
pub struct SweepGroup {
    pub merchant_id: Uuid,
    pub network_type: String,
    pub chain_ref: String,
    pub custody_address: String,
    pub asset_id: Uuid,
    pub total: Decimal,
    pub rows: i64,
    pub oldest: chrono::DateTime<Utc>,
}

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


    /// For the test API and, later, the policy engine: what could be swept.
    /// Grouping key = (merchant, network, chain, custody address, asset).
    pub async fn sweepable_groups(&self) -> Result<Vec<SweepGroup>, String> {
        let rows = sqlx::query(r#"
            SELECT merchant_id, network_type, chain_ref, custody_address, asset_id,
                   SUM(amount) AS total, COUNT(*) AS rows, MIN(created_at) AS oldest
              FROM sweep_queue WHERE status='pending'
             GROUP BY 1,2,3,4,5 ORDER BY oldest"#)
            .fetch_all(&self.pool).await.map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(|r| SweepGroup {
            merchant_id: r.get("merchant_id"), network_type: r.get("network_type"),
            chain_ref: r.get("chain_ref"), custody_address: r.get("custody_address"),
            asset_id: r.get("asset_id"), total: r.get("total"), rows: r.get("rows"),
            oldest: r.get("oldest"),
        }).collect())
    }

    /// Turn one group of pending sweep rows into one outbound_transfers row.
    /// `handler_id` picks among several sweepers; None = automatic.
    pub async fn request_sweep(
        &self,
        merchant_id: Uuid,
        network_type: &str,
        chain_ref: &str,
        custody_address: &str,
        asset_id: Uuid,
        handler_id: Option<&str>,
    ) -> Result<Uuid, String> {
        // 1. Asset row -> AssetKey (the ledger's identity, not a token id).
        let a = sqlx::query("SELECT asset_kind, address, registered FROM assets WHERE id=$1")
            .bind(asset_id).fetch_optional(&self.pool).await.map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown asset {asset_id}"))?;
        if !a.get::<bool, _>("registered") {
            return Err(format!("asset {asset_id} is not registered — owed, not movable"));
        }
        let chain = ChainRef::new(network_type, chain_ref);
        let asset = match a.get::<String, _>("asset_kind").as_str() {
            "native" => AssetKey::native(chain),
            _ => AssetKey::contract_canonical(chain, a.get::<String, _>("address")),
        };

        // 2. Which handler can move it. 0 -> log+err, 1 -> it, 2+ -> pick.
        let candidates = self.registry.sweepers_for_asset(&asset);
        let handler = match (candidates.len(), handler_id) {
            (0, _) => {
                eprintln!("sweep: no sweeper registered for {asset:?}");
                return Err(format!("no handler can sweep {asset:?}"));
            }
            (_, Some(id)) => candidates.into_iter().find(|h| h.token_id() == id)
                .ok_or_else(|| format!("handler {id} cannot sweep {asset:?}"))?,
            (1, None) => candidates.into_iter().next().unwrap(),
            (_, None) => {
                // TODO: surface the choice to the operator/merchant UI instead
                // of taking the first registration.
                eprintln!("sweep: {} sweepers for {asset:?}; using {}", candidates.len(), candidates[0].token_id());
                candidates.into_iter().next().unwrap()
            }
        };
        let sweeper = handler.sweeper().expect("filtered on sweeper()");

        // 3. Lock the group and read what the Ledgerer recorded about custody.
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let rows = sqlx::query(r#"
            SELECT id, amount, custody_kind, authority_address, authority_ref, sweep_params
              FROM sweep_queue
             WHERE status='pending' AND merchant_id=$1 AND network_type=$2 AND chain_ref=$3
               AND custody_address=$4 AND asset_id=$5
             FOR UPDATE SKIP LOCKED"#)
            .bind(merchant_id).bind(network_type).bind(chain_ref).bind(custody_address).bind(asset_id)
            .fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
        if rows.is_empty() {
            return Err("nothing pending for that group".into());
        }

        let custody_kind = AddressKind::from_db(rows[0].get::<String, _>("custody_kind").as_str())
            .ok_or("bad custody_kind")?;
        let authority_address: String = rows[0].get("authority_address");
        let authority = SignerRef::parse(
            rows[0].get::<Option<String>, _>("authority_ref").as_deref()
                .ok_or("sweep row has no authority_ref (role:index) — connector must supply it")?)?;
        let queued_total: Decimal = rows.iter().map(|r| r.get::<Decimal, _>("amount")).sum();
        let mut params = serde_json::Map::new();
        for r in &rows {
            if let Value::Object(m) = r.get::<Value, _>("sweep_params") { params.extend(m); }
        }

        let destination = wallet_address(&self.pool, merchant_id, network_type, purpose::MAIN).await?;

        // 4. Draft -> handler finishes it.
        let draft = SweepDraft {
            merchant_id, asset: asset.clone(), asset_id,
            custody_address: custody_address.to_string(), custody_kind,
            authority_address, authority, destination,
            queued_total, movement_count: rows.len(),
            sweep_params: Value::Object(params),
        };
        let plan = sweeper.plan(&self.pool, &draft).await?;

        // 5. Persist the request and bind the rows to it.
        let amount_requested = match plan.amount {
            TransferAmount::Exact(n) => Some(n.to_string()),
            TransferAmount::Max => None,
        };
        let transfer_id: Uuid = sqlx::query_scalar(r#"
            INSERT INTO outbound_transfers
                (merchant_id, network_type, chain_ref, asset_id, token_id, intent,
                 from_address, from_kind, authority_role, authority_index,
                 fee_payer_role, fee_payer_index, to_address, amount_requested, params)
            VALUES ($1,$2,$3,$4,$5,'sweep',$6,$7,$8,$9,$10,$11,$12,$13::text::numeric,$14)
            RETURNING id"#)
            .bind(merchant_id).bind(network_type).bind(chain_ref).bind(asset_id).bind(handler.token_id())
            .bind(&plan.from.address).bind(plan.from.kind.as_str())
            .bind(plan.from.authority.role.as_i16()).bind(plan.from.authority.index as i32)
            .bind(plan.fee_payer.map(|f| f.role.as_i16())).bind(plan.fee_payer.map(|f| f.index as i32))
            .bind(&plan.to).bind(amount_requested).bind(&plan.params)
            .fetch_one(&mut *tx).await
            .map_err(|e| format!("insert outbound_transfers (a live transfer may already exist for this address/asset): {e}"))?;

        let ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
        sqlx::query("UPDATE sweep_queue SET status='claimed', transfer_id=$2 WHERE id = ANY($1)")
            .bind(&ids).bind(transfer_id).execute(&mut *tx).await.map_err(|e| e.to_string())?;

        tx.commit().await.map_err(|e| e.to_string())?;
        println!("sweep requested: {transfer_id} ({} rows, {queued_total} of {asset:?})", ids.len());
        Ok(transfer_id)
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