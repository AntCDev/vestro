// use rust_decimal::Decimal;
// use sqlx::{PgConnection, PgPool};
// use uuid::Uuid;
// use crate::keys::derivation::purpose;
// use crate::keys::wallets::wallet_address;
// use crate::networks::NetworkClient;
// 
// /// Called by the sweeper before claiming a sweep_queue row. Idempotent:
// /// the partial unique index absorbs a concurrent or retried call.
// /// Returns the live intent, or None if the address already has enough gas.
// pub async fn ensure_gas<GasIntent>(
//     pool: &PgPool,
//     client: &dyn NetworkClient,
//     merchant_id: Uuid,
//     invoice_id: Option<Uuid>,
//     to_address: &str,
//     required: Decimal,   // base units, incl. buffer
// ) -> Result<Option<GasIntent>, String> {
//     let have = client.get_native_balance(to_address).await?.base_units();
//     if have >= required {
//         return Ok(None);
//     }
//     let deficit = required - have;
// 
//     let feeder = wallet_address(pool, merchant_id, client.network_type(), purpose::GAS_FEEDER)
//         .await?;
// 
//     let row = sqlx::query_as!(
//         GasIntent,
//         r#"
//         INSERT INTO gas_funding_intents
//             (merchant_id, invoice_id, network_type, chain_ref,
//              from_address, to_address, amount)
//         VALUES ($1, $2, $3, $4, $5, $6, $7)
//         ON CONFLICT DO NOTHING
//         RETURNING id, status, amount, nonce, tx_hash
//         "#,
//         merchant_id, invoice_id, client.network_type(), client.chain_ref(),
//         feeder, to_address, deficit
//     )
//         .fetch_optional(pool).await.map_err(|e| e.to_string())?;
// 
//     match row {
//         Some(intent) => Ok(Some(intent)),
//         // Conflict: a live intent already exists for this destination. Return
//         // it rather than opening a second one — two advances to one address
//         // means two nonces in flight and one of them will strand.
//         None => existing_live_intent(pool, client, to_address).await.map(Some),
//     }
// }
// 
// /// Nonce allocation for a feeder on one chain. Seeded from the node's pending
// /// count the first time we ever sign for this address on this chain, then
// /// owned by the DB — the node's view lags our own submissions.
// pub async fn allocate_nonce(
//     conn: &mut PgConnection,
//     client: &dyn NetworkClient,
//     address: &str,
// ) -> Result<i64, String> {
//     let seeded: Option<i64> = sqlx::query_scalar!(
//         r#"
//         UPDATE chain_nonces SET next_nonce = next_nonce + 1, updated_at = now()
//          WHERE address = $1 AND network_type = $2 AND chain_ref = $3
//         RETURNING next_nonce - 1
//         "#,
//         address, client.network_type(), client.chain_ref()
//     )
//         .fetch_optional(&mut *conn).await.map_err(|e| e.to_string())?
//         .flatten();
// 
//     if let Some(n) = seeded { return Ok(n); }
// 
//     let onchain = client.get_pending_nonce(address).await? as i64;
//     sqlx::query!(
//         "INSERT INTO chain_nonces (address, network_type, chain_ref, next_nonce) \
//          VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
//         address, client.network_type(), client.chain_ref(), onchain + 1
//     )
//         .execute(&mut *conn).await.map_err(|e| e.to_string())?;
//     Ok(onchain)
// }