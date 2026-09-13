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
    let nonce = Nonce::try_from(nonce_bytes)
        .map_err(|_| "Invalid nonce length: expected 12 bytes".to_string())?;
    let cipher = Aes256Gcm::new(master_key.into());

    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| "Decryption failed (tampered data or wrong key)".to_string())
}

/// AES-256-GCM Authenticated Encryption
pub fn encrypt_data(master_key: &[u8; 32], data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let cipher = Aes256Gcm::new(master_key.into());

    // Generate standard 96-bit (12-byte) nonce
    let mut nonce_bytes = [0u8; 12];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, data)
        .map_err(|e| format!("Encryption error: {e}"))?;

    Ok((ciphertext, nonce_bytes.to_vec()))
}