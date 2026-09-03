//! Domain model and storage for İz.
//!
//! The vocabulary — roles and record shapes — compiles everywhere, so the UI
//! and the server speak the same types. The `server` feature adds the halves
//! that must never reach the browser: the store, the password hashing and the
//! token minting.

pub mod board;
pub mod detail;
pub mod role;
pub use board::{BoardView, Column, TaskCard};
pub use detail::TaskDetail;
pub use role::Role;

#[cfg(feature = "server")]
pub mod accounts;
#[cfg(feature = "server")]
pub mod auth;
#[cfg(feature = "server")]
pub mod live;
#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod mail;
#[cfg(feature = "server")]
pub mod store;

#[cfg(feature = "server")]
pub use accounts::{AccountError, Accounts};
#[cfg(feature = "server")]
pub use board::{BoardReads, load as load_board};
#[cfg(feature = "server")]
pub use config::{Config, ConfigError};
#[cfg(feature = "server")]
pub use detail::{DetailReads, load as load_detail};
#[cfg(feature = "server")]
pub use live::{Change, Topic};
#[cfg(feature = "server")]
pub use mail::{Engine as MailEngine, MailError, Mailer, Outgoing};
#[cfg(feature = "server")]
pub use store::{Store, StoreError, TursoStore};
