//! Asset identity.
//!
//! An asset is a fact about a chain: (network_type, chain_ref, kind, address).
//! It is *not* owned by a handler — several handlers may advertise the same
//! asset, and the ledger is denominated in assets, never in token IDs.
//!
//! This module lives at the crate root rather than under `tokens/` on purpose:
//! the ledger, the Ledgerer and the (future) sweep planner all need `ChainRef`
//! and `AssetKey`, and none of them should have to depend on the token registry
//! to get them. `ledgerer.rs` used to carry its own copies of these three
//! types; they are gone, and it imports from here.

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

// ─────────────────────────────────────────────────────────────────────────────
// Chain
// ─────────────────────────────────────────────────────────────────────────────

/// Which chain. `chain_ref` must be whatever the network client's
/// `chain_ref()` returned — never a literal (LEDGER.md §1.1).
///
/// Lives here rather than in `ledgerer.rs` because `AssetKey` contains one:
/// splitting them put two spellings of the same pair in the crate.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct ChainRef {
    pub network_type: String,
    pub chain_ref: String,
}

impl ChainRef {
    pub fn new(network_type: impl Into<String>, chain_ref: impl Into<String>) -> Self {
        Self {
            network_type: network_type.into(),
            chain_ref: chain_ref.into(),
        }
    }

    pub fn is_network(&self, network_type: &str) -> bool {
        self.network_type == network_type
    }

    /// Canonicalize an address under *this* chain's rules. Sugar over
    /// `canonical_address` so callers that already hold a `ChainRef` don't
    /// have to reach into its fields.
    pub fn canonical_address(&self, address: &str) -> Result<String, String> {
        canonical_address(&self.network_type, address)
    }
}

impl From<(&str, &str)> for ChainRef {
    fn from((network_type, chain_ref): (&str, &str)) -> Self {
        Self::new(network_type, chain_ref)
    }
}

impl From<(String, String)> for ChainRef {
    fn from((network_type, chain_ref): (String, String)) -> Self {
        Self { network_type, chain_ref }
    }
}

impl fmt::Display for ChainRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network_type, self.chain_ref)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Asset identity
// ─────────────────────────────────────────────────────────────────────────────

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

    /// Parse `assets.asset_kind`. Mirrors `AddressKind::from_db` in the
    /// Ledgerer so both stringly DB columns are decoded the same way.
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "native" => Some(AssetKind::Native),
            "contract" => Some(AssetKind::Contract),
            _ => None,
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
///
/// `address` is `None` iff `kind == Native`, enforced both here and by the
/// `assets_native_has_no_address` CHECK. When `Some`, it is *already canonical*
/// — every constructor that takes an untrusted string runs it through
/// `canonical_address` first, and `contract_canonical` exists for the callers
/// (the Ledgerer, rows read back out of the DB) that are handing over a value
/// that was canonicalized upstream and must not be renormalised by a layer that
/// doesn't know the network's rules.
///
/// `chain` is `#[serde(flatten)]`ed so the JSON shape is unchanged from when
/// `network_type` and `chain_ref` were fields on this struct.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct AssetKey {
    #[serde(flatten)]
    pub chain: ChainRef,
    pub kind: AssetKind,
    pub address: Option<String>,
}

impl AssetKey {
    pub fn native(chain: impl Into<ChainRef>) -> Self {
        Self {
            chain: chain.into(),
            kind: AssetKind::Native,
            address: None,
        }
    }

    /// Canonicalizes. Use this for anything originating in config, an RPC
    /// response or an HTTP request.
    pub fn contract(chain: impl Into<ChainRef>, address: &str) -> Result<Self, String> {
        let chain = chain.into();
        let address = chain.canonical_address(address)?;
        Ok(Self {
            chain,
            kind: AssetKind::Contract,
            address: Some(address),
        })
    }

    /// Trusts the caller: no normalisation, no validation. For addresses read
    /// back out of `assets.address`, or produced by a connector that has
    /// already canonicalized. Passing a non-canonical string here means the
    /// lookup silently misses.
    pub fn contract_canonical(chain: impl Into<ChainRef>, address: impl Into<String>) -> Self {
        Self {
            chain: chain.into(),
            kind: AssetKind::Contract,
            address: Some(address.into()),
        }
    }

    /// The one place that decides what "native" looks like in config.
    /// `None`, `""` and `"0"` all mean native; everything else is a contract.
    /// Handlers should never re-implement this test.
    pub fn from_optional_address(
        chain: impl Into<ChainRef>,
        address: Option<&str>,
    ) -> Result<Self, String> {
        let chain = chain.into();
        match address
            .map(str::trim)
            .filter(|a| !a.is_empty() && *a != "0")
        {
            None => Ok(Self::native(chain)),
            Some(a) => Self::contract(chain, a),
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self.kind, AssetKind::Native)
    }

    /// Accessors so call sites that only want one half of the chain pair don't
    /// have to spell out `.chain.network_type`.
    pub fn network_type(&self) -> &str {
        &self.chain.network_type
    }

    pub fn chain_ref(&self) -> &str {
        &self.chain.chain_ref
    }
}

impl fmt::Display for AssetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.address {
            None => write!(f, "{}/native", self.chain),
            Some(a) => write!(f, "{}/{a}", self.chain),
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

// ─────────────────────────────────────────────────────────────────────────────
// What a handler advertises
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Registry writes
// ─────────────────────────────────────────────────────────────────────────────

/// Upsert an advertised asset and flag it registered. Runtime-checked query
/// (not `query!`) so this compiles without a live DB in CI.
///
/// Conflict target is the named constraint, not the column list: `address` is
/// nullable, so a bare `(network_type, chain_ref, asset_kind, address)` target
/// would never match an existing native row. `ensure_observed_asset` in the
/// Ledgerer names the same constraint for the same reason.
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
        .bind(&spec.key.chain.network_type)
        .bind(&spec.key.chain.chain_ref)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_and_contract_are_distinct_keys() {
        let chain = ChainRef::new(NETWORK_EVM, "1");
        let native = AssetKey::native(chain.clone());
        let usdc = AssetKey::contract(chain, "0xA0B86991C6218B36C1D19D4A2E9EB0CE3606EB48").unwrap();
        assert!(native.is_native());
        assert_eq!(usdc.address.as_deref(), Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"));
        assert_ne!(native, usdc);
    }

    #[test]
    fn optional_address_treats_zero_as_native() {
        let chain = ChainRef::new(NETWORK_EVM, "1");
        for a in [None, Some(""), Some("  "), Some("0")] {
            assert!(AssetKey::from_optional_address(chain.clone(), a).unwrap().is_native());
        }
    }

    #[test]
    fn contract_canonical_does_not_normalise() {
        let k = AssetKey::contract_canonical(ChainRef::new(NETWORK_EVM, "1"), "0xAB");
        assert_eq!(k.address.as_deref(), Some("0xAB"));
    }

    #[test]
    fn serialized_shape_is_still_flat() {
        let k = AssetKey::native(ChainRef::new(NETWORK_SOLANA, "mainnet"));
        let v = serde_json::to_value(&k).unwrap();
        assert_eq!(v["network_type"], "solana");
        assert_eq!(v["chain_ref"], "mainnet");
        assert_eq!(v["kind"], "native");
    }
}