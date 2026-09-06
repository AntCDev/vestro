use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::networks::evm::EVMNetwork;
use crate::tokens::{CheckoutContext};

#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub info: &'static str,
    pub token_address: Option<&'static str>, // None for native ETH
    pub decimals: u8,
    pub required_confirmations: i32,
}
/// Shared checkout payload for every EVM handler (eth, base, polygon,
/// sepolia, base_sepolia). Everything that varies is already in
/// `network` + `config`, so per-chain handlers delegate here in one line.
///
/// PUBLIC OUTPUT — anyone with the invoice link can read this. No RPC URLs,
/// no derivation indices, no key material.
pub async fn evm_checkout_data(
    network: &EVMNetwork,
    config: &TokenConfig,
    pool: &PgPool,
    ctx: &CheckoutContext,
) -> Result<Value, String> {
    let is_native = config.token_address.is_none();

    // NUMERIC(78,0) base units. Always a string: 1 ETH is 1e18, which loses
    // integer precision the moment it becomes a JS number.
    let amount_base = ctx.amount_requested.normalize().to_string();

    let mut out = json!({
        "chain": {
            "chain_id": network.chain_id(),
            "name": network.display_name(),
        },
        "token": {
            "symbol": config.name,
            "address": config.token_address,
            "decimals": config.decimals,
            "is_native": is_native,
        },
        "amount": {
            "base_units": amount_base,
            "display": to_display_units(ctx.amount_requested, config.decimals),
        },
        // Bare address on purpose — no EIP-681 URI. Per NETWORKS.md the naive
        // path exists precisely because wallets misparse QR-embedded metadata.
        "naive_path": {
            "deposit_address": ctx.wallet_address,
        },
        // Filled below iff a vault is configured for this chain.
        "smart_path": Value::Null,
    });

    let Some(vault) = network.vault_address() else {
        return Ok(out);
    };

    // Same value watch_logs matches the Payment event's indexed `identifier`
    // against, so the smart path and the detector cannot drift apart.
    let identifier = ctx
        .payment_reference
        .as_deref()
        .ok_or("EVM invoice has no payment_reference (bytes16 identifier)")?;

    if !is_bytes16_hex(identifier) {
        return Err(format!(
            "payment_reference {identifier} is not a 16-byte hex identifier"
        ));
    }

    let merchant_wallet = network.merchant_wallet(pool, ctx.merchant_id).await?;

    // Args are self-describing so a view can either map positionally
    // (args.map(a => a.value)) or check names against the abi string.
    let (abi, function_name, args, value_base_units) = if is_native {
        (
            network.vault_pay_native_abi(),
            "payNative",
            json!([
                { "name": "identifier", "type": "bytes16", "value": identifier },
                { "name": "merchant",   "type": "address",  "value": merchant_wallet },
            ]),
            amount_base.clone(),
        )
    } else {
        let token_address = config.token_address.expect("checked !is_native");
        (
            network.vault_pay_abi(),
            "pay",
            json!([
                { "name": "token",      "type": "address",  "value": token_address },
                { "name": "amount",     "type": "uint256",  "value": amount_base },
                { "name": "identifier", "type": "bytes16",  "value": identifier },
                { "name": "merchant",   "type": "address",  "value": merchant_wallet },
            ]),
            "0".to_string(),
        )
    };

    // ERC-20 only. These signatures are hardcoded rather than env-driven
    // because they're the ERC-20 standard, not the operator's contract —
    // a token that doesn't match them wouldn't work with the vault either.
    let approval = if is_native {
        Value::Null
    } else {
        json!({
            "token_address": config.token_address,
            "spender": vault,
            "amount_base_units": amount_base,
            "allowance_abi": "function allowance(address owner, address spender) view returns (uint256)",
            "approve_abi": "function approve(address spender, uint256 amount) returns (bool)",
        })
    };

    out["smart_path"] = json!({
        "kind": "vault_call",
        "vault_address": vault,
        "merchant_wallet": merchant_wallet,
        "identifier": identifier,
        "call": {
            "abi": abi,
            "function_name": function_name,
            "args": args,
            "value_base_units": value_base_units,
        },
        // null for native: evm.html skips straight to the pay step.
        "approval": approval,
    });

    Ok(out)
}

/// 0x + exactly 32 hex chars, matching the contract's bytes16 identifier
/// (a dash-stripped UUID).
fn is_bytes16_hex(s: &str) -> bool {
    match s.strip_prefix("0x") {
        Some(body) => body.len() == 32 && body.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

/// Base units -> human units by scale shift, so no division rounding.
fn to_display_units(base: Decimal, decimals: u8) -> String {
    let mut d = base;
    if d.set_scale(decimals as u32).is_err() {
        return base.to_string(); // decimals > 28; caller sees base units
    }
    d.normalize().to_string()
}