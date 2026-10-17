//! Deterministic simulated File Explorer (test-only).
//!
//! Runs `esr-core` commands and produces events on logical time, and provides a
//! reference model for model-based property tests.
#![forbid(unsafe_code)]

pub mod explorer;
pub mod fold;
pub mod reference;
pub mod restore;
