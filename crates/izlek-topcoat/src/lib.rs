// topcoat's `view!` nests as deeply as leptos's did for the same pages, so
// the raised limit carries over from day one rather than waiting for the
// first overflow.
#![recursion_limit = "256"]

pub mod layout;
pub mod server;
pub mod smtp;
