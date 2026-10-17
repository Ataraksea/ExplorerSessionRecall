//! Version gate: `RtlGetVersion` + `UBR`, supported from build 22621.675.
//!
//! File Explorer tabs shipped in build 22621.675 (KB5019509, October 2022);
//! plain 22621 RTM has no tabs (Requirements 1.2, 1.3). The decision is the pure
//! [`is_supported`]; [`read_os_version`] and [`show_unsupported_message`] are
//! thin Windows wrappers, and [`check_or_notify`] combines them for startup.
//!
//! The gate runs before the Session_File is opened, so an unsupported build
//! exits without touching it (Req 1.3).

use std::cmp::Ordering;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
};
use windows::core::{HSTRING, w};

/// First build number of Windows 11 version 22H2.
pub const MIN_BUILD: u32 = 22621;

/// First update build revision (UBR) of 22621 that has File Explorer tabs.
pub const MIN_UBR: u32 = 675;

/// The minimum supported version, as named in the unsupported message.
pub const MINIMUM_VERSION_TEXT: &str = "Windows 11 version 22H2, build 22621.675 or later";

/// Caption of the unsupported message box.
const MESSAGE_CAPTION: &str = "Explorer Session Restore";

/// OS version as read from `RtlGetVersion` and the `UBR` registry value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    /// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\UBR`; `None` when
    /// the value is absent or unreadable.
    pub ubr: Option<u32>,
}

impl OsVersion {
    /// Whether this version passes the gate.
    pub fn is_supported(&self) -> bool {
        is_supported(self.build, self.ubr)
    }
}

/// Pure version-gate decision (Req 1.2, 1.3).
///
/// * `build > 22621` (e.g. 22631, 26100): supported. Every later build has tabs,
///   so the UBR is irrelevant and may be missing.
/// * `build == 22621`: supported only when `ubr >= 675`. A missing UBR is
///   treated as **unsupported**: without it the gate cannot confirm that tabs
///   exist, and running against a tab-less Explorer would break the tracking
///   and restore guarantees, whereas a refusal is visible and harmless.
/// * `build < 22621` (Windows 11 21H2 = 22000, Windows 10 < 22000): unsupported.
pub fn is_supported(build: u32, ubr: Option<u32>) -> bool {
    match build.cmp(&MIN_BUILD) {
        Ordering::Greater => true,
        Ordering::Equal => ubr.is_some_and(|ubr| ubr >= MIN_UBR),
        Ordering::Less => false,
    }
}

/// Body of the unsupported message. Always names the minimum version and,
/// when known, the detected build.
pub fn unsupported_message(detected: Option<OsVersion>) -> String {
    let mut text = format!("Explorer Session Restore requires {MINIMUM_VERSION_TEXT}.");
    match detected {
        Some(OsVersion { build, ubr: Some(ubr), .. }) => {
            text.push_str(&format!("\n\nThis PC is running build {build}.{ubr}."));
        }
        Some(OsVersion { build, ubr: None, .. }) => {
            text.push_str(&format!("\n\nThis PC is running build {build}."));
        }
        None => {}
    }
    text
}

/// Reads the OS version via `RtlGetVersion` (not subject to the
/// compatibility shims of `GetVersionEx`) and the `UBR` registry value.
pub fn read_os_version() -> windows::core::Result<OsVersion> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid, writable OSVERSIONINFOW with its size field set.
    unsafe { RtlGetVersion(&mut info) }.ok()?;
    Ok(OsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
        ubr: read_ubr(),
    })
}

/// Reads `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\UBR` as a DWORD.
/// Any failure (absent, wrong type, access denied) yields `None`.
fn read_ubr() -> Option<u32> {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: the data pointer and size describe a valid, writable u32.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("UBR"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// Shows the modal unsupported-build message and returns once the user
/// dismisses it. The caller exits immediately afterwards (Req 1.3).
pub fn show_unsupported_message(detected: Option<OsVersion>) {
    let text = HSTRING::from(unsupported_message(detected));
    let caption = HSTRING::from(MESSAGE_CAPTION);
    // SAFETY: both strings are valid, NUL-terminated, and outlive the call.
    unsafe {
        MessageBoxW(
            None,
            &text,
            &caption,
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// Startup gate: returns `true` when the build is supported. Otherwise shows
/// the unsupported message, waits for dismissal, and returns `false`; the
/// caller must then exit without opening the Session_File.
///
/// If the version cannot be read at all, support cannot be confirmed, so the
/// build is treated as unsupported.
pub fn check_or_notify() -> bool {
    let detected = read_os_version().ok();
    if detected.is_some_and(|v| v.is_supported()) {
        return true;
    }
    show_unsupported_message(detected);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_build_boundary() {
        assert!(!is_supported(22621, Some(674)));
        assert!(is_supported(22621, Some(675)));
        assert!(is_supported(22621, Some(3880)));
    }

    #[test]
    fn missing_ubr_on_22621_is_unsupported() {
        assert!(!is_supported(22621, None));
    }

    #[test]
    fn later_builds_are_supported_regardless_of_ubr() {
        assert!(is_supported(22622, None));
        assert!(is_supported(22631, Some(0)));
        assert!(is_supported(26100, None));
    }

    #[test]
    fn earlier_builds_are_unsupported() {
        assert!(!is_supported(22000, Some(9999))); // Windows 11 21H2
        assert!(!is_supported(19045, Some(4000))); // Windows 10 22H2
        assert!(!is_supported(0, None));
    }

    #[test]
    fn message_names_minimum_version() {
        let detected = OsVersion { major: 10, minor: 0, build: 22000, ubr: Some(2538) };
        let text = unsupported_message(Some(detected));
        assert!(text.contains(MINIMUM_VERSION_TEXT));
        assert!(text.contains("22000.2538"));
        assert!(unsupported_message(None).contains(MINIMUM_VERSION_TEXT));
    }

    /// Reads (never displays anything) the real OS version of the test host.
    #[test]
    fn read_os_version_returns_plausible_values() {
        let v = read_os_version().expect("RtlGetVersion");
        println!("detected {}.{}.{}.{:?}", v.major, v.minor, v.build, v.ubr);
        assert_eq!(v.major, 10, "Windows 10/11 report major version 10");
        assert!(v.build >= 10240, "build {} below Windows 10 RTM", v.build);
        assert!(v.ubr.is_some(), "UBR should be readable on Windows 10/11");
    }
}
