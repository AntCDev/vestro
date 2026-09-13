use sqlx::PgPool;
use uuid::Uuid;
use crate::keys::derivation::DerivationScheme;
use crate::tokens::decrypt_data;

fn master_key() -> Result<[u8; 32], String> {
    let hex_key = std::env::var("MASTER_KEY")
        .map_err(|_| "MASTER_KEY environment variable not set".to_string())?;
    let bytes = hex::decode(&hex_key)
        .map_err(|_| "MASTER_KEY must be a valid hex string".to_string())?;
    bytes.as_slice().try_into()
        .map_err(|_| "MASTER_KEY must be exactly 32 bytes".to_string())
}

/// The only place a merchant mnemonic is decrypted. Callers hold it for the
/// duration of one derivation and drop it.
pub async fn load_merchant_seed(pool: &PgPool, merchant_id: Uuid) -> Result<String, String> {
    let row = sqlx::query!(
        "SELECT encrypted_secret, encryption_nonce FROM merchant_key_material \
         WHERE merchant_id = $1 AND key_family = 'bip39'",
        merchant_id
    )
        .fetch_optional(pool).await.map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No bip39 key material for merchant {merchant_id}"))?;

    let plaintext = decrypt_data(&master_key()?, &row.encrypted_secret, &row.encryption_nonce)
        .map_err(|e| format!("Failed to decrypt seed for merchant {merchant_id}: {e}"))?;
    String::from_utf8(plaintext).map_err(|e| format!("Seed is not valid UTF-8: {e}"))
}

/// Boot assertion. The DB row documents the scheme; it never drives derivation.
/// If code and DB disagree, every address this process would derive is a
/// different address than the one holding the money — so refuse to start.
pub async fn assert_scheme(pool: &PgPool, scheme: &DerivationScheme) -> Result<(), String> {
    let row = sqlx::query!(
        "SELECT coin_type, template FROM derivation_schemes \
         WHERE network_type = $1 AND version = $2 AND active",
        scheme.network_type, scheme.version as i16
    )
        .fetch_optional(pool).await.map_err(|e| e.to_string())?
        .ok_or_else(|| format!(
            "No active derivation scheme v{} for {}", scheme.version, scheme.network_type))?;

    if row.coin_type != scheme.coin_type as i32 || row.template != scheme.template {
        return Err(format!(
            "Derivation scheme mismatch for {}: db has coin={} template={}, \
             code has coin={} template={}. Refusing to derive.",
            scheme.network_type, row.coin_type, row.template,
            scheme.coin_type, scheme.template
        ));
    }
    Ok(())
}