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
pub mod handler;
pub mod invoicer;
pub mod registry;
pub mod sweeper;

mod evm_common;
mod sol_common;

// pub mod base;
pub mod base_sepolia;
// pub mod bitcoin;
// pub mod eth;
pub mod sepolia;
pub mod sol_devnet;

pub use checkout::{CheckoutContext, PresignContext, StatusContext};
pub use handler::{TokenHandler, TokenSummary};
pub use invoicer::Invoicer;
pub use registry::TokenRegistry;
pub use sweeper::Sweeper;

pub use crate::keys::crypto::decrypt_data;
