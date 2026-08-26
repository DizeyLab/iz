// The view tree in `pages` nests deeply enough that the type of one rendered
// page overflows the default query depth when the crate is built with tests.
#![recursion_limit = "256"]

pub mod app;
pub mod auth;
pub mod board;
pub mod detail;
pub mod pages;
pub mod settings;
#[cfg(feature = "ssr")]
pub mod server;
#[cfg(feature = "ssr")]
pub mod smtp;

#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(crate::app::App);
}
