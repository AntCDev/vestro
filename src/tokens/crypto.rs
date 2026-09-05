//! Key material handling shared by every invoicer that derives addresses.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use sqlx::PgPool;
use uuid::Uuid;

pub fn decrypt_data(
    master_key: &[u8; 32],
    ciphertext: &[u8],
    nonce_bytes: &[u8],
) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != 12 {
        return Err("Invalid nonce length: expected 12 bytes".to_string());
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| "Decryption failed (tampered data or wrong key)".to_string())
}

/// Load + decrypt a merchant's BIP39 mnemonic.
///
/// This was thirty identical lines at the top of every `create_invoice_payment`.
/// It is the same on every chain — the mnemonic is chain-agnostic, only the
/// derivation path downstream is not — so it lives here once.
pub async fn load_merchant_mnemonic(pool: &PgPool, merchant_id: Uuid) -> Result<String, String> {
    let master_key_hex = std::env::var("MASTER_KEY")
        .map_err(|_| "MASTER_KEY environment variable not set".to_string())?;

    let master_key_vec =
        hex::decode(&master_key_hex).map_err(|e| format!("Failed to decode MASTER_KEY hex: {e}"))?;

    let master_key: &[u8; 32] = master_key_vec
        .as_slice()
        .try_into()
        .map_err(|_| "MASTER_KEY must be exactly 32 bytes (64 hex characters)".to_string())?;

    let key_material = sqlx::query!(
        r#"
        SELECT encrypted_secret, encryption_nonce
        FROM merchant_key_material
        WHERE merchant_id = $1 AND key_family = 'bip39'
        "#,
        merchant_id
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("Failed to fetch key material for merchant {merchant_id}: {e}"))?;

    let decrypted_bytes = decrypt_data(
        master_key,
        &key_material.encrypted_secret,
        &key_material.encryption_nonce,
    )?;

    String::from_utf8(decrypted_bytes)
        .map_err(|e| format!("Invalid UTF-8 sequence in decrypted mnemonic: {e}"))
}
