use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::keys::derivation::*;
use crate::keys::store::load_merchant_seed;
use crate::networks::NetworkClient;

/// Derive and persist every wallet this family requires for one merchant.
/// Takes a connection so registration can run it inside the same transaction
/// that inserts the merchant and its key material.
pub async fn provision_merchant_wallets(
    conn: &mut PgConnection,
    client: &dyn NetworkClient,
    merchant_id: Uuid,
    mnemonic: &str,
) -> Result<Vec<DerivedAddress>, String> {
    let network = client.network_type();
    let mut out = Vec::new();

    for spec in client.required_wallets() {
        let derived = client.derive(mnemonic, spec.role, spec.index)?;

        // Refuse to shadow a funded wallet. If a row exists at this
        // (role, index) with a different address, the seed or the scheme
        // changed underneath us and every assumption downstream is void.
        let existing = sqlx::query!(
            "SELECT address, derivation_path FROM merchant_wallets \
             WHERE merchant_id = $1 AND network_type = $2 AND role = $3 AND wallet_index = $4",
            merchant_id, network, spec.role.as_i16(), spec.index as i32
        )
            .fetch_optional(&mut *conn).await.map_err(|e| e.to_string())?;

        if let Some(row) = existing {
            if row.address != derived.address {
                return Err(format!(
                    "{network} {} for merchant {merchant_id} exists as {} ({}) but the current \
                     scheme derives {} ({}). Seed or scheme changed — refusing to proceed.",
                    spec.purpose, row.address, row.derivation_path, derived.address, derived.path
                ));
            }
            out.push(derived);
            continue;
        }

        sqlx::query!(
            r#"
            INSERT INTO merchant_wallets
                (merchant_id, network_type, role, wallet_index,
                 purpose, address, derivation_path, scheme_version)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (merchant_id, network_type, role, wallet_index) DO NOTHING
            "#,
            merchant_id, network, spec.role.as_i16(), spec.index as i32,
            spec.purpose, derived.address, derived.path, derived.scheme_version
        )
            .execute(&mut *conn).await
            .map_err(|e| format!("Failed to save {network} {} for merchant {merchant_id}: {e}",
                                 spec.purpose))?;

        println!("Initialized {network} {} ({}) at {} for merchant {merchant_id}",
                 spec.purpose, derived.address, derived.path);
        out.push(derived);
    }

    // Reserve deposit index 0 for the main wallet so the invoice allocator
    // can never hand it out. next_index is the highest ALLOCATED index.
    sqlx::query!(
        "INSERT INTO merchant_network_indices (merchant_id, network, role, next_index) \
         VALUES ($1, $2, 0, 0) ON CONFLICT DO NOTHING",
        merchant_id, network
    )
        .execute(&mut *conn).await.map_err(|e| e.to_string())?;

    Ok(out)
}

/// Backfill at spin_up: any merchant missing any required wallet on this
/// family. Replaces the per-network `ensure_merchant_wallets`.
///
/// Note the predicate is on `purpose`, not on role/index — a merchant who
/// registered before this family declared a gas feeder needs one added
/// without disturbing their main wallet.
pub async fn ensure_merchant_wallets(
    pool: &PgPool,
    client: &dyn NetworkClient,
) -> Result<(), String> {
    let network = client.network_type();
    let purposes: Vec<String> = client
        .required_wallets()
        .iter()
        .map(|s| s.purpose.to_string())
        .collect();

    let incomplete = sqlx::query!(
        r#"
        SELECT m.id
        FROM merchants m
        JOIN merchant_key_material km ON m.id = km.merchant_id
        WHERE km.key_family = 'bip39'
          AND EXISTS (
              SELECT 1 FROM unnest($2::text[]) AS want(purpose)
              WHERE NOT EXISTS (
                  SELECT 1 FROM merchant_wallets mw
                  WHERE mw.merchant_id = m.id
                    AND mw.network_type = $1
                    AND mw.purpose = want.purpose
              )
          )
        "#,
        network, &purposes
    )
        .fetch_all(pool).await
        .map_err(|e| format!("Failed to query merchants missing {network} wallets: {e}"))?;

    for record in incomplete {
        let mnemonic = load_merchant_seed(pool, record.id).await?;
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        provision_merchant_wallets(&mut tx, client, record.id, &mnemonic).await?;
        tx.commit().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Allocate the next deposit index for (merchant, family). Role is always
/// Deposit — operational indices are fixed constants and never allocated.
pub async fn allocate_deposit_index(
    pool: &PgPool,
    merchant_id: Uuid,
    network: &str,
) -> Result<u32, String> {
    let row = sqlx::query!(
        r#"
        INSERT INTO merchant_network_indices (merchant_id, network, role, next_index)
        VALUES ($1, $2, 0, 1)
        ON CONFLICT (merchant_id, network, role)
        DO UPDATE SET next_index = merchant_network_indices.next_index + 1,
                      updated_at = now()
        RETURNING next_index
        "#,
        merchant_id, network
    )
        .fetch_one(pool).await
        .map_err(|e| format!("Failed to allocate deposit index: {e}"))?;

    Ok(row.next_index as u32)
}

/// Lookup by purpose. The gas feeder and the sweep destination both come from
/// here, so neither the feeder nor the sweeper ever re-derives.
pub async fn wallet_address(
    pool: &PgPool,
    merchant_id: Uuid,
    network: &str,
    purpose: &str,
) -> Result<String, String> {
    sqlx::query_scalar!(
        "SELECT address FROM merchant_wallets \
         WHERE merchant_id = $1 AND network_type = $2 AND purpose = $3",
        merchant_id, network, purpose
    )
        .fetch_optional(pool).await.map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Merchant {merchant_id} has no {purpose} wallet on {network}"))
}

/// Watcher classification. An address the merchant owns is never `External`,
/// and the gas feeder in particular must classify as `Gas` or the Ledgerer
/// will book a top-up into custody_unswept and enqueue a sweep of it.
pub async fn address_kind(
    pool: &PgPool,
    network: &str,
    address: &str,
) -> Result<Option<(Uuid, crate::ledgerer::AddressKind)>, String> {
    use crate::ledgerer::AddressKind;

    let row = sqlx::query!(
        "SELECT merchant_id, purpose FROM merchant_wallets \
         WHERE network_type = $1 AND address = $2",
        network, address
    )
        .fetch_optional(pool).await.map_err(|e| e.to_string())?;

    Ok(row.map(|r| {
        let kind = match r.purpose.as_str() {
            purpose::MAIN => AddressKind::MerchantMain,
            purpose::GAS_FEEDER => AddressKind::Gas,
            _ => AddressKind::Operator,
        };
        (r.merchant_id, kind)
    }))
}