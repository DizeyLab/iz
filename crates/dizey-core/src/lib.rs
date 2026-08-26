//! Domain model and storage for Dizey.
//!
//! The vocabulary — roles and record shapes — compiles everywhere, so the UI
//! and the server speak the same types. The `server` feature adds the halves
//! that must never reach the browser: the store, the password hashing and the
//! token minting.

pub mod role;
pub use role::Role;

#[cfg(feature = "server")]
pub mod accounts;
#[cfg(feature = "server")]
pub mod auth;
#[cfg(feature = "server")]
pub mod store;

#[cfg(feature = "server")]
pub use accounts::{AccountError, Accounts};
#[cfg(feature = "server")]
pub use store::{Store, StoreError, TursoStore};
