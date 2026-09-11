use sqlx::PgPool;
use crate::keys::derivation::{DerivationScheme, KeyScope};

fn master_key() -> Result<[u8; 32], String> {
    let hex_key = std::env::var("MASTER_KEY")
        .map_err(|_| "MASTER_KEY environment variable not set".to_string())?;
    let bytes = hex::decode(&hex_key)
        .map_err(|_| "MASTER_KEY must be a valid hex string".to_string())?;
    bytes.as_slice().try_into()
        .map_err(|_| "MASTER_KEY must be exactly 32 bytes".to_string())
}

pub async fn load_seed(pool: &PgPool, scope: KeyScope) -> Result<String, String> {
    let key = master_key()?;
    let (secret, nonce) = match scope {
        KeyScope::Platform => {
            let row = sqlx::query!(
                "SELECT encrypted_secret, encryption_nonce FROM platform_key_material WHERE id = 1"
            )
                .fetch_optional(pool).await.map_err(|e| e.to_string())?
                .ok_or("Platform key material missing — run bootstrap")?;
            (row.encrypted_secret, row.encryption_nonce)
        }
        KeyScope::Merchant(id) => {
            let row = sqlx::query!(
                "SELECT encrypted_secret, encryption_nonce FROM merchant_key_material \
                 WHERE merchant_id = $1 AND key_family = 'bip39'",
                id
            )
                .fetch_optional(pool).await.map_err(|e| e.to_string())?
                .ok_or_else(|| format!("No bip39 key material for merchant {id}"))?;
            (row.encrypted_secret, row.encryption_nonce)
        }
    };

    let plaintext = decrypt_data(&key, &secret, &nonce)
        .map_err(|e| format!("Failed to decrypt seed for {}: {e}", scope.as_str()))?;
    String::from_utf8(plaintext).map_err(|e| format!("Seed is not valid UTF-8: {e}"))
}

/// Creates the platform seed if absent. Gated behind an env flag so a deploy
/// pointed at an empty database cannot silently mint a new gas feeder and
/// orphan the funded one.
pub async fn ensure_platform_seed(pool: &PgPool) -> Result<(), String> {
    let exists = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM platform_key_material WHERE id = 1)")
        .fetch_one(pool).await.map_err(|e| e.to_string())?.unwrap_or(false);
    if exists { return Ok(()); }

    if std::env::var("ALLOW_PLATFORM_SEED_BOOTSTRAP").as_deref() != Ok("1") {
        return Err("No platform seed present and bootstrap not enabled. \
                    Set ALLOW_PLATFORM_SEED_BOOTSTRAP=1 only on a fresh install.".into());
    }

    let mnemonic = bip39::Mnemonic::generate(24)
        .map_err(|e| format!("Failed to generate platform mnemonic: {e}"))?
        .to_string();
    let (ciphertext, nonce) = encrypt_data(&master_key()?, mnemonic.as_bytes())
        .map_err(|e| format!("Failed to encrypt platform mnemonic: {e}"))?;

    sqlx::query!(
        "INSERT INTO platform_key_material (encrypted_secret, encryption_nonce) \
         VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        ciphertext, nonce
    ).execute(pool).await.map_err(|e| e.to_string())?;

    eprintln!("!! Generated a new platform seed. Back it up now — losing it means \
               losing every operational wallet including the gas feeder.");
    Ok(())
}

/// Boot assertion. The DB row documents the scheme; it never drives derivation.
pub async fn assert_scheme(pool: &PgPool, scheme: &DerivationScheme) -> Result<(), String> {
    let row = sqlx::query!(
        "SELECT coin_type, template FROM derivation_schemes \
         WHERE network_type = $1 AND version = $2 AND active",
        scheme.network_type, scheme.version
    )
        .fetch_optional(pool).await.map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No active derivation scheme v{} for {}", scheme.version, scheme.network_type))?;

    if row.coin_type != scheme.coin_type as i32 || row.template != scheme.template {
        return Err(format!(
            "Derivation scheme mismatch for {}: db has coin={} template={}, code has coin={} template={}. \
             Refusing to derive.",
            scheme.network_type, row.coin_type, row.template, scheme.coin_type, scheme.template
        ));
    }
    Ok(())
}