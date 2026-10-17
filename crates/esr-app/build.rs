//! Embeds `esr.manifest` (Common Controls 6, per-monitor DPI awareness v2,
//! supportedOS, asInvoker) into esr.exe and the esr-app test executables.
//!
//! No build dependencies: the MSVC linker (link.exe or lld-link) embeds the
//! manifest itself with `/MANIFEST:EMBED /MANIFESTINPUT:<file>`, so no `.rc`
//! file or resource compiler is needed. `/MANIFESTUAC:NO` keeps the linker
//! from adding a second `trustInfo` block (the manifest has its own).
//!
//! `rustc-link-arg` applies to every linked target of this package (the `esr`
//! binary and the test harnesses), not to crates that depend on `esr-app`.
//! The UI therefore resolves `TaskDialogIndirect` at run time (see
//! `ui::prompt`) instead of importing it, so those other executables still
//! load without the Common Controls 6 manifest.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=esr.manifest");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        // The project only supports the MSVC toolchain; other targets get no manifest.
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let manifest = manifest_dir.join("esr.manifest");
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFESTUAC:NO");
}
