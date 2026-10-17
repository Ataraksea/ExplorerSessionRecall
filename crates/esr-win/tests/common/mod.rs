//! Helpers shared by the `#[ignore]` verification spikes. Each test binary
//! includes this with `mod common;` and uses only part of it.

#![allow(dead_code)]

pub mod explorer_guard;
pub mod fold_spike;
