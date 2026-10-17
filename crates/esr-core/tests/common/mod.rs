//! Helpers shared by the `esr-core` integration tests.
//!
//! Each file under `tests/` is its own crate; pull these in with `mod common;`
//! and then `use common::strategies::*;`. Not every test crate uses every
//! helper, hence the `dead_code` allowance.
#![allow(dead_code)]

pub mod strategies;
