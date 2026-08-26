//! Domain model and storage for Dizey.
//!
//! Everything in here is server-side only: the web crate pulls it in behind its
//! `ssr` feature so none of it is compiled into the wasm bundle.

pub mod accounts;
pub mod auth;
pub mod role;
pub mod store;

pub use role::Role;
pub use accounts::{AccountError, Accounts};
pub use store::{Store, StoreError, TursoStore};
