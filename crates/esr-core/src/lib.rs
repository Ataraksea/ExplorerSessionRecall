//! Functional core of Explorer Session Restore.
//!
//! Every input is an [`engine`] `Event` and every output a `Command`; this crate
//! has no Windows dependencies, so all behavioural logic is testable against the
//! simulated Explorer in `esr-sim`.
#![forbid(unsafe_code)]

pub mod engine;
pub mod fold;
pub mod format;
pub mod launch;
pub mod logfmt;
pub mod model;
pub mod plan;
pub mod restore;
pub mod search;
pub mod settings;
pub mod store;
pub mod tracker;
