//! Latch node: control service, streaming engine setup, and the unified
//! window that both views other machines and shares this one.

pub mod app;
pub mod clipboard;
pub mod config;
pub mod display;
pub mod legacy;
pub mod logfile;
pub mod power;
pub mod prep;
#[cfg(feature = "client")]
pub mod product;
pub mod service;
pub mod setup;
pub mod streamer;
#[cfg(not(windows))]
pub mod unix_setup;
pub mod update;
pub mod virtual_display;
pub mod wake;
