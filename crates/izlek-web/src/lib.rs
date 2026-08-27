// topcoat's `view!` nests as deeply as the old UI's did for the same pages, so
// the raised limit carries over from day one rather than waiting for the
// first overflow.
#![recursion_limit = "256"]

pub mod auth;
pub mod board;
pub mod detail;
pub mod files;
pub mod layout;
pub mod logs;
pub mod rules;
pub mod settings;
pub mod pages;
pub mod server;
pub mod smtp;
