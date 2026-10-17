//! Windows adapters for Explorer Session Restore.
//!
//! Each adapter turns Windows signals into `esr_core` events or carries out
//! `esr_core` commands. The port traits they back live in `esr_app::ports`.

// Shared helpers and spike-derived strategy selections.
pub mod diag;
pub mod browser_worker;
pub mod desktops;
pub mod keys;
pub mod location;
pub mod scanner;
pub mod shell_util;
pub mod strategy;

// Adapters (see design.md, "esr-win (adapters)").
pub mod display_info;
pub mod explorer_options;
pub mod file_store;
pub mod fold_probe;
pub mod hider;
pub mod log_sink;
pub mod navigator;
pub mod placeholder;
pub mod prober;
pub mod session_end;
pub mod shell_windows_source;
pub mod startup;
pub mod tab_strip;
pub mod toast;
pub mod uia_strip;
pub mod window_event_source;
