//! The handler itself: an advertiser, plus a link to whatever it can actually do.
//!
//! `TokenHandler` no longer contains payment logic. It answers two questions:
//!   1. what is this token?            -> `descriptor()`
//!   2. what can it do?                -> `invoicer()` / `sweeper()`
//!
//! Advertising is not a capability flag because it is not optional — a handler
//! that cannot describe itself cannot be registered at all.

use serde::Serialize;

use crate::assets::AssetSpec;
use crate::tokens::invoicer::Invoicer;
use crate::tokens::sweeper::Sweeper;

/// Shown in the UI as a badge. `Experimental` is the "WIP, here so you can watch
/// it come together" state — devnet handlers, half-finished chains.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HandlerStatus {
    Stable,
    Experimental,
}

impl HandlerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HandlerStatus::Stable => "stable",
            HandlerStatus::Experimental => "experimental",
        }
    }
}

/// Everything a handler advertises about itself. Purely informative except for
/// `asset`, which is what the ledger and the sweep planner resolve against.
#[derive(Clone, Debug, Serialize)]
pub struct TokenDescriptor {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub info: String,

    /// Canonical network family: "evm" | "solana" | "esplora" | …
    /// Same string as `assets.network_type` and `invoices.network_type`.
    pub network: String,
    /// Canonical chain within that family: "84532", "8453", "devnet",
    /// "mainnet", "testnet3". A string precisely so an EVM chain id and a
    /// Solana cluster name can share the column.
    pub chain: String,
    /// Grouping flag for the UI. Not derived from `chain` — the mapping from
    /// chain to "is this play money" is not something a parser should guess.
    pub testnet: bool,

    pub status: HandlerStatus,
    pub asset: AssetSpec,
}

impl TokenDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        name: &str,
        detail: &str,
        info: &str,
        network: &str,
        chain: &str,
        testnet: bool,
        asset: AssetSpec,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            detail: detail.to_string(),
            info: info.to_string(),
            network: network.to_string(),
            chain: chain.to_string(),
            testnet,
            status: HandlerStatus::Stable,
            asset,
        }
    }

    pub fn experimental(mut self) -> Self {
        self.status = HandlerStatus::Experimental;
        self
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Capabilities {
    /// Always true. Present so the frontend can render one uniform list.
    pub advertise: bool,
    pub invoice: bool,
    pub sweep: bool,
}

impl Capabilities {
    pub fn badge(&self) -> String {
        let mut parts = vec!["advertise"];
        if self.invoice {
            parts.push("invoice");
        }
        if self.sweep {
            parts.push("sweep");
        }
        parts.join("+")
    }
}

pub trait TokenHandler: Send + Sync {
    fn descriptor(&self) -> &TokenDescriptor;

    fn token_id(&self) -> &str {
        &self.descriptor().id
    }

    /// `None` == this token cannot be invoiced. It still advertises its asset,
    /// so the ledger and any sweeper keep working.
    fn invoicer(&self) -> Option<&dyn Invoicer> {
        None
    }

    /// `None` == funds received against this token's asset must be swept by
    /// some *other* handler advertising the same asset, or not at all.
    fn sweeper(&self) -> Option<&dyn Sweeper> {
        None
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            advertise: true,
            invoice: self.invoicer().is_some(),
            sweep: self.sweeper().is_some(),
        }
    }
}

/// What the frontend gets: descriptor fields flattened, plus capabilities.
/// Group by `network`, then `chain`, filter on `testnet`, badge on `status`.
#[derive(Clone, Debug, Serialize)]
pub struct TokenSummary {
    #[serde(flatten)]
    pub descriptor: TokenDescriptor,
    pub capabilities: Capabilities,
}

impl TokenSummary {
    pub fn from_handler(h: &dyn TokenHandler) -> Self {
        Self {
            descriptor: h.descriptor().clone(),
            capabilities: h.capabilities(),
        }
    }
}
