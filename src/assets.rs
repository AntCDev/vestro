//! Asset identity.
//!
//! An asset is a fact about a chain: (network_type, chain_ref, kind, address).
//! It is *not* owned by a handler — several handlers may advertise the same
//! asset, and the ledger is denominated in assets, never in token IDs.
//!
//! This module lives at the crate root rather than under `tokens/` on purpose:
//! the ledger and the (future) sweep planner both need `AssetKey` and neither
//! should have to depend on the token registry to get it.

use std::fmt;

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Canonical `network_type` values. These strings are load-bearing: they are
/// what `invoices.network_type`, `merchant_wallets.network_type` and
/// `assets.network_type` all agree on. Adding Tron means adding a constant
/// here and a branch in `canonical_address`, nothing else.
pub const NETWORK_EVM: &str = "evm";
pub const NETWORK_SOLANA: &str = "solana";
pub const NETWORK_ESPLORA: &str = "esplora";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
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

impl fmt::Display for AssetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The primary key of an asset, in the same shape as the `assets_identity`
/// constraint. Hashable so the registry can index handlers by it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct AssetKey {
    pub network_type: String,
    pub chain_ref: String,
    pub kind: AssetKind,
    /// Canonical form. NULL iff kind == Native, enforced both here and by the
    /// `assets_native_has_no_address` CHECK.
    pub address: Option<String>,
}

impl AssetKey {
    pub fn native(network_type: &str, chain_ref: &str) -> Self {
        Self {
            network_type: network_type.to_string(),
            chain_ref: chain_ref.to_string(),
            kind: AssetKind::Native,
            address: None,
        }
    }

    pub fn contract(network_type: &str, chain_ref: &str, address: &str) -> Result<Self, String> {
        Ok(Self {
            network_type: network_type.to_string(),
            chain_ref: chain_ref.to_string(),
            kind: AssetKind::Contract,
            address: Some(canonical_address(network_type, address)?),
        })
    }

    /// The one place that decides what "native" looks like in config.
    /// `None`, `""` and `"0"` all mean native; everything else is a contract.
    /// Handlers should never re-implement this test.
    pub fn from_optional_address(
        network_type: &str,
        chain_ref: &str,
        address: Option<&str>,
    ) -> Result<Self, String> {
        match address
            .map(str::trim)
            .filter(|a| !a.is_empty() && *a != "0")
        {
            None => Ok(Self::native(network_type, chain_ref)),
            Some(a) => Self::contract(network_type, chain_ref, a),
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self.kind, AssetKind::Native)
    }
}

impl fmt::Display for AssetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.address {
            None => write!(f, "{}/{}/native", self.network_type, self.chain_ref),
            Some(a) => write!(f, "{}/{}/{}", self.network_type, self.chain_ref, a),
        }
    }
}

/// Textual canonicalization, matching the CHECK constraints on `assets`.
///
/// EVM and Esplora are lowercased; Solana is base58 and case-sensitive, so it
/// gets validated instead. An unknown network is stored verbatim — better a
/// pass-through than a wrong guess when Tron lands.
///
/// NOTE on Esplora: lowercasing is only safe while its assets are native-only.
/// Base58 Bitcoin addresses *are* case-sensitive, so if an Esplora-family asset
/// ever needs a base58 identifier, this branch and the
/// `assets_esplora_lowercase` CHECK both have to be revisited.
pub fn canonical_address(network_type: &str, address: &str) -> Result<String, String> {
    let a = address.trim();
    if a.is_empty() {
        return Err("empty asset address".to_string());
    }

    match network_type {
        NETWORK_EVM => {
            let lower = a.to_ascii_lowercase();
            let ok = lower.len() == 42
                && lower.starts_with("0x")
                && lower[2..].chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(format!("not a well-formed EVM address: {a}"));
            }
            Ok(lower)
        }
        NETWORK_ESPLORA => Ok(a.to_ascii_lowercase()),
        NETWORK_SOLANA => {
            let bytes = bs58::decode(a)
                .into_vec()
                .map_err(|e| format!("solana address is not base58 ({a}): {e}"))?;
            if bytes.len() != 32 {
                return Err(format!(
                    "solana address {a} decodes to {} bytes, expected 32",
                    bytes.len()
                ));
            }
            Ok(a.to_string())
        }
        _ => Ok(a.to_string()),
    }
}

/// Everything a handler advertises about an asset. Two handlers pointing at the
/// same `key` are expected to agree on the rest; `sync_assets` warns when they
/// don't, because the last writer wins in the DB.
#[derive(Clone, Debug, Serialize)]
pub struct AssetSpec {
    pub key: AssetKey,
    pub symbol: String,
    pub decimals: u8,
    /// Chain-specific facts about the asset itself, not about any route:
    /// Solana `token_program` + `ata_program`, Tron trc-kind, and so on.
    pub params: Value,
}

impl AssetSpec {
    pub fn new(key: AssetKey, symbol: &str, decimals: u8, params: Value) -> Self {
        Self {
            key,
            symbol: symbol.to_string(),
            decimals,
            params,
        }
    }

    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.symbol != other.symbol || self.decimals != other.decimals || self.params != other.params
    }

    /// Convenience for the orchestrator, which has to put `token_program` on
    /// the invoice row. Returns None on chains that have no such concept.
    pub fn param_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(Value::as_str)
    }
}

/// Upsert an advertised asset and flag it registered. Runtime-checked query
/// (not `query!`) so this compiles without a live DB in CI.
pub async fn upsert_registered(pool: &PgPool, spec: &AssetSpec) -> Result<Uuid, String> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO assets
            (network_type, chain_ref, asset_kind, address,
             decimals, symbol, asset_params, registered, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, true, now())
        ON CONFLICT ON CONSTRAINT assets_identity DO UPDATE
        SET decimals     = EXCLUDED.decimals,
            symbol       = EXCLUDED.symbol,
            asset_params = EXCLUDED.asset_params,
            registered   = true,
            updated_at   = now()
        RETURNING id
        "#,
    )
    .bind(&spec.key.network_type)
    .bind(&spec.key.chain_ref)
    .bind(spec.key.kind.as_str())
    .bind(spec.key.address.as_deref())
    .bind(spec.decimals as i16)
    .bind(&spec.symbol)
    .bind(&spec.params)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("upsert asset {}: {e}", spec.key))
}

/// Clear `registered` on everything not in `keep`. Rows are never deleted: an
/// asset that loses its last handler keeps every ledger entry ever written
/// against it and simply stops being withdrawable.
///
/// An empty `keep` clears everything, which is the correct reading of "no
/// handler advertises anything right now".
pub async fn clear_unadvertised(pool: &PgPool, keep: &[Uuid]) -> Result<u64, String> {
    let res = sqlx::query(
        r#"
        UPDATE assets
           SET registered = false, updated_at = now()
         WHERE registered
           AND NOT (id = ANY($1))
        "#,
    )
    .bind(keep)
    .execute(pool)
    .await
    .map_err(|e| format!("clear_unadvertised: {e}"))?;

    Ok(res.rows_affected())
}
