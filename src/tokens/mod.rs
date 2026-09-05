//! Token layer.
//!
//! ```text
//! TokenHandler  ── descriptor()  ──▶  TokenDescriptor { network, chain, testnet, asset }
//!      │
//!      ├── invoicer()  -> Option<&dyn Invoicer>   create + serve checkout
//!      └── sweeper()   -> Option<&dyn Sweeper>    move funds out (dummy)
//! ```
//!
//! A capability that a token does not have is simply `None`. Observation is not
//! a capability here — see the note at the top of `invoicer.rs`.

pub mod checkout;
pub mod crypto;
pub mod handler;
pub mod invoicer;
pub mod registry;
pub mod sweeper;

mod evm_common;
mod sol_common;

pub mod base;
pub mod base_sepolia;
pub mod bitcoin;
pub mod eth;
pub mod sepolia;
pub mod sol_devnet;

pub use checkout::{CheckoutContext, CheckoutView, PresignContext, StatusContext, GENERIC_VIEW};
pub use handler::{Capabilities, HandlerStatus, TokenDescriptor, TokenHandler, TokenSummary};
pub use invoicer::{Invoicer, PaymentDetails};
pub use registry::TokenRegistry;
pub use sweeper::{SweepOutcome, SweepRequest, Sweeper};

pub use crypto::{decrypt_data, load_merchant_mnemonic};
