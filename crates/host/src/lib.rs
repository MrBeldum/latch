//! Latch node: control service, streaming engine setup, and the unified
//! window that both views other machines and shares this one.

pub mod app;
pub mod audio;
pub mod brand;
pub mod clipboard;
pub mod config;
#[cfg(any(windows, test))]
pub mod cursor;
pub mod display;
pub mod legacy;
pub mod logfile;
pub mod migrate;
pub mod power;
pub mod product;
pub mod service;
pub mod setup;
pub mod streamer;
#[cfg(not(windows))]
pub mod unix_setup;
pub mod update;
#[cfg_attr(not(windows), allow(dead_code))]
pub mod verinfo;
pub mod virtual_display;
pub mod wake;
