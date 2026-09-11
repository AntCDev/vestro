use bip32::{DerivationPath, XPrv};
use bip39::Mnemonic;

pub const SCHEME_VERSION: i16 = 1;

/// Top-level branch. Hardened. One subtree per role so a role's xprv can be
/// handed to a signer without exposing any other role.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum KeyRole {
    /// Merchant-facing money.
    /// index 0  = merchant main wallet (sweep destination)
    /// index 1+ = per-invoice deposit addresses
    Deposit = 0,
    /// Infrastructure the merchant never sees.
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

/// Well-known indices under `KeyRole::Operational`. Never renumber these —
/// they are load-bearing addresses that already hold value.
pub mod operational {
    pub const GAS_FEEDER: u32 = 0;
    pub const BATCH_VAULT_OWNER: u32 = 1;
    pub const FEE_COLLECTOR: u32 = 2;
}

pub const GAS_FEEDER_PURPOSE: &str = "gas_feeder";
pub const BATCH_VAULT_PURPOSE: &str = "batch_vault_owner";

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
        s.parse()
            .map_err(|e| format!("Invalid derivation path {s}: {e}"))
    }

    /// Derive the extended private key. Every caller that needs an address AND
    /// a signature goes through here, so address and key can never disagree.
    pub fn derive(&self, mnemonic: &str, role: KeyRole, index: u32) -> Result<XPrv, String> {
        let parsed = Mnemonic::parse(mnemonic)
            .map_err(|e| format!("Invalid mnemonic: {e}"))?;
        let seed = parsed.to_seed("");
        let path = self.path(role, index)?;
        XPrv::derive_from_path(&seed, &path)
            .map_err(|e| format!("Failed to derive at {}: {e}", self.path_string(role, index)))
    }
}

#[derive(Clone, Debug)]
pub struct DerivedAddress {
    pub address: String,      // already canonicalized for storage
    pub role: KeyRole,
    pub index: u32,
    pub path: String,
    pub scheme_version: i16,
    pub reference: Option<String>,
}

#[derive(Copy, Clone, Debug)]
pub enum KeyScope {
    Platform,
    Merchant(uuid::Uuid),
}

impl KeyScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            KeyScope::Platform => "platform",
            KeyScope::Merchant(_) => "merchant",
        }
    }
    pub fn merchant_id(&self) -> Option<uuid::Uuid> {
        match self { KeyScope::Merchant(id) => Some(*id), _ => None }
    }
}