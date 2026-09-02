// topcoat's `view!` nests as deeply as the old UI's did for the same pages, so
// the raised limit carries over from day one rather than waiting for the
// first overflow.
#![recursion_limit = "256"]

pub mod auth;
pub mod board;
pub mod detail;
pub mod dropdown;
pub mod feed;
pub mod files;
pub mod i18n;
pub mod layout;
pub mod live;
pub mod logs;
pub mod pages;
pub mod people;
pub mod photo;
pub mod rules;
pub mod server;
pub mod settings;
pub mod sheet;
pub mod smtp;
pub mod tags;
