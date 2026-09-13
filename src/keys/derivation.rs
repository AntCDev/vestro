use bip32::DerivationPath;
use bip39::Mnemonic;

pub const SCHEME_VERSION: i16 = 1;

/// Top-level branch. Hardened. One subtree per role so a role's subtree can be
/// handed to a signer without exposing any other role — and, more immediately,
/// so the key that must live hot (the gas feeder, which signs on every sweep)
/// is never the key that holds swept funds (the main wallet).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum KeyRole {
    /// Merchant-facing money.
    /// index 0  = main wallet (sweep destination)
    /// index 1+ = per-invoice deposit addresses
    Deposit = 0,
    /// Infrastructure the merchant never quotes to a payer.
    Operational = 1,
}

impl KeyRole {
    pub const fn as_u32(self) -> u32 { self as u32 }
    pub const fn as_i16(self) -> i16 { self as i16 }

    pub fn from_i16(v: i16) -> Result<Self, String> {
        match v {
            0 => Ok(KeyRole::Deposit),
            1 => Ok(KeyRole::Operational),
            other => Err(format!("Unknown key role {other}")),
        }
    }
}

/// Well-known indices under `KeyRole::Operational`. Never renumber — these are
/// load-bearing addresses that hold value.
pub mod operational {
    pub const GAS_FEEDER: u32 = 0;
    pub const FEE_COLLECTOR: u32 = 1;
    // 2 is reserved for a future per-merchant vault authority. The current
    // batching vault is shared infrastructure and has no merchant-scoped key.
}

pub mod purpose {
    pub const MAIN: &str = "main";
    pub const GAS_FEEDER: &str = "gas_feeder";
    pub const FEE_COLLECTOR: &str = "fee_collector";
}

/// A named wallet a network family wants provisioned for every merchant.
#[derive(Copy, Clone, Debug)]
pub struct WalletSpec {
    pub role: KeyRole,
    pub index: u32,
    pub purpose: &'static str,
}

pub const MAIN_WALLET: WalletSpec = WalletSpec {
    role: KeyRole::Deposit,
    index: 0,
    purpose: purpose::MAIN,
};

pub const GAS_FEEDER_WALLET: WalletSpec = WalletSpec {
    role: KeyRole::Operational,
    index: operational::GAS_FEEDER,
    purpose: purpose::GAS_FEEDER,
};

#[derive(Clone, Debug)]
pub struct DerivationScheme {
    pub network_type: &'static str,
    pub coin_type: u32,
    pub template: &'static str,
    pub version: i16,
}

impl DerivationScheme {
    pub fn path_string(&self, role: KeyRole, index: u32) -> String {
        self.template
            .replace("{coin}", &self.coin_type.to_string())
            .replace("{role}", &role.as_u32().to_string())
            .replace("{index}", &index.to_string())
    }

    pub fn path(&self, role: KeyRole, index: u32) -> Result<DerivationPath, String> {
        let s = self.path_string(role, index);
        s.parse().map_err(|e| format!("Invalid derivation path {s}: {e}"))
    }

    /// BIP-39 seed. Curve-agnostic on purpose: secp256k1 families walk this
    /// with bip32::XPrv, ed25519 families walk it with SLIP-0010. There is no
    /// shared key type, so this is as far up as the shared code can go.
    pub fn seed(&self, mnemonic: &str) -> Result<[u8; 64], String> {
        let parsed = Mnemonic::parse(mnemonic).map_err(|e| format!("Invalid mnemonic: {e}"))?;
        Ok(parsed.to_seed(""))
    }
}

/// What a derivation produces. Address is already canonical for storage.
#[derive(Clone, Debug)]
pub struct DerivedAddress {
    pub address: String,
    pub role: KeyRole,
    pub index: u32,
    pub path: String,
    pub scheme_version: i16,
    /// Only populated by `next_deposit_address` — the on-chain reference key
    /// (EVM vault calldata tag, Solana reference pubkey) for this invoice.
    pub reference: Option<String>,
}