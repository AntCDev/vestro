//! The registry: token_id -> handler, plus an asset -> handlers index.
//!
//! The asset index is the piece the ledger needs. The ledger never knows a
//! token ID; it knows it holds N of some asset and asks here who can move it.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;

use crate::assets::AssetKey;
use crate::networks::NetworkRegistry;
use crate::tokens::handler::{TokenDescriptor, TokenHandler, TokenSummary};

pub struct TokenRegistry {
    handlers: HashMap<String, Arc<dyn TokenHandler>>,
    /// Registration order, so listings are stable across restarts.
    order: Vec<String>,
    by_asset: HashMap<AssetKey, Vec<String>>,
}

impl TokenRegistry {
    pub fn new(networks: Arc<NetworkRegistry>) -> Self {
        println!("\n🪙 Registering Token Handlers...");
        let mut registry = Self {
            handlers: HashMap::new(),
            order: Vec::new(),
            by_asset: HashMap::new(),
        };

        crate::tokens::eth::register(&mut registry, networks.clone());
        crate::tokens::sepolia::register(&mut registry, networks.clone());
        crate::tokens::base::register(&mut registry, networks.clone());
        crate::tokens::base_sepolia::register(&mut registry, networks.clone());
        crate::tokens::sol_devnet::register(&mut registry, networks.clone());

        registry
    }

    pub fn descriptor(&self, id: &str) -> Option<TokenDescriptor> {
        self.handlers.get(id).map(|h| h.descriptor().clone())
    }    
    
    /// Takes only the handler — the descriptor comes off it, so there is no
    /// way for the registered metadata and the handler's own view of itself to
    /// drift apart.
    pub fn register<H>(&mut self, handler: H)
    where
        H: TokenHandler + 'static,
    {
        let handler: Arc<dyn TokenHandler> = Arc::new(handler);
        let d: TokenDescriptor = handler.descriptor().clone();
        let caps = handler.capabilities();

        if self.handlers.contains_key(&d.id) {
            println!("  ❌ duplicate token id {} — keeping the first registration", d.id);
            return;
        }

        println!(
            "  ✅ {:<22} {:<6} {}/{}{}  [{}]  {}",
            d.id,
            d.name,
            d.network,
            d.chain,
            if d.testnet { " (testnet)" } else { "" },
            caps.badge(),
            d.asset.key,
        );

        self.by_asset
            .entry(d.asset.key.clone())
            .or_default()
            .push(d.id.clone());
        self.order.push(d.id.clone());
        self.handlers.insert(d.id, handler);
    }

    pub fn get_handler(&self, id: &str) -> Option<Arc<dyn TokenHandler>> {
        self.handlers.get(id).cloned()
    }

    /// In registration order.
    pub fn all(&self) -> Vec<Arc<dyn TokenHandler>> {
        self.order
            .iter()
            .filter_map(|id| self.handlers.get(id).cloned())
            .collect()
    }

    pub fn descriptors(&self) -> Vec<TokenDescriptor> {
        self.all().iter().map(|h| h.descriptor().clone()).collect()
    }

    /// What the frontend renders. Group by `network` / `chain`, filter on
    /// `testnet`, badge on `capabilities` and `status`.
    pub fn summaries(&self) -> Vec<TokenSummary> {
        self.all()
            .iter()
            .map(|h| TokenSummary::from_handler(h.as_ref()))
            .collect()
    }

    /// Kept for call sites that predate the rename.
    pub fn get_metadata(&self) -> Vec<TokenSummary> {
        self.summaries()
    }

    /// Every handler advertising this exact asset, regardless of capability.
    pub fn for_asset(&self, key: &AssetKey) -> Vec<Arc<dyn TokenHandler>> {
        self.by_asset
            .get(key)
            .map(|ids| ids.iter().filter_map(|id| self.handlers.get(id).cloned()).collect())
            .unwrap_or_default()
    }

    /// The subset of those that can actually move it. Zero => the asset is
    /// visible in the ledger but not withdrawable. One => sweep automatically.
    /// More than one => the operator (or the merchant's "sweep now" dialog)
    /// picks which handler to use.
    pub fn sweepers_for_asset(&self, key: &AssetKey) -> Vec<Arc<dyn TokenHandler>> {
        self.for_asset(key)
            .into_iter()
            .filter(|h| h.sweeper().is_some())
            .collect()
    }

    pub fn invoiceable(&self) -> Vec<Arc<dyn TokenHandler>> {
        self.all()
            .into_iter()
            .filter(|h| h.invoicer().is_some())
            .collect()
    }

    /// Seeds the view catalogue and the token->view mapping.
    /// Idempotent: existing rows are never overwritten, so an operator who
    /// repoints a token in the DB keeps that choice across restarts.
    ///
    /// Only invoice-capable handlers get a mapping — a token with no invoicer
    /// has no checkout page to point at.
    pub async fn sync_checkout_views(&self, pool: &PgPool) -> Result<(), sqlx::Error> {
        println!("\n🖼️  Syncing checkout views...");
        let mut tx = pool.begin().await?;

        for handler in self.all() {
            let Some(invoicer) = handler.invoicer() else {
                continue;
            };
            let token_id = handler.token_id().to_string();
            let view = invoicer.checkout_view();

            sqlx::query(
                r#"
                INSERT INTO checkout_views (id, path, description)
                VALUES ($1, $2, $3)
                ON CONFLICT (id) DO NOTHING
                "#,
            )
            .bind(view.id)
            .bind(view.path)
            .bind(view.description)
            .execute(&mut *tx)
            .await?;

            let inserted = sqlx::query(
                r#"
                INSERT INTO token_checkout_views (token_id, view_id)
                VALUES ($1, $2)
                ON CONFLICT (token_id) DO NOTHING
                "#,
            )
            .bind(&token_id)
            .bind(view.id)
            .execute(&mut *tx)
            .await?;

            if inserted.rows_affected() == 1 {
                println!("  ✅ {} -> {} ({})", token_id, view.id, view.path);
            }
        }

        tx.commit().await?;

        // Surface tokens whose DB mapping diverges from the code default —
        // intentional after an operator edit, but worth seeing in the log.
        let overrides = sqlx::query_as::<_, (String, String)>(
            r#"SELECT token_id, view_id FROM token_checkout_views ORDER BY token_id"#,
        )
        .fetch_all(pool)
        .await?;

        for (token_id, view_id) in overrides {
            match self.handlers.get(&token_id) {
                Some(h) => match h.invoicer() {
                    Some(inv) if inv.checkout_view().id != view_id => {
                        println!("  ⚙️  {} overridden -> {}", token_id, view_id);
                    }
                    Some(_) => {}
                    None => println!(
                        "  ⚠️  {} mapped to {} but its handler cannot invoice",
                        token_id, view_id
                    ),
                },
                None => println!(
                    "  ⚠️  {} mapped to {} but no handler registered",
                    token_id, view_id
                ),
            }
        }

        Ok(())
    }
}
