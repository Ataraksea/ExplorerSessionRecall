//! Embeds `esr.manifest` (Common Controls 6, per-monitor DPI awareness v2,
//! supportedOS, asInvoker) into esr.exe and the esr-app test executables, and
//! a VERSIONINFO resource (product name, publisher, version) into esr.exe.
//!
//! No build dependencies: the MSVC linker (link.exe or lld-link) embeds the
//! manifest itself with `/MANIFEST:EMBED /MANIFESTINPUT:<file>`.
//! `/MANIFESTUAC:NO` keeps the linker from adding a second `trustInfo` block
//! (the manifest has its own).
//!
//! `rustc-link-arg` applies to every linked target of this package (the `esr`
//! binary and the test harnesses), not to crates that depend on `esr-app`.
//! The UI therefore resolves `TaskDialogIndirect` at run time (see
//! `ui::prompt`) instead of importing it, so those other executables still
//! load without the Common Controls 6 manifest.
//!
//! The VERSIONINFO resource is compiled from a generated `.rc` file with the
//! Windows SDK resource compiler (`rc.exe`, from `RC`, `PATH`, or the newest
//! Windows Kits 10 directory) and linked into the `esr` binary only
//! (`rustc-link-arg-bins`). The `.rc` has no manifest resource, so the
//! linker's embedded manifest stays the only one. A release build fails if
//! rc.exe is missing, because release artifacts must carry version info;
//! other builds print a warning and skip it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=esr.manifest");
    println!("cargo:rerun-if-env-changed=RC");

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

    let release = env::var("PROFILE").is_ok_and(|p| p == "release");
    match compile_version_resource() {
        Ok(res) => println!("cargo:rustc-link-arg-bins={}", res.display()),
        Err(e) if release => panic!("esr.exe VERSIONINFO resource: {e}"),
        Err(e) => println!("cargo:warning=skipping the VERSIONINFO resource: {e}"),
    }
}

/// Writes `$OUT_DIR\esr.rc`, compiles it to `$OUT_DIR\esr.res`, and returns that path.
fn compile_version_resource() -> Result<PathBuf, String> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| format!("OUT_DIR: {e}"))?);
    let part = |name: &str| -> Result<u32, String> {
        env::var(name)
            .map_err(|e| format!("{name}: {e}"))?
            .parse::<u32>()
            .map_err(|e| format!("{name}: {e}"))
    };
    let (major, minor, patch) = (
        part("CARGO_PKG_VERSION_MAJOR")?,
        part("CARGO_PKG_VERSION_MINOR")?,
        part("CARGO_PKG_VERSION_PATCH")?,
    );

    let rc_file = out_dir.join("esr.rc");
    let res_file = out_dir.join("esr.res");
    fs::write(&rc_file, version_rc(major, minor, patch))
        .map_err(|e| format!("cannot write {}: {e}", rc_file.display()))?;

    let rc = find_rc().ok_or("rc.exe (Windows SDK resource compiler) not found")?;
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status()
        .map_err(|e| format!("cannot run {}: {e}", rc.display()))?;
    if !status.success() {
        return Err(format!("{} failed with {status}", rc.display()));
    }
    Ok(res_file)
}

/// The `.rc` source: numeric constants only, so no `#include` is needed.
fn version_rc(major: u32, minor: u32, patch: u32) -> String {
    let comma = format!("{major},{minor},{patch},0");
    let dotted = format!("{major}.{minor}.{patch}.0");
    format!(
        r#"1 VERSIONINFO
FILEVERSION {comma}
PRODUCTVERSION {comma}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Ataraksea"
            VALUE "FileDescription", "Explorer Session Recall"
            VALUE "FileVersion", "{dotted}"
            VALUE "InternalName", "esr"
            VALUE "LegalCopyright", "Copyright (c) 2026 Ataraksea. MIT OR Apache-2.0"
            VALUE "OriginalFilename", "esr.exe"
            VALUE "ProductName", "Explorer Session Recall"
            VALUE "ProductVersion", "{dotted}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}

/// `RC`, then `rc.exe` on `PATH`, then the newest Windows Kits 10 version directory.
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(rc));
    }
    let on_path = Command::new("rc.exe")
        .arg("/?")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if on_path {
        return Some(PathBuf::from("rc.exe"));
    }
    let arch = match env::var("HOST").unwrap_or_default() {
        h if h.starts_with("aarch64") => "arm64",
        _ => "x64",
    };
    let kits = PathBuf::from(env::var_os("ProgramFiles(x86)")?).join(r"Windows Kits\10\bin");
    newest_kit_rc(&kits, arch)
}

fn newest_kit_rc(bin: &Path, arch: &str) -> Option<PathBuf> {
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for entry in fs::read_dir(bin).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("10.") {
            continue;
        }
        let Ok(version) = name.split('.').map(str::parse::<u32>).collect::<Result<Vec<_>, _>>() else {
            continue;
        };
        let rc = entry.path().join(arch).join("rc.exe");
        if rc.is_file() && best.as_ref().is_none_or(|(v, _)| version > *v) {
            best = Some((version, rc));
        }
    }
    best.map(|(_, rc)| rc)
}
