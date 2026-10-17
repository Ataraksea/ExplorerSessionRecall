//! `ExplorerOptions`: `PersistBrowsers`, `LaunchTo`, and `SeparateProcess`
//! under `HKCU\...\Explorer\Advanced`, plus the shell facts the runtime needs
//! to build an `esr_core::launch::LaunchToContext` (known folder paths and the
//! OS build).
//!
//! # Writing
//!
//! [`ExplorerOptions::set_persist_browsers`] writes only `PersistBrowsers`
//! (no other option is ever modified) and then broadcasts
//! `WM_SETTINGCHANGE` with `lParam = "ShellState"`. That is the notification
//! the shell itself sends when folder options change (the Folder Options
//! dialog and `SHGetSetSettings(.., TRUE)`), and the one Explorer windows
//! handle to re-read their `Advanced` settings. Explorer reads
//! `PersistBrowsers` itself at sign-in, so the broadcast is a courtesy that
//! keeps any open Folder Options view consistent; a failed or timed-out
//! broadcast does not fail the write.
//!
//! The broadcast uses `SendMessageTimeoutW(HWND_BROADCAST, .., SMTO_ABORTIFHUNG,
//! BROADCAST_TIMEOUT_MS)`. Hung windows are skipped, but the call can still
//! block for a while if many windows answer slowly, so call it from a worker
//! or a thread that tolerates a short stall.
//!
//! # Testability
//!
//! The registry subkey (under `HKEY_CURRENT_USER`) and whether to broadcast are
//! injectable via [`ExplorerOptions::with_subkey`], so tests use a scratch key
//! and never touch the user's real `Explorer\Advanced` values.
//!
//! Requirements 10.2, 10.5.

use esr_core::engine::{ExplorerOptionValues, IoErrorKind};
use esr_core::launch::LaunchToContext;
use esr_core::model::WideString;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DISK_FULL, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    ERROR_SHARING_VIOLATION, ERROR_SUCCESS, LPARAM, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::Shell::{
    FOLDERID_Downloads, FOLDERID_SkyDrive, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows::core::{GUID, PCWSTR, w};

/// `HKCU` subkey holding File Explorer's advanced options.
pub const ADVANCED_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
/// Builtin_Restore_Option ("Restore previous folder windows at logon").
pub const PERSIST_BROWSERS: &str = "PersistBrowsers";
/// "Open File Explorer to".
pub const LAUNCH_TO: &str = "LaunchTo";
/// "Launch folder windows in a separate process".
pub const SEPARATE_PROCESS: &str = "SeparateProcess";
/// Per-window timeout for the `WM_SETTINGCHANGE` broadcast.
pub const BROADCAST_TIMEOUT_MS: u32 = 1_000;

/// The `ExplorerOptions` adapter (backs the `Options` port in `esr-app`).
#[derive(Debug, Clone)]
pub struct ExplorerOptions {
    /// NUL-terminated `HKCU` subkey.
    subkey: Vec<u16>,
    broadcast: bool,
}

impl Default for ExplorerOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ExplorerOptions {
    /// The real `HKCU\...\Explorer\Advanced` key, broadcasting after writes.
    pub fn new() -> Self {
        Self::with_subkey(ADVANCED_SUBKEY, true)
    }

    /// Options stored under `HKCU\<subkey>`; `broadcast` controls whether
    /// [`set_persist_browsers`](Self::set_persist_browsers) sends
    /// `WM_SETTINGCHANGE("ShellState")` after writing.
    pub fn with_subkey(subkey: &str, broadcast: bool) -> Self {
        Self {
            subkey: to_wide_nul(subkey),
            broadcast,
        }
    }

    /// Reads all three options. Each is `None` when absent, not a
    /// `REG_DWORD`, or unreadable. Booleans are `true` for any non-zero DWORD.
    pub fn read(&self) -> ExplorerOptionValues {
        ExplorerOptionValues {
            persist_browsers: self.read_dword(PERSIST_BROWSERS).map(|v| v != 0),
            launch_to: self.read_dword(LAUNCH_TO),
            separate_process: self.read_dword(SEPARATE_PROCESS).map(|v| v != 0),
        }
    }

    /// Writes `PersistBrowsers` (1 or 0) as a `REG_DWORD`, creating the key if
    /// needed, and then broadcasts the setting change if enabled. Only the
    /// write's result is reported.
    pub fn set_persist_browsers(&self, enabled: bool) -> Result<(), IoErrorKind> {
        let value: u32 = enabled.into();
        let name = to_wide_nul(PERSIST_BROWSERS);
        // SAFETY: both strings are NUL-terminated and outlive the call; the
        // data pointer covers exactly 4 readable bytes.
        let err = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(self.subkey.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_DWORD.0,
                Some(&value as *const u32 as *const core::ffi::c_void),
                std::mem::size_of::<u32>() as u32,
            )
        };
        if err != ERROR_SUCCESS {
            return Err(io_error_kind(err));
        }
        if self.broadcast {
            let _ = broadcast_shell_state_change();
        }
        Ok(())
    }

    fn read_dword(&self, name: &str) -> Option<u32> {
        let name = to_wide_nul(name);
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: strings are NUL-terminated; `data`/`size` are valid out
        // pointers describing a 4-byte buffer. RRF_RT_REG_DWORD rejects every
        // other value type with ERROR_UNSUPPORTED_TYPE.
        let err = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(self.subkey.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut data as *mut u32 as *mut core::ffi::c_void),
                Some(&mut size),
            )
        };
        (err == ERROR_SUCCESS).then_some(data)
    }
}

/// Broadcasts `WM_SETTINGCHANGE(0, "ShellState")` to all top-level windows,
/// skipping hung ones. Returns whether the broadcast completed.
pub fn broadcast_shell_state_change() -> bool {
    let area = w!("ShellState");
    // SAFETY: `area` is a static NUL-terminated string valid for the call; the
    // result pointer is optional and not used.
    let result = unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(area.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            BROADCAST_TIMEOUT_MS,
            None,
        )
    };
    result.0 != 0
}

/// The path of a known folder (`SHGetKnownFolderPath`, `KF_FLAG_DEFAULT`), or
/// `None` when the folder is not set up (for example OneDrive when the user
/// never signed in to it).
pub fn known_folder_path(id: &GUID) -> Option<WideString> {
    // SAFETY: valid GUID pointer; no token (current user).
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: on success `path` is a NUL-terminated CoTaskMemAlloc'd string.
    let units = unsafe { path.as_wide() }.to_vec();
    // SAFETY: allocated by the shell with the COM task allocator; freed once.
    unsafe { CoTaskMemFree(Some(path.0 as *const core::ffi::c_void)) };
    (!units.is_empty()).then_some(WideString(units))
}

/// `FOLDERID_Downloads`.
pub fn downloads_path() -> Option<WideString> {
    known_folder_path(&FOLDERID_Downloads)
}

/// OneDrive (`FOLDERID_SkyDrive`).
pub fn onedrive_path() -> Option<WideString> {
    known_folder_path(&FOLDERID_SkyDrive)
}

/// The OS build number from `RtlGetVersion` (not subject to the
/// compatibility shims of `GetVersionEx`).
pub fn os_build() -> Option<u32> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid, writable OSVERSIONINFOW with its size set.
    unsafe { RtlGetVersion(&mut info) }.ok().ok()?;
    Some(info.dwBuildNumber)
}

/// Everything the core needs to resolve `LaunchTo`. `build` is 0 if the OS
/// version cannot be read (the LaunchTo table then uses its pre-26100 rows).
pub fn launch_to_context() -> LaunchToContext {
    LaunchToContext {
        build: os_build().unwrap_or(0),
        downloads: downloads_path(),
        onedrive: onedrive_path(),
    }
}

/// Maps a registry Win32 error to the core's `IoErrorKind`.
fn io_error_kind(err: WIN32_ERROR) -> IoErrorKind {
    match err {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => IoErrorKind::NotFound,
        ERROR_ACCESS_DENIED => IoErrorKind::AccessDenied,
        ERROR_SHARING_VIOLATION => IoErrorKind::SharingViolation,
        ERROR_DISK_FULL => IoErrorKind::DiskFull,
        other => IoErrorKind::Other(other.0),
    }
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::System::Registry::{REG_BINARY, REG_QWORD, REG_SZ, RegDeleteTreeW};

    /// A scratch key `HKCU\Software\ESRTest_<pid>_<n>`, deleted (with its
    /// values) on drop. Never the real `Explorer\Advanced` key.
    struct ScratchKey {
        path: String,
    }

    impl ScratchKey {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = format!(r"Software\ESRTest_{}_opts{n}", std::process::id());
            assert!(path.starts_with(r"Software\ESRTest_"));
            let key = Self { path };
            key.delete(); // leftovers from a crashed run with a reused pid
            key
        }

        /// Options on this key, never broadcasting.
        fn options(&self) -> ExplorerOptions {
            ExplorerOptions::with_subkey(&self.path, false)
        }

        fn set_raw(&self, name: &str, kind: u32, data: &[u8]) {
            let subkey = to_wide_nul(&self.path);
            let name = to_wide_nul(name);
            // SAFETY: NUL-terminated strings; `data` is readable for its length.
            let err = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey.as_ptr()),
                    PCWSTR(name.as_ptr()),
                    kind,
                    Some(data.as_ptr().cast()),
                    data.len() as u32,
                )
            };
            assert_eq!(err, ERROR_SUCCESS);
        }

        fn set_dword(&self, name: &str, value: u32) {
            self.set_raw(name, REG_DWORD.0, &value.to_le_bytes());
        }

        fn delete(&self) {
            let subkey = to_wide_nul(&self.path);
            // SAFETY: NUL-terminated scratch subkey; deletes it and its values.
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr())) };
        }
    }

    impl Drop for ScratchKey {
        fn drop(&mut self) {
            self.delete();
        }
    }

    #[test]
    fn absent_key_reads_all_none() {
        let key = ScratchKey::new();
        assert_eq!(key.options().read(), ExplorerOptionValues::default());
    }

    #[test]
    fn present_dwords_are_read() {
        let key = ScratchKey::new();
        key.set_dword(PERSIST_BROWSERS, 1);
        key.set_dword(LAUNCH_TO, 3);
        key.set_dword(SEPARATE_PROCESS, 0);
        assert_eq!(
            key.options().read(),
            ExplorerOptionValues {
                persist_browsers: Some(true),
                launch_to: Some(3),
                separate_process: Some(false),
            }
        );
        // Any non-zero DWORD is true.
        key.set_dword(SEPARATE_PROCESS, 7);
        assert_eq!(key.options().read().separate_process, Some(true));
    }

    #[test]
    fn wrong_types_read_as_none() {
        let key = ScratchKey::new();
        let sz: Vec<u8> = "1\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        key.set_raw(PERSIST_BROWSERS, REG_SZ.0, &sz);
        key.set_raw(LAUNCH_TO, REG_QWORD.0, &3u64.to_le_bytes());
        key.set_raw(SEPARATE_PROCESS, REG_BINARY.0, &1u32.to_le_bytes());
        assert_eq!(key.options().read(), ExplorerOptionValues::default());
    }

    #[test]
    fn set_persist_browsers_round_trips_and_touches_nothing_else() {
        let key = ScratchKey::new();
        key.set_dword(LAUNCH_TO, 2);
        key.set_dword(SEPARATE_PROCESS, 1);
        let opts = key.options();

        opts.set_persist_browsers(true).expect("write true");
        assert_eq!(
            opts.read(),
            ExplorerOptionValues {
                persist_browsers: Some(true),
                launch_to: Some(2),
                separate_process: Some(true),
            }
        );
        opts.set_persist_browsers(false).expect("write false");
        assert_eq!(opts.read().persist_browsers, Some(false));
        assert_eq!(opts.read().launch_to, Some(2));
        assert_eq!(opts.read().separate_process, Some(true));
    }

    #[test]
    fn set_persist_browsers_replaces_a_wrong_type_value() {
        let key = ScratchKey::new();
        key.set_raw(PERSIST_BROWSERS, REG_SZ.0, &[b'x', 0, 0, 0]);
        key.options().set_persist_browsers(true).expect("write");
        assert_eq!(key.options().read().persist_browsers, Some(true));
    }

    #[test]
    fn real_key_is_readable() {
        // Read only; the real values are never written by tests.
        let values = ExplorerOptions::new().read();
        if let Some(v) = values.launch_to {
            assert!(v < 1_000, "implausible LaunchTo {v}");
        }
    }

    #[test]
    fn error_kinds_map() {
        assert_eq!(io_error_kind(ERROR_FILE_NOT_FOUND), IoErrorKind::NotFound);
        assert_eq!(io_error_kind(ERROR_PATH_NOT_FOUND), IoErrorKind::NotFound);
        assert_eq!(
            io_error_kind(ERROR_ACCESS_DENIED),
            IoErrorKind::AccessDenied
        );
        assert_eq!(
            io_error_kind(ERROR_SHARING_VIOLATION),
            IoErrorKind::SharingViolation
        );
        assert_eq!(io_error_kind(ERROR_DISK_FULL), IoErrorKind::DiskFull);
        assert_eq!(io_error_kind(WIN32_ERROR(1234)), IoErrorKind::Other(1234));
    }

    #[test]
    fn known_folders_resolve() {
        let downloads = downloads_path()
            .expect("Downloads resolves")
            .to_string_lossy();
        let absolute = (downloads.len() > 3 && downloads.as_bytes()[1] == b':')
            || downloads.starts_with(r"\\");
        assert!(absolute, "not an absolute path: {downloads}");
        assert!(
            !downloads.ends_with('\\') && !downloads.contains('\0'),
            "{downloads}"
        );
        // OneDrive may not be configured; when it is, it is a non-empty path.
        if let Some(onedrive) = onedrive_path() {
            assert!(!onedrive.0.is_empty() && !onedrive.0.contains(&0));
        }
    }

    #[test]
    fn os_build_and_context() {
        let build = os_build().expect("RtlGetVersion");
        assert!(build >= 10_240, "build {build}");
        let ctx = launch_to_context();
        assert_eq!(ctx.build, build);
        assert_eq!(ctx.downloads, downloads_path());
        assert_eq!(ctx.onedrive, onedrive_path());
    }
}
