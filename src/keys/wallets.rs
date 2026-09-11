use sqlx::PgPool;
use crate::keys::derivation::*;
use crate::keys::store::load_seed;
use crate::networks::NetworkClient;

/// Backfill (role 0, index 0) for every merchant missing one on this family.
pub async fn ensure_merchant_main_wallets(
    pool: &PgPool,
    client: &dyn NetworkClient,
) -> Result<(), String> {
    let network = client.network_type();

    let missing = sqlx::query!(
        r#"
        SELECT m.id
        FROM merchants m
        JOIN merchant_key_material km ON m.id = km.merchant_id
        WHERE km.key_family = 'bip39'
          AND NOT EXISTS (
              SELECT 1 FROM merchant_wallets mw
              WHERE mw.merchant_id = m.id
                AND mw.network_type = $1
                AND mw.role = 0
                AND mw.wallet_index = 0
          )
        "#,
        network
    )
        .fetch_all(pool).await
        .map_err(|e| format!("Failed to query merchants missing {network} wallets: {e}"))?;

    for record in missing {
        let mnemonic = load_seed(pool, KeyScope::Merchant(record.id)).await?;
        let derived = client.derive(&mnemonic, KeyRole::Deposit, 0)?;

        sqlx::query!(
            r#"
            INSERT INTO merchant_wallets
                (merchant_id, network_type, role, wallet_index, address, derivation_path, scheme_version)
            VALUES ($1, $2, 0, 0, $3, $4, $5)
            ON CONFLICT (merchant_id, network_type, role, wallet_index) DO NOTHING
            "#,
            record.id, network, derived.address, derived.path, derived.scheme_version
        )
            .execute(pool).await
            .map_err(|e| format!("Failed to save {network} wallet for merchant {}: {e}", record.id))?;

        println!("Initialized {network} main wallet ({}) at {} for merchant {}",
                 derived.address, derived.path, record.id);
    }
    Ok(())
}

/// Platform-scoped operational wallets. Family-wide: one feeder address serves
/// every chain in the family, nonces are tracked per chain separately.
pub async fn ensure_platform_operational_wallets(
    pool: &PgPool,
    client: &dyn NetworkClient,
) -> Result<(), String> {
    let network = client.network_type();
    let mnemonic = load_seed(pool, KeyScope::Platform).await?;

    let wanted = [
        (operational::GAS_FEEDER, GAS_FEEDER_PURPOSE),
        (operational::BATCH_VAULT_OWNER, BATCH_VAULT_PURPOSE),
    ];

    for (index, purpose) in wanted {
        let derived = client.derive(&mnemonic, KeyRole::Operational, index)?;

        // Guard against a scheme change silently re-pointing a funded wallet.
        let existing = sqlx::query!(
            "SELECT address, derivation_path FROM operational_wallets \
             WHERE scope = 'platform' AND network_type = $1 AND role = 1 AND wallet_index = $2",
            network, index as i32
        )
            .fetch_optional(pool).await.map_err(|e| e.to_string())?;

        if let Some(row) = existing {
            if row.address != derived.address {
                return Err(format!(
                    "Operational wallet {purpose} on {network} already exists as {} ({}) but the \
                     current scheme derives {} ({}). Seed or scheme changed — refusing to proceed.",
                    row.address, row.derivation_path, derived.address, derived.path
                ));
            }
            continue;
        }

        sqlx::query!(
            r#"
            INSERT INTO operational_wallets
                (scope, merchant_id, network_type, role, wallet_index,
                 purpose, address, derivation_path, scheme_version)
            VALUES ('platform', NULL, $1, 1, $2, $3, $4, $5, $6)
            "#,
            network, index as i32, purpose,
            derived.address, derived.path, derived.scheme_version
        )
            .execute(pool).await
            .map_err(|e| format!("Failed to save {purpose} for {network}: {e}"))?;

        println!("Initialized {network} {purpose} ({}) at {}", derived.address, derived.path);
    }
    Ok(())
}

pub async fn operational_address(
    pool: &PgPool,
    network_type: &str,
    index: u32,
) -> Result<String, String> {
    sqlx::query_scalar!(
        "SELECT address FROM operational_wallets \
         WHERE scope = 'platform' AND network_type = $1 AND role = 1 AND wallet_index = $2",
        network_type, index as i32
    )
        .fetch_optional(pool).await.map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No operational wallet {index} for {network_type}"))
}