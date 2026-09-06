use axum::{extract::State, http::StatusCode, Json};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce, Key
};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2
};
use bip39::Mnemonic;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::networks::evm::derive_evm_address;

#[derive(Deserialize)]
pub struct SignUpMerchantRequest {
    pub name: String,
    pub slug: String,
    pub password: String,
    pub webhook_url: Option<String>,
    pub mnemonic: Option<String>,
}

#[derive(Serialize)]
pub struct SignUpMerchantResponse {
    pub merchant_id: Uuid,
    pub name: String,
    pub slug: String,
    pub status: String,
    pub api_key_id: String,
    pub api_key_secret: String,
    pub mnemonic: String,
    pub webhook_secret: Option<String>,
}

/// Helper to hash user passwords securely using Argon2id
fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("Password hashing failed: {e}"))
}

/// Helper to hash high-entropy API Secrets using SHA-256
fn hash_api_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// AES-256-GCM Authenticated Encryption
fn encrypt_data(master_key: &[u8; 32], data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));

    // Generate standard 96-bit (12-byte) nonce
    let mut nonce_bytes = [0u8; 12];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|e| format!("Encryption error: {e}"))?;

    Ok((ciphertext, nonce_bytes.to_vec()))
}

/// AES-256-GCM Authenticated Decryption
pub fn decrypt_data(master_key: &[u8; 32], ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != 12 {
        return Err("Invalid nonce length".to_string());
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| "Decryption failed (tampered data or wrong key)".to_string())
}

/// POST /api/merchants
pub async fn signup_merchant_handler(
    State(state): State<AppState>,
    Json(payload): Json<SignUpMerchantRequest>,
) -> Result<Json<SignUpMerchantResponse>, (StatusCode, String)> {
    // 1. Fetch & parse 256-bit Hex Master Key
    let master_key_hex = std::env::var("MASTER_KEY").map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MASTER_KEY environment variable not set".to_string(),
        )
    })?;

    let master_key_vec = hex::decode(&master_key_hex).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MASTER_KEY must be a valid 64-character hex string".to_string(),
        )
    })?;

    let master_key: &[u8; 32] = master_key_vec.as_slice().try_into().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "MASTER_KEY must be exactly 32 bytes (64 hex characters)".to_string(),
        )
    })?;

    // 2. Process or generate BIP39 Mnemonic
    let mnemonic_phrase = match payload.mnemonic {
        Some(ref m) => {
            Mnemonic::parse(m)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid mnemonic: {e}")))?;
            m.clone()
        }
        None => {
            let m = Mnemonic::generate(12)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed mnemonic generation: {e}")))?;
            m.to_string()
        }
    };

    // 3. Derive one wallet address per *configured* network family
    let mut derived_wallets: Vec<(&'static str, String)> = Vec::new();
    for client in state.networks.representative_clients() {
        let mut address = client
            .derive_wallet_address(&mnemonic_phrase, 0)
            .map_err(|e| (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to derive {} wallet: {e}", client.network_type()),
            ))?;

        // EVM addresses are checksum-cased; store canonically lowercase.
        // Solana/Bitcoin addresses are case-sensitive — never lowercase them.
        if client.network_type() == "evm" {
            address = address.to_lowercase();
        }

        derived_wallets.push((client.network_type(), address));
    }
    // 4. Hash password with Argon2id & API Secret with SHA-256
    let password_hash = hash_password(&payload.password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let api_key_id = format!("pk_live_{}", &Uuid::new_v4().simple().to_string()[..24]);
    let raw_api_secret = format!("sk_live_{}", Uuid::new_v4().simple());
    let api_key_secret_hash = hash_api_secret(&raw_api_secret);

    // 5. Encrypt Webhook Secret (if provided) using AES-256-GCM
    let (webhook_secret_str, webhook_encrypted, webhook_nonce) = match &payload.webhook_url {
        Some(_) => {
            let secret_str = format!("whsec_{}", Uuid::new_v4().simple());
            let (enc, nonce) = encrypt_data(master_key, secret_str.as_bytes())
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            (Some(secret_str), Some(enc), Some(nonce))
        }
        None => (None, None, None),
    };

    // 6. Encrypt Mnemonic using AES-256-GCM
    let (mnemonic_encrypted, mnemonic_nonce) = encrypt_data(master_key, mnemonic_phrase.as_bytes())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 7. Database Transaction
    let mut tx = state.pool.begin().await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("DB Transaction error: {e}"))
    })?;

    let merchant_id: Uuid = sqlx::query_scalar!(
        r#"
        INSERT INTO merchants (
            name, slug, password_hash, api_key_id, api_key_secret_hash,
            webhook_url, webhook_secret_encrypted, webhook_secret_nonce
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id
        "#,
        payload.name,
        payload.slug,
        password_hash,
        api_key_id,
        api_key_secret_hash,
        payload.webhook_url,
        webhook_encrypted,
        webhook_nonce
    )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to insert merchant: {e}")))?;

    sqlx::query!(
        r#"
        INSERT INTO merchant_key_material (
            merchant_id, key_family, encrypted_secret, encryption_nonce, encryption_version
        )
        VALUES ($1, $2, $3, $4, 1)
        "#,
        merchant_id,
        "bip39",
        mnemonic_encrypted,
        mnemonic_nonce
    )
        .execute(&mut *tx)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed key material insertion: {e}")))?;

    // Insert one wallet + one network-index row per configured family
    for (network_type, address) in &derived_wallets {
        sqlx::query!(
            r#"
            INSERT INTO merchant_wallets (merchant_id, network_type, address)
            VALUES ($1, $2, $3)
            "#,
            merchant_id,
            network_type,
            address
        )
            .execute(&mut *tx)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed {network_type} wallet insertion: {e}")))?;

        sqlx::query!(
            r#"
            INSERT INTO merchant_network_indices (
                merchant_id, network, account_index, next_index
            )
            VALUES ($1, $2, 0, 0)
            "#,
            merchant_id,
            network_type
        )
            .execute(&mut *tx)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed {network_type} index insertion: {e}")))?;
    }

    tx.commit().await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Transaction commit failed: {e}"))
    })?;

    Ok(Json(SignUpMerchantResponse {
        merchant_id,
        name: payload.name,
        slug: payload.slug,
        status: "active".to_string(),
        api_key_id,
        api_key_secret: raw_api_secret,
        mnemonic: mnemonic_phrase,
        webhook_secret: webhook_secret_str,
    }))
}