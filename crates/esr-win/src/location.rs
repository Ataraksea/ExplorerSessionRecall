//! PIDL to `LocationId` capture and location kind detection
//! (Requirements 8.1, 8.2, 8.4, 8.5, 8.7, 8.8).
//!
//! # Capture
//!
//! [`capture_from_pidl`] turns an absolute ITEMIDLIST (as returned by
//! [`current_folder_pidl`](crate::shell_util::current_folder_pidl)) into an
//! [`ObservedLocation`]:
//!
//! - **Parsing name**: `SHGetNameFromIDList(SIGDN_DESKTOPABSOLUTEPARSING)`, the
//!   desktop-absolute `SHGDN_FORPARSING` name. It is stored exactly as the Shell
//!   returns it, up to [`MAX_WIDE_LEN`] UTF-16 units: no case folding, no
//!   Unicode normalisation, no `\\?\` handling, and mapped drives keep their
//!   drive letter (never translated to UNC, Req 8.4, 8.7). The one exception:
//!   above `MAX_PATH` the Shell reports 8.3 short names for file-system
//!   components, which [`expand_short_names`] replaces with the names on disk
//!   (Req 8.4). Newer builds (measured on 26300) instead report such a path
//!   with a `\\?\` prefix and long names; it is stored as reported, and
//!   [`classify_kind`] accepts both forms, so nothing relies on the prefix
//!   being absent.
//! - **Display name**: `SIGDN_NORMALDISPLAY`. This is the name Explorer shows in
//!   the window title and in the tab header (`IWebBrowser2::LocationName`), and
//!   it is what UIA reports as the tab name on 22621. Measured on 22621: Home,
//!   This PC, Downloads, `C:\Windows`, Recycle Bin, Control Panel, libraries and
//!   ZIP folders give the same text in `NORMALDISPLAY`, `PARENTRELATIVE` and
//!   `PARENTRELATIVEEDITING` (an archive shows as `Archive` with extensions
//!   hidden). They differ for drive roots and share roots: `C:\` is
//!   `Windows (C:)` (volume label and letter, as in the tab header) but
//!   `Windows` when editing, and `\\localhost\C$` is `C$ (\\localhost)` but
//!   `C$` when editing. A failure to read the display name leaves it empty
//!   (the tracker can still match by parsing name); a display name longer
//!   than [`MAX_WIDE_LEN`] is truncated.
//! - **PIDL bytes**: the input, kept as the fallback binding.
//! - **Kind**: [`classify_kind`], with `GetDriveTypeW` and `GetFileAttributesW`.
//! - **Search**: a `search-ms:` parsing name is parsed with
//!   [`parse_search_ms`]. A search-results tab's own PIDL (Explorer's native
//!   search folder) does *not* have one: on 22621 its
//!   `SIGDN_DESKTOPABSOLUTEPARSING` name is a display name such as
//!   `Search Results in Downloads` (which classifies as `Shell(Other)`), and
//!   the `search-ms:` URL is its `SIGDN_DESKTOPABSOLUTEEDITING` name. So for
//!   a `Shell(Other)` parsing name the editing name is read as well, and a
//!   `search-ms:` editing name becomes the stored parsing name
//!   ([`search_url_for`]). Its scope parsing name is bound with
//!   `SHParseDisplayName` and captured recursively (at most
//!   [`MAX_SCOPE_DEPTH`] levels), so the scope gets its own kind, display
//!   name, PIDL and *canonical* parsing name (for example `shell:Personal`
//!   becomes the Documents path). A scope that cannot be bound makes the whole
//!   search unidentifiable. `search-ms:` names are always bound with
//!   [`parse_bind_ctx`], which yields the
//!   native search-folder PIDL rather than an Internet-URL item.
//!
//! Anything that fails (malformed PIDL bytes, no parsing name, a parsing name
//! longer than [`MAX_WIDE_LEN`], an unparsable `search-ms:` URL, an unbindable
//! scope) is reported as [`ObservedLocation::Unidentifiable`] (Req 8.8). The
//! `try_` variants return the reason.
//!
//! All capture functions call the Shell and must run on a COM-initialised
//! thread (the scanner's STA).
//!
//! # Kind detection ([`classify_kind`], pure)
//!
//! | Parsing name | Kind |
//! |---|---|
//! | `search-ms:…` (ASCII case-insensitive) | `Search` |
//! | `::{20D04FE0-…}` (This PC) alone | `Shell(ThisPc)` |
//! | `::{20D04FE0-…}\\\?\…` (WPD device path), `…\::{35786D3C-…}…`, `::{35786D3C-…}…` | `Shell(PortableDevice)` |
//! | `::{20D04FE0-…}\::{21EC2020-…}…` / `…\::{26EE0668-…}…` | `Shell(ControlPanel)` |
//! | `::{F874310E-…}…` | `Shell(Home)` |
//! | `::{E88865EA-…}…` | `Shell(Gallery)` |
//! | `::{645FF040-…}…` | `Shell(RecycleBin)` |
//! | `::{F02C1A0D-…}…`, `::{208D2C60-…}…`, `\\server` without a share | `Shell(Network)` |
//! | `::{031E4825-…}…`, or a path whose last component ends in `.library-ms` | `Shell(Library)` |
//! | a path whose last component ends in `.search-ms` or `.searchConnector-ms` | `Shell(SavedSearch)` |
//! | `::{26EE0668-…}…`, `::{21EC2020-…}…` | `Shell(ControlPanel)` |
//! | any other `::{…}` or unrecognised form | `Shell(Other)` |
//! | path below a component ending in `.zip` that is a file | `ZipFolder` |
//! | `X:\…` / `\\?\X:\…` | `FileSystem(v)`, `v` from the drive type of `X:\` |
//! | `\\?\Volume{…}\…` | `FileSystem(v)`, `v` from the drive type of the volume root |
//! | `\\server\share…` / `\\?\UNC\server\share…` | `FileSystem(Unc)` |
//!
//! Drive types: `DRIVE_REMOTE` → `Mapped` (the drive letter is kept, Req 8.7),
//! `DRIVE_REMOVABLE` and `DRIVE_CDROM` → `Removable`, anything else → `Fixed`.
//! GUIDs are compared ASCII case-insensitively.
//!
//! The `.zip` archive itself (`C:\a\b.zip`) is a `FileSystem` Location; only
//! folders *inside* it are `ZipFolder`. This matches
//! [`ancestors`](esr_core::plan::fallback::ancestors), where the `.zip` folder
//! and everything above it are file-system ancestors, and the prober, which
//! treats an existing `.zip` file as an available file-system folder. A folder
//! that merely has a `.zip` name is not an archive, which is why the archive
//! component must be a file.
//!
//! A saved search (`.search-ms` file, which Explorer browses as a folder of
//! results) is `Shell(SavedSearch)`, not `FileSystem`: like a live search it
//! is never redirected to an ancestor folder, so a saved search whose file is
//! gone is omitted (Req 9.2) rather than replaced by the folder that held it.

use std::ffi::c_void;

use esr_core::engine::ObservedLocation;
use esr_core::model::{
    LocationId, LocationKind, MAX_WIDE_LEN, SearchSpec, ShellKind, Volume, WideString,
};
use esr_core::search::{SearchParseError, is_search_ms, parse_search_ms};
use windows::Wdk::Storage::FileSystem::{
    FILE_CASE_SENSITIVE_INFORMATION, FileCaseSensitiveInformation, NtQueryInformationFile,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FindClose, FindFirstFileW,
    GetDriveTypeW, GetFileAttributesW, GetVolumeInformationW, INVALID_FILE_ATTRIBUTES,
    OPEN_EXISTING, WIN32_FIND_DATAW,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SHGetNameFromIDList, SHParseDisplayName, SIGDN, SIGDN_DESKTOPABSOLUTEEDITING,
    SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY,
};
use windows::core::PCWSTR;

use crate::shell_util::{parse_bind_ctx, pidl_to_bytes};

/// Deepest chain of search scopes followed (a search scoped to a search …).
pub const MAX_SCOPE_DEPTH: usize = 4;

/// `GetDriveTypeW` results (`Win32_System_WindowsProgramming`).
pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
pub const DRIVE_CDROM: u32 = 5;

/// Shell folder CLSIDs recognised in `::{…}` parsing names.
pub mod clsid {
    pub const THIS_PC: &str = "20D04FE0-3AEA-1069-A2D8-08002B30309D";
    pub const HOME: &str = "F874310E-B6B7-47DC-BC84-B9E6B38F5903";
    pub const GALLERY: &str = "E88865EA-0E1C-4E20-9AA6-EDCD0212C87C";
    pub const RECYCLE_BIN: &str = "645FF040-5081-101B-9F08-00AA002F954E";
    pub const NETWORK: &str = "F02C1A0D-BE21-4350-88B0-7367FC96EF3C";
    /// Legacy "My Network Places".
    pub const NETWORK_LEGACY: &str = "208D2C60-3AEA-1069-A2D7-08002B30309D";
    pub const LIBRARIES: &str = "031E4825-7B94-4DC3-B131-E946B44C8DD5";
    /// Control Panel (category view).
    pub const CONTROL_PANEL: &str = "26EE0668-A00A-44D7-9371-BEB064C98683";
    /// All Control Panel Items.
    pub const CONTROL_PANEL_ALL: &str = "21EC2020-3AEA-1069-A2DD-08002B30309D";
    /// The "Portable Devices" folder.
    pub const PORTABLE_DEVICES: &str = "35786D3C-B075-49B9-88DD-029876E11C01";
}

/// Why a Location could not be identified (logged as `UNIDENTIFIABLE-LOCATION`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// The bytes are not a terminated ITEMIDLIST.
    MalformedPidl,
    /// The Shell could not produce a parsing name.
    NoParsingName(windows::core::Error),
    /// The parsing name is empty.
    EmptyParsingName,
    /// The parsing name exceeds [`MAX_WIDE_LEN`] UTF-16 units.
    TooLong { len: usize },
    /// A parsing name to bind is empty or contains a NUL.
    InvalidName,
    /// `SHParseDisplayName` failed for a parsing name (e.g. a search scope).
    Unbindable(windows::core::Error),
    /// The `search-ms:` URL cannot be represented (Req 8.8).
    Search(SearchParseError),
    /// Search scopes nested deeper than [`MAX_SCOPE_DEPTH`].
    ScopeTooDeep,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::MalformedPidl => write!(f, "malformed item ID list"),
            CaptureError::NoParsingName(e) => write!(f, "no parsing name: {e}"),
            CaptureError::EmptyParsingName => write!(f, "empty parsing name"),
            CaptureError::TooLong { len } => {
                write!(
                    f,
                    "parsing name has {len} UTF-16 units (limit {MAX_WIDE_LEN})"
                )
            }
            CaptureError::InvalidName => write!(f, "parsing name is empty or contains NUL"),
            CaptureError::Unbindable(e) => write!(f, "parsing name cannot be bound: {e}"),
            CaptureError::Search(e) => write!(f, "unsupported search: {e}"),
            CaptureError::ScopeTooDeep => write!(f, "search scopes nested too deeply"),
        }
    }
}

impl std::error::Error for CaptureError {}

// ---------------------------------------------------------------------------
// Capture (Shell)
// ---------------------------------------------------------------------------

/// Captures the Location of an absolute PIDL (bytes including the terminator).
/// Any failure is [`ObservedLocation::Unidentifiable`] (Req 8.8).
pub fn capture_from_pidl(pidl: &[u8]) -> ObservedLocation {
    observed(try_capture_from_pidl(pidl))
}

/// [`capture_from_pidl`] with the failure reason.
pub fn try_capture_from_pidl(pidl: &[u8]) -> Result<LocationId, CaptureError> {
    capture_pidl_at_depth(pidl, 0)
}

/// Binds a parsing name with `SHParseDisplayName` and captures the result.
/// The stored parsing name is the one the Shell reports for the bound item.
pub fn capture_from_parsing_name(parsing_name: &WideString) -> ObservedLocation {
    observed(try_capture_from_parsing_name(parsing_name))
}

/// [`capture_from_parsing_name`] with the failure reason.
pub fn try_capture_from_parsing_name(
    parsing_name: &WideString,
) -> Result<LocationId, CaptureError> {
    capture_name_at_depth(parsing_name.units(), 0)
}

fn observed(r: Result<LocationId, CaptureError>) -> ObservedLocation {
    match r {
        Ok(l) => ObservedLocation::Known(l),
        Err(_) => ObservedLocation::Unidentifiable,
    }
}

fn capture_name_at_depth(name: &[u16], depth: usize) -> Result<LocationId, CaptureError> {
    let pidl = parse_display_name(name)?;
    capture_pidl_at_depth(&pidl, depth)
}

fn capture_pidl_at_depth(pidl: &[u8], depth: usize) -> Result<LocationId, CaptureError> {
    if !pidl_is_well_formed(pidl) {
        return Err(CaptureError::MalformedPidl);
    }
    let parsing_name =
        name_of(pidl, SIGDN_DESKTOPABSOLUTEPARSING).map_err(CaptureError::NoParsingName)?;
    let parsing_name = WideString(expand_short_names(parsing_name.units(), long_name));
    if parsing_name.is_empty() {
        return Err(CaptureError::EmptyParsingName);
    }
    // A search-results tab reports a display name as its parsing name; its
    // `search-ms:` URL is the editing name (see the module docs).
    let plain_kind = (!is_search_ms(&parsing_name))
        .then(|| classify_kind(parsing_name.units(), drive_type, is_file));
    let parsing_name = match plain_kind {
        Some(k) => search_url_for(k, || name_of(pidl, SIGDN_DESKTOPABSOLUTEEDITING).ok())
            .unwrap_or(parsing_name),
        None => parsing_name,
    };
    if parsing_name.len() > MAX_WIDE_LEN {
        return Err(CaptureError::TooLong {
            len: parsing_name.len(),
        });
    }
    let mut display_name = name_of(pidl, SIGDN_NORMALDISPLAY).unwrap_or_default();
    display_name.0.truncate(MAX_WIDE_LEN);

    let (kind, search) = if is_search_ms(&parsing_name) {
        let parsed = parse_search_ms(&parsing_name).map_err(CaptureError::Search)?;
        if depth >= MAX_SCOPE_DEPTH {
            return Err(CaptureError::ScopeTooDeep);
        }
        let scope = capture_name_at_depth(parsed.scope.units(), depth + 1)?;
        let spec = SearchSpec {
            query: parsed.query,
            scope: Box::new(scope),
        };
        (LocationKind::Search, Some(spec))
    } else {
        (
            plain_kind.unwrap_or_else(|| classify_kind(parsing_name.units(), drive_type, is_file)),
            None,
        )
    };
    Ok(LocationId {
        case_sensitive: detect_case_sensitive(&SystemCaseProbe, kind, parsing_name.units()),
        kind,
        parsing_name,
        display_name,
        pidl: pidl.to_vec(),
        search,
    })
}

/// The `search-ms:` URL to record for a PIDL whose parsing name classified
/// as `kind`, or `None` to keep the parsing name.
///
/// On 22621 a search-results tab's PIDL (the native search folder) reports a
/// *display name* such as `Search Results in Downloads` as its
/// `SIGDN_DESKTOPABSOLUTEPARSING` name, which classifies as `Shell(Other)`.
/// Its `SIGDN_DESKTOPABSOLUTEEDITING` name is the `search-ms:` URL. Only for
/// `Shell(Other)` is `editing_name` asked (one extra Shell call), and only a
/// `search-ms:` result is used.
pub fn search_url_for(
    kind: LocationKind,
    editing_name: impl FnOnce() -> Option<WideString>,
) -> Option<WideString> {
    if kind != LocationKind::Shell(ShellKind::Other) {
        return None;
    }
    editing_name().filter(is_search_ms)
}

/// A terminated ITEMIDLIST: every `cb` (≥ 3, i.e. at least one payload byte)
/// stays in bounds and the 2-byte zero terminator is the last thing in the
/// buffer. The desktop PIDL (terminator only) is accepted.
pub fn pidl_is_well_formed(bytes: &[u8]) -> bool {
    let mut at = 0usize;
    loop {
        let Some(cb) = bytes.get(at..at + 2) else {
            return false;
        };
        let cb = usize::from(u16::from_le_bytes([cb[0], cb[1]]));
        if cb == 0 {
            return at + 2 == bytes.len();
        }
        if cb < 3 {
            return false;
        }
        match at.checked_add(cb) {
            Some(next) if next <= bytes.len() => at = next,
            _ => return false,
        }
    }
}

/// `SHGetNameFromIDList` on a well-formed PIDL.
fn name_of(pidl: &[u8], sigdn: SIGDN) -> windows::core::Result<WideString> {
    // SAFETY: `pidl` is a terminated ITEMIDLIST (checked by the caller);
    // ITEMIDLIST is packed(1), so any alignment is fine.
    let name = unsafe { SHGetNameFromIDList(pidl.as_ptr().cast::<ITEMIDLIST>(), sigdn)? };
    if name.is_null() {
        return Err(windows::Win32::Foundation::E_POINTER.into());
    }
    // SAFETY: on success the Shell returns a NUL-terminated string.
    let units = unsafe { name.as_wide() }.to_vec();
    // SAFETY: allocated by the Shell with the COM task allocator; freed once.
    unsafe { CoTaskMemFree(Some(name.0.cast_const().cast::<c_void>())) };
    Ok(WideString(units))
}

/// `SHParseDisplayName` → absolute PIDL bytes. A `search-ms:` URL is parsed
/// with [`parse_bind_ctx`] (native search folder, not an Internet-URL item).
fn parse_display_name(name: &[u16]) -> Result<Vec<u8>, CaptureError> {
    if name.is_empty() || name.contains(&0) {
        return Err(CaptureError::InvalidName);
    }
    let mut z = name.to_vec();
    z.push(0);
    let ctx = parse_bind_ctx(name);
    let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: `z` is NUL-terminated; `out` is a valid out pointer.
    let r = unsafe { SHParseDisplayName(PCWSTR(z.as_ptr()), ctx.as_ref(), &mut out, 0, None) };
    let bytes = if out.is_null() {
        None
    } else {
        // SAFETY: a non-null result is a valid PIDL owned by us; freed once below.
        let bytes = unsafe { pidl_to_bytes(out) };
        // SAFETY: allocated by SHParseDisplayName with the COM task allocator.
        unsafe { CoTaskMemFree(Some(out.cast_const().cast::<c_void>())) };
        Some(bytes)
    };
    r.map_err(CaptureError::Unbindable)?;
    bytes.ok_or(CaptureError::Unbindable(
        windows::Win32::Foundation::E_POINTER.into(),
    ))
}

/// `GetDriveTypeW` on a root such as `C:\` or `\\?\Volume{…}\`.
pub fn drive_type(root: &[u16]) -> u32 {
    let mut z = root.to_vec();
    z.push(0);
    // SAFETY: `z` is NUL-terminated.
    unsafe { GetDriveTypeW(PCWSTR(z.as_ptr())) }
}

/// Whether `path` (absolute, drive or UNC form) names an existing file, not a
/// folder. Uses the `\\?\` form so paths longer than `MAX_PATH` work.
pub fn is_file(path: &[u16]) -> bool {
    let mut z = verbatim_form(path);
    z.push(0);
    // SAFETY: `z` is NUL-terminated.
    let attrs = unsafe { GetFileAttributesW(PCWSTR(z.as_ptr())) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_DIRECTORY.0 == 0
}

/// The on-disk (long) name of the last component of `path`, via
/// `FindFirstFileW` on the `\\?\` form. `None` if it cannot be found.
pub fn long_name(path: &[u16]) -> Option<Vec<u16>> {
    let mut z = verbatim_form(path);
    z.push(0);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `z` is NUL-terminated and `data` is a valid out buffer.
    let handle = unsafe { FindFirstFileW(PCWSTR(z.as_ptr()), &mut data) }.ok()?;
    // SAFETY: `handle` is a valid search handle; closed once.
    let _ = unsafe { FindClose(handle) };
    let len = data
        .cFileName
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(data.cFileName.len());
    (len > 0).then(|| data.cFileName[..len].to_vec())
}

/// `X:\…` → `\\?\X:\…`, `\\server\…` → `\\?\UNC\server\…`; other forms as is.
/// Used only for the file-system call, never for what is stored.
fn verbatim_form(path: &[u16]) -> Vec<u16> {
    if starts_with(path, VERBATIM) || starts_with(path, DEVICE) {
        return path.to_vec();
    }
    let mut out: Vec<u16> = VERBATIM.encode_utf16().collect();
    if starts_with(path, r"\\") {
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(&path[2..]);
    } else if is_drive(path) {
        out.extend_from_slice(path);
    } else {
        return path.to_vec();
    }
    out
}

// ---------------------------------------------------------------------------
// Case sensitivity (Req 14.20)
// ---------------------------------------------------------------------------

/// `GetVolumeInformationW` file-system flag: the volume supports per-directory
/// case sensitivity (`FILE_SUPPORTS_CASE_SENSITIVE_DIRS`).
pub const FILE_SUPPORTS_CASE_SENSITIVE_DIRS: u32 = 0x0000_2000;

/// `FILE_CASE_SENSITIVE_INFORMATION::Flags`: the directory is case sensitive
/// (`FILE_CS_FLAG_CASE_SENSITIVE_DIR`).
pub const FILE_CS_FLAG_CASE_SENSITIVE_DIR: u32 = 1;

/// The two file-system questions behind [`detect_case_sensitive`], so that the
/// decision can be tested without a case-sensitive directory.
pub trait CaseProbe {
    /// The `GetVolumeInformationW` flags of the volume with root `root`
    /// (`C:\`, `\\server\share\`, `\\?\Volume{…}\`), or `None` on failure.
    fn volume_flags(&self, root: &[u16]) -> Option<u32>;

    /// Whether the directory `dir` is case sensitive
    /// (`NtQueryInformationFile(FileCaseSensitiveInformation)`); `None` when
    /// it cannot be asked (missing, access denied, unsupported).
    fn directory_case_sensitive(&self, dir: &[u16]) -> Option<bool>;
}

/// [`CaseProbe`] on the real file system.
pub struct SystemCaseProbe;

impl CaseProbe for SystemCaseProbe {
    fn volume_flags(&self, root: &[u16]) -> Option<u32> {
        let mut z = verbatim_form(root);
        z.push(0);
        let mut flags = 0u32;
        // SAFETY: `z` is NUL-terminated and `flags` is a valid out pointer; the
        // optional buffers are omitted.
        unsafe {
            GetVolumeInformationW(PCWSTR(z.as_ptr()), None, None, None, Some(&mut flags), None)
        }
        .ok()?;
        Some(flags)
    }

    fn directory_case_sensitive(&self, dir: &[u16]) -> Option<bool> {
        let mut z = verbatim_form(dir);
        z.push(0);
        // SAFETY: `z` is NUL-terminated. Attribute access only, shared with
        // everybody; `BACKUP_SEMANTICS` is required to open a directory.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(z.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .ok()?;
        let answer = query_case_flags(handle);
        // SAFETY: `handle` was opened above and is closed once.
        let _ = unsafe { CloseHandle(handle) };
        answer.map(|flags| flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
    }
}

/// `NtQueryInformationFile(FileCaseSensitiveInformation)` on an open handle.
fn query_case_flags(handle: HANDLE) -> Option<u32> {
    let mut info = FILE_CASE_SENSITIVE_INFORMATION::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `handle` is open with `FILE_READ_ATTRIBUTES`; `info` is a
    // `FILE_CASE_SENSITIVE_INFORMATION` of exactly the length passed.
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut status_block,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_CASE_SENSITIVE_INFORMATION>() as u32,
            FileCaseSensitiveInformation,
        )
    };
    status.is_ok().then_some(info.Flags)
}

/// The volume root and the directories to ask, the folder first and the root
/// last, for a file-system parsing name; `None` for a name without a
/// recognisable root. Components are kept exactly as given.
fn case_probe_paths(parsing_name: &[u16]) -> Option<(Vec<u16>, Vec<Vec<u16>>)> {
    let (root, start) = split_root(parsing_name)?;
    let root = match root {
        Root::Drive(r) => r,
        Root::Unc => {
            let mut r = parsing_name[..start].to_vec();
            r.push(BS);
            r
        }
    };
    let base = &parsing_name[..start];
    let mut dirs = vec![root.clone()];
    let mut path = base.to_vec();
    for segment in parsing_name[start..]
        .split(|&u| u == BS)
        .filter(|s| !s.is_empty())
    {
        path.push(BS);
        path.extend_from_slice(segment);
        dirs.push(path.clone());
    }
    dirs.reverse();
    Some((root, dirs))
}

/// Whether the folder named by a captured parsing name lies on a
/// case-sensitive directory (Req 14.20): only a `FileSystem` Location on a
/// volume whose flags include [`FILE_SUPPORTS_CASE_SENSITIVE_DIRS`] is asked,
/// and then the folder and its ancestors up to the volume root, nearest first,
/// stopping at the first case-sensitive one (a path is resolved case
/// sensitively as soon as one of its directories is). Every other Location,
/// and any failure, gives `false`, without a directory query.
pub fn detect_case_sensitive(
    probe: &impl CaseProbe,
    kind: LocationKind,
    parsing_name: &[u16],
) -> bool {
    if !matches!(kind, LocationKind::FileSystem(_)) {
        return false;
    }
    let Some((root, dirs)) = case_probe_paths(parsing_name) else {
        return false;
    };
    if !probe
        .volume_flags(&root)
        .is_some_and(|flags| flags & FILE_SUPPORTS_CASE_SENSITIVE_DIRS != 0)
    {
        return false;
    }
    dirs.iter()
        .any(|dir| probe.directory_case_sensitive(dir) == Some(true))
}

// ---------------------------------------------------------------------------
// Kind detection (pure)
// ---------------------------------------------------------------------------

const BS: u16 = b'\\' as u16;
const VERBATIM: &str = r"\\?\";
const DEVICE: &str = r"\\.\";

/// Classifies a desktop-absolute parsing name (see the module docs).
///
/// `drive_type(root)` returns the `GetDriveTypeW` value of a root such as
/// `C:\`; `is_file(path)` tells whether a path prefix ending in `.zip` is a
/// file. Both are only called for file-system forms.
pub fn classify_kind(
    parsing_name: &[u16],
    drive_type: impl Fn(&[u16]) -> u32,
    is_file: impl Fn(&[u16]) -> bool,
) -> LocationKind {
    let n = parsing_name;
    if starts_with_ci(n, esr_core::search::SCHEME) {
        return LocationKind::Search;
    }
    if starts_with(n, "::{") {
        return LocationKind::Shell(classify_shell(n));
    }
    if let Some((root, start)) = split_root(n) {
        let last = last_component(n, start);
        if last.is_some_and(|c| ends_with_ci(c, ".library-ms")) {
            return LocationKind::Shell(ShellKind::Library);
        }
        if last.is_some_and(|c| {
            ends_with_ci(c, ".search-ms") || ends_with_ci(c, ".searchconnector-ms")
        }) {
            return LocationKind::Shell(ShellKind::SavedSearch);
        }
        if is_inside_zip(n, start, &is_file) {
            return LocationKind::ZipFolder;
        }
        let volume = match root {
            Root::Drive(r) => volume_for_drive_type(drive_type(&r)),
            Root::Unc => Volume::Unc,
        };
        return LocationKind::FileSystem(volume);
    }
    if is_bare_server(n) {
        return LocationKind::Shell(ShellKind::Network);
    }
    LocationKind::Shell(ShellKind::Other)
}

/// Replaces 8.3 short-name components of a file-system parsing name with the
/// names on disk; any other parsing name is returned unchanged.
///
/// Above `MAX_PATH` the Shell reports file-system parsing names with 8.3
/// short names (`…\ESR-LO~1\A-RATH~1`), and so does Explorer's address bar.
/// Storing that would not preserve the path (Req 8.4), so every component
/// that contains `~` (only those: short names always do) is looked up with
/// `long_name(prefix)`, where `prefix` is the already expanded path up to and
/// including that component. A failed lookup (for example a component inside
/// a ZIP archive, or a name that really contains `~`) keeps the component as
/// it is. Separators and every other unit are copied verbatim.
pub fn expand_short_names(
    parsing_name: &[u16],
    long_name: impl Fn(&[u16]) -> Option<Vec<u16>>,
) -> Vec<u16> {
    const TILDE: u16 = b'~' as u16;
    let Some((_, start)) = split_root(parsing_name) else {
        return parsing_name.to_vec();
    };
    if !parsing_name[start..].contains(&TILDE) {
        return parsing_name.to_vec();
    }
    let mut out = parsing_name[..start].to_vec();
    for (i, segment) in parsing_name[start..].split(|&u| u == BS).enumerate() {
        if i > 0 {
            out.push(BS);
        }
        let at = out.len();
        out.extend_from_slice(segment);
        if segment.contains(&TILDE)
            && let Some(long) = long_name(&out)
        {
            out.truncate(at);
            out.extend(long);
        }
    }
    out
}

/// `DRIVE_REMOTE` → `Mapped`, `DRIVE_REMOVABLE`/`DRIVE_CDROM` → `Removable`,
/// else `Fixed`.
pub fn volume_for_drive_type(t: u32) -> Volume {
    match t {
        DRIVE_REMOTE => Volume::Mapped,
        DRIVE_REMOVABLE | DRIVE_CDROM => Volume::Removable,
        _ => Volume::Fixed,
    }
}

fn classify_shell(n: &[u16]) -> ShellKind {
    let (first, rest) = split_first(n);
    let Some(guid) = shell_guid(first) else {
        return ShellKind::Other;
    };
    let is = |id: &str| eq_ci(guid, id);
    if is(clsid::THIS_PC) {
        if rest.is_empty() {
            return ShellKind::ThisPc;
        }
        // WPD devices: `::{This PC}\\\?\usb#vid_…#{6ac27878-…}\SID-…`.
        if starts_with(rest, VERBATIM) {
            return ShellKind::PortableDevice;
        }
        let (second, _) = split_first(rest);
        return match shell_guid(second) {
            Some(g) if eq_ci(g, clsid::PORTABLE_DEVICES) => ShellKind::PortableDevice,
            Some(g) if eq_ci(g, clsid::CONTROL_PANEL) || eq_ci(g, clsid::CONTROL_PANEL_ALL) => {
                ShellKind::ControlPanel
            }
            _ => ShellKind::Other,
        };
    }
    if is(clsid::HOME) {
        ShellKind::Home
    } else if is(clsid::GALLERY) {
        ShellKind::Gallery
    } else if is(clsid::RECYCLE_BIN) {
        ShellKind::RecycleBin
    } else if is(clsid::NETWORK) || is(clsid::NETWORK_LEGACY) {
        ShellKind::Network
    } else if is(clsid::LIBRARIES) {
        ShellKind::Library
    } else if is(clsid::CONTROL_PANEL) || is(clsid::CONTROL_PANEL_ALL) {
        ShellKind::ControlPanel
    } else if is(clsid::PORTABLE_DEVICES) {
        ShellKind::PortableDevice
    } else {
        ShellKind::Other
    }
}

/// `::{GUID}` → `GUID`.
fn shell_guid(component: &[u16]) -> Option<&[u16]> {
    (starts_with(component, "::{") && component.last() == Some(&u16::from(b'}')))
        .then(|| &component[3..component.len() - 1])
}

/// Splits at the first backslash: `(before, after)`; `after` excludes it.
fn split_first(n: &[u16]) -> (&[u16], &[u16]) {
    match n.iter().position(|&u| u == BS) {
        Some(i) => (&n[..i], &n[i + 1..]),
        None => (n, &[]),
    }
}

enum Root {
    /// Root to pass to `GetDriveTypeW` (`C:\`, `\\?\Volume{…}\`).
    Drive(Vec<u16>),
    Unc,
}

/// Recognises a file-system root and returns it with the index where the
/// path below the root starts.
fn split_root(n: &[u16]) -> Option<(Root, usize)> {
    if starts_with(n, VERBATIM) {
        let r = &n[4..];
        if is_drive(r) {
            return Some((Root::Drive(drive_root(r[0])), 6));
        }
        if starts_with_ci(r, r"UNC\") {
            return unc_share_end(n, 8).map(|end| (Root::Unc, end));
        }
        if starts_with_ci(r, "Volume{") {
            return Some(match n[4..].iter().position(|&u| u == BS) {
                Some(i) => (Root::Drive(n[..=4 + i].to_vec()), 4 + i),
                None => {
                    let mut root = n.to_vec();
                    root.push(BS);
                    (Root::Drive(root), n.len())
                }
            });
        }
        return None;
    }
    if is_drive(n) {
        return Some((Root::Drive(drive_root(n[0])), 2));
    }
    if starts_with(n, r"\\") && !starts_with(n, DEVICE) {
        return unc_share_end(n, 2).map(|end| (Root::Unc, end));
    }
    None
}

/// `X:` followed by `\` or the end.
fn is_drive(n: &[u16]) -> bool {
    n.len() >= 2
        && u8::try_from(n[0]).is_ok_and(|c| c.is_ascii_alphabetic())
        && n[1] == u16::from(b':')
        && (n.len() == 2 || n[2] == BS)
}

fn drive_root(letter: u16) -> Vec<u16> {
    vec![letter, u16::from(b':'), BS]
}

/// For `server\share…` starting at `s`: the index just past `share`, if both
/// are non-empty.
fn unc_share_end(n: &[u16], s: usize) -> Option<usize> {
    let server_len = n[s..].iter().position(|&u| u == BS)?;
    if server_len == 0 {
        return None;
    }
    let share_start = s + server_len + 1;
    let share_len = n[share_start..]
        .iter()
        .position(|&u| u == BS)
        .unwrap_or(n.len() - share_start);
    (share_len > 0).then_some(share_start + share_len)
}

/// `\\server` or `\\server\` (a network computer, not a folder).
fn is_bare_server(n: &[u16]) -> bool {
    if !starts_with(n, r"\\") || starts_with(n, VERBATIM) || starts_with(n, DEVICE) {
        return false;
    }
    let rest = &n[2..];
    let server = rest
        .iter()
        .position(|&u| u == BS)
        .map_or(rest, |i| &rest[..i]);
    !server.is_empty() && rest[server.len()..].iter().all(|&u| u == BS)
}

/// The end of `n` without trailing backslashes (never before `start`).
fn trimmed_end(n: &[u16], start: usize) -> usize {
    let mut end = n.len();
    while end > start && n[end - 1] == BS {
        end -= 1;
    }
    end
}

/// The last non-empty component below the root.
fn last_component(n: &[u16], start: usize) -> Option<&[u16]> {
    let end = trimmed_end(n, start);
    let body = &n[start.min(end)..end];
    let from = body.iter().rposition(|&u| u == BS).map_or(0, |i| i + 1);
    let c = &body[from..];
    (!c.is_empty()).then_some(c)
}

/// Whether some component *before* the last one ends in `.zip` and is a file.
fn is_inside_zip(n: &[u16], start: usize, is_file: &impl Fn(&[u16]) -> bool) -> bool {
    let end = trimmed_end(n, start);
    let mut i = start;
    while i < end {
        if n[i] == BS {
            i += 1;
            continue;
        }
        let j = n[i..end]
            .iter()
            .position(|&u| u == BS)
            .map_or(end, |p| i + p);
        if j < end && ends_with_ci(&n[i..j], ".zip") && is_file(&n[..j]) {
            return true;
        }
        i = j;
    }
    false
}

fn starts_with(units: &[u16], pattern: &str) -> bool {
    let mut it = units.iter();
    pattern.encode_utf16().all(|p| it.next() == Some(&p))
}

/// ASCII case-insensitive prefix test against an ASCII pattern.
fn starts_with_ci(units: &[u16], pattern: &str) -> bool {
    units.len() >= pattern.len()
        && units
            .iter()
            .zip(pattern.bytes())
            .all(|(&u, b)| u8::try_from(u).is_ok_and(|c| c.eq_ignore_ascii_case(&b)))
}

fn eq_ci(units: &[u16], pattern: &str) -> bool {
    units.len() == pattern.len() && starts_with_ci(units, pattern)
}

fn ends_with_ci(units: &[u16], pattern: &str) -> bool {
    units.len() >= pattern.len() && starts_with_ci(&units[units.len() - pattern.len()..], pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn fs(v: Volume) -> LocationKind {
        LocationKind::FileSystem(v)
    }

    fn shell(k: ShellKind) -> LocationKind {
        LocationKind::Shell(k)
    }

    /// Drive types: C fixed, E removable, F CD-ROM, Z mapped, others unknown.
    fn fake_drive_type(root: &[u16]) -> u32 {
        match String::from_utf16_lossy(root).as_str() {
            r"C:\" | r"c:\" => DRIVE_FIXED,
            r"E:\" => DRIVE_REMOVABLE,
            r"F:\" => DRIVE_CDROM,
            r"Z:\" => DRIVE_REMOTE,
            r"\\?\Volume{1234}\" => DRIVE_REMOVABLE,
            _ => 0,
        }
    }

    /// Files: anything whose last component is `a.zip` or `B.ZIP`.
    fn fake_is_file(path: &[u16]) -> bool {
        let p = String::from_utf16_lossy(path);
        p.ends_with(r"\a.zip") || p.ends_with(r"\B.ZIP")
    }

    fn kind(s: &str) -> LocationKind {
        classify_kind(&w(s), fake_drive_type, fake_is_file)
    }

    #[test]
    fn classification_table() {
        use ShellKind::*;
        let cases: &[(&str, LocationKind)] = &[
            // File system volumes (Req 8.2, 8.7: the drive letter is kept).
            (r"C:\", fs(Volume::Fixed)),
            (r"C:", fs(Volume::Fixed)),
            (r"c:\Windows\System32", fs(Volume::Fixed)),
            (r"E:\photos", fs(Volume::Removable)),
            (r"F:\", fs(Volume::Removable)),
            (r"Z:\team\docs", fs(Volume::Mapped)),
            (r"Q:\unknown", fs(Volume::Fixed)),
            (r"\\?\C:\very\long", fs(Volume::Fixed)),
            (r"\\?\Z:\x", fs(Volume::Mapped)),
            (r"\\?\Volume{1234}\dir", fs(Volume::Removable)),
            (r"\\?\Volume{1234}", fs(Volume::Removable)),
            (r"\\server\share", fs(Volume::Unc)),
            (r"\\server\share\", fs(Volume::Unc)),
            (r"\\server\share\dir\sub", fs(Volume::Unc)),
            (r"\\?\UNC\server\share\dir", fs(Volume::Unc)),
            // ZIP: only below an archive that is a file.
            (r"C:\t\a.zip\inner", LocationKind::ZipFolder),
            (r"C:\t\a.zip\inner\deeper\", LocationKind::ZipFolder),
            (r"Z:\t\B.ZIP\x", LocationKind::ZipFolder),
            (r"\\server\share\a.zip\x", LocationKind::ZipFolder),
            (r"C:\t\a.zip", fs(Volume::Fixed)),
            (r"C:\t\a.zip\", fs(Volume::Fixed)),
            (r"C:\t\folder.zip\x", fs(Volume::Fixed)),
            (r"\\server\a.zip", fs(Volume::Unc)),
            // Shell locations.
            ("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", shell(ThisPc)),
            ("::{20d04fe0-3aea-1069-a2d8-08002b30309d}", shell(ThisPc)),
            ("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}", shell(Home)),
            ("::{E88865EA-0E1C-4E20-9AA6-EDCD0212C87C}", shell(Gallery)),
            (
                "::{645FF040-5081-101B-9F08-00AA002F954E}",
                shell(RecycleBin),
            ),
            ("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", shell(Network)),
            (
                "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\\SERVER",
                shell(Network),
            ),
            (r"\\SERVER", shell(Network)),
            (r"\\SERVER\", shell(Network)),
            ("::{031E4825-7B94-4DC3-B131-E946B44C8DD5}", shell(Library)),
            (
                "::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\\Documents.library-ms",
                shell(Library),
            ),
            (
                r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Libraries\Music.library-ms",
                shell(Library),
            ),
            (r"C:\Users\me\Searches\needle.search-ms", shell(SavedSearch)),
            (r"C:\Users\me\Searches\NEEDLE.Search-MS", shell(SavedSearch)),
            (r"\\srv\share\saved.search-ms", shell(SavedSearch)),
            (r"C:\Users\me\Searches\web.searchConnector-ms", shell(SavedSearch)),
            (r"C:\Users\me\Searches", fs(Volume::Fixed)),
            (r"C:\a.search-ms.txt", fs(Volume::Fixed)),
            // A search tab's native PIDL reports a display name as its parsing name.
            ("Search Results in Downloads", shell(Other)),
            (
                "::{26EE0668-A00A-44D7-9371-BEB064C98683}",
                shell(ControlPanel),
            ),
            (
                "::{26EE0668-A00A-44D7-9371-BEB064C98683}\\0\\::{BB06C0E4-D293-4F75-8A90-CB05B6477EEE}",
                shell(ControlPanel),
            ),
            (
                "::{21EC2020-3AEA-1069-A2DD-08002B30309D}",
                shell(ControlPanel),
            ),
            (
                "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\::{21EC2020-3AEA-1069-A2DD-08002B30309D}",
                shell(ControlPanel),
            ),
            (
                "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\\\\\?\\usb#vid_04e8&pid_6860#{6ac27878-a6fa-4155-ba85-f98f491d4f33}\\SID-{10001,,1}\\DCIM",
                shell(PortableDevice),
            ),
            (
                "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\\\\\?\\swd#wpdbusenum#_??_usbstor#disk",
                shell(PortableDevice),
            ),
            (
                "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\::{35786D3C-B075-49B9-88DD-029876E11C01}",
                shell(PortableDevice),
            ),
            (
                "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\::{0DB7E03F-FC29-4DC6-9020-FF41B59E513A}",
                shell(Other),
            ),
            ("::{018D5C66-4533-4307-9B53-224DE2ED1FE6}", shell(Other)),
            ("::{not-closed", shell(Other)),
            (r"\\.\PhysicalDrive0", shell(Other)),
            ("ftp://example.invalid/", shell(Other)),
            ("relative\\path", shell(Other)),
            // Search.
            (
                "search-ms:query=a&crumb=location:C%3A%5C",
                LocationKind::Search,
            ),
            ("SEARCH-MS:garbage", LocationKind::Search),
        ];
        for (name, expected) in cases {
            assert_eq!(kind(name), *expected, "{name}");
        }
    }

    #[test]
    fn classification_passes_roots_and_prefixes() {
        let roots = std::cell::RefCell::new(Vec::new());
        let files = std::cell::RefCell::new(Vec::new());
        let k = classify_kind(
            &w(r"D:\x.zip\folder.ZIP\y"),
            |r| {
                roots.borrow_mut().push(String::from_utf16_lossy(r));
                DRIVE_FIXED
            },
            |p| {
                files.borrow_mut().push(String::from_utf16_lossy(p));
                false
            },
        );
        assert_eq!(k, fs(Volume::Fixed));
        assert_eq!(*roots.borrow(), [r"D:\"]);
        assert_eq!(*files.borrow(), [r"D:\x.zip", r"D:\x.zip\folder.ZIP"]);
        // Shell names never touch the file system.
        let k = classify_kind(
            &w("::{645FF040-5081-101B-9F08-00AA002F954E}"),
            |_| panic!(),
            |_| panic!(),
        );
        assert_eq!(k, shell(ShellKind::RecycleBin));
    }

    #[test]
    fn classification_keeps_unicode_and_long_names_exact() {
        // Non-ASCII, NFD and unpaired surrogates do not confuse the parser.
        let mut n = w("C:\\Jose\u{301}\\\u{65e5}\u{672c}");
        n.push(0xD800);
        assert_eq!(
            classify_kind(&n, fake_drive_type, fake_is_file),
            fs(Volume::Fixed)
        );
        let mut long = w(r"\\?\C:\");
        long.extend(std::iter::repeat_n(
            u16::from(b'a'),
            MAX_WIDE_LEN - long.len(),
        ));
        assert_eq!(
            classify_kind(&long, fake_drive_type, fake_is_file),
            fs(Volume::Fixed)
        );
    }

    #[test]
    fn short_name_expansion() {
        let lookups = std::cell::RefCell::new(Vec::new());
        let fake = |p: &[u16]| {
            let p = String::from_utf16_lossy(p);
            lookups.borrow_mut().push(p.clone());
            match p.as_str() {
                r"C:\Temp\ESR-LO~1" => Some(w("esr-loc-long")),
                r"C:\Temp\esr-loc-long\A-RATH~1" => Some(w("a-rather-long")),
                r"\\srv\share\LONGNA~1" => Some(w("Long Name")),
                _ => None,
            }
        };
        let x = |s: &str| String::from_utf16_lossy(&expand_short_names(&w(s), fake));
        assert_eq!(
            x(r"C:\Temp\ESR-LO~1\A-RATH~1\b"),
            r"C:\Temp\esr-loc-long\a-rather-long\b"
        );
        assert_eq!(
            *lookups.borrow(),
            [r"C:\Temp\ESR-LO~1", r"C:\Temp\esr-loc-long\A-RATH~1"]
        );
        // Unknown short names, a real `~` and trailing separators stay as they are.
        assert_eq!(x(r"C:\Temp\ESR-LO~1\a~b\"), r"C:\Temp\esr-loc-long\a~b\");
        assert_eq!(x(r"\\srv\share\LONGNA~1"), r"\\srv\share\Long Name");
        lookups.borrow_mut().clear();
        // No lookups without a `~` below the root, or for non-file-system names.
        assert_eq!(x(r"C:\Temp\x"), r"C:\Temp\x");
        assert_eq!(x(r"C:\"), r"C:\");
        assert_eq!(x(r"\\srv~1\share"), r"\\srv~1\share");
        assert_eq!(
            x("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\X~1"),
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\X~1"
        );
        assert_eq!(
            x("search-ms:query=A~1&crumb=location:C%3A%5C"),
            "search-ms:query=A~1&crumb=location:C%3A%5C"
        );
        assert!(lookups.borrow().is_empty());
    }

    #[test]
    fn search_url_only_for_unrecognised_names() {
        let url = || Some(WideString::from("search-ms:query=a&crumb=location:C%3A%5C"));
        let asked = std::cell::Cell::new(0);
        let ask = || {
            asked.set(asked.get() + 1);
            url()
        };
        assert_eq!(search_url_for(shell(ShellKind::Other), ask), url());
        // Recognised kinds never ask for the editing name.
        for k in [
            fs(Volume::Fixed),
            LocationKind::ZipFolder,
            shell(ShellKind::ThisPc),
            shell(ShellKind::SavedSearch),
        ] {
            assert_eq!(
                search_url_for(k, || {
                    asked.set(asked.get() + 100);
                    url()
                }),
                None
            );
        }
        assert_eq!(asked.get(), 1);
        // An editing name that is not a search URL is ignored.
        assert_eq!(
            search_url_for(shell(ShellKind::Other), || Some("Search Results".into())),
            None
        );
        assert_eq!(search_url_for(shell(ShellKind::Other), || None), None);
    }

    #[test]
    fn volume_mapping() {
        assert_eq!(volume_for_drive_type(DRIVE_REMOTE), Volume::Mapped);
        assert_eq!(volume_for_drive_type(DRIVE_REMOVABLE), Volume::Removable);
        assert_eq!(volume_for_drive_type(DRIVE_CDROM), Volume::Removable);
        assert_eq!(volume_for_drive_type(DRIVE_FIXED), Volume::Fixed);
        assert_eq!(volume_for_drive_type(0), Volume::Fixed);
        assert_eq!(volume_for_drive_type(6), Volume::Fixed); // RAM disk
    }

    #[test]
    fn real_drive_types_classify_consistently() {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;
        // The system drive is a fixed disk and keeps its letter.
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let root = w(&format!(r"{sys}\"));
        assert_eq!(drive_type(&root), DRIVE_FIXED);
        let windir = w(&format!(r"{sys}\Windows"));
        assert_eq!(
            classify_kind(&windir, drive_type, is_file),
            fs(Volume::Fixed)
        );
        // Every present drive letter (mapped ones included, if the machine
        // has any) classifies from its own drive type, without UNC lookup.
        // SAFETY: no arguments.
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u8 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = char::from(b'A' + i);
            let root = w(&format!(r"{letter}:\"));
            let t = drive_type(&root);
            let name = w(&format!(r"{letter}:\some\folder"));
            let k = classify_kind(&name, drive_type, |_| false);
            assert_eq!(k, fs(volume_for_drive_type(t)), "{letter}: type {t}");
            if t == DRIVE_REMOTE {
                assert_eq!(k, fs(Volume::Mapped), "{letter}:");
            }
        }
    }

    #[test]
    fn mapped_drive_keeps_letter_root() {
        // Only the drive root is consulted; nothing is translated to UNC.
        let roots = std::cell::RefCell::new(Vec::new());
        for name in [r"M:\team\docs", r"\\?\M:\team\docs", r"M:"] {
            let k = classify_kind(
                &w(name),
                |r| {
                    roots.borrow_mut().push(String::from_utf16_lossy(r));
                    DRIVE_REMOTE
                },
                |_| false,
            );
            assert_eq!(k, fs(Volume::Mapped), "{name}");
        }
        assert_eq!(*roots.borrow(), [r"M:\", r"M:\", r"M:\"]);
    }

    #[test]
    fn pidl_shape_checks() {
        assert!(pidl_is_well_formed(&[0, 0]));
        assert!(pidl_is_well_formed(&[3, 0, 9, 0, 0]));
        assert!(pidl_is_well_formed(&[3, 0, 9, 4, 0, 1, 2, 0, 0]));
        for bad in [
            &[][..],
            &[0],
            &[0, 0, 0],       // trailing bytes
            &[3, 0, 9],       // no terminator
            &[9, 0, 1, 0, 0], // cb beyond the buffer
            &[2, 0, 0, 0],    // empty item
            &[1, 0, 0, 0],
            &[0xFF, 0xFF, 0, 0],
        ] {
            assert!(!pidl_is_well_formed(bad), "{bad:?}");
        }
    }

    #[test]
    fn verbatim_form_for_file_checks() {
        let v = |s: &str| String::from_utf16_lossy(&verbatim_form(&w(s)));
        assert_eq!(v(r"C:\a.zip"), r"\\?\C:\a.zip");
        assert_eq!(v(r"\\srv\share\a.zip"), r"\\?\UNC\srv\share\a.zip");
        assert_eq!(v(r"\\?\C:\a.zip"), r"\\?\C:\a.zip");
        assert_eq!(v("x"), "x");
    }

    // -----------------------------------------------------------------------
    // Case sensitivity (fake probe)
    // -----------------------------------------------------------------------

    /// A probe with a fixed volume answer and a set of case-sensitive
    /// directories, recording every question.
    struct FakeProbe {
        volume: Option<u32>,
        sensitive: Vec<&'static str>,
        volume_asked: std::cell::RefCell<Vec<String>>,
        dirs_asked: std::cell::RefCell<Vec<String>>,
    }

    impl FakeProbe {
        fn new(volume: Option<u32>, sensitive: &[&'static str]) -> Self {
            Self {
                volume,
                sensitive: sensitive.to_vec(),
                volume_asked: Default::default(),
                dirs_asked: Default::default(),
            }
        }
    }

    impl CaseProbe for FakeProbe {
        fn volume_flags(&self, root: &[u16]) -> Option<u32> {
            self.volume_asked
                .borrow_mut()
                .push(String::from_utf16_lossy(root));
            self.volume
        }

        fn directory_case_sensitive(&self, dir: &[u16]) -> Option<bool> {
            let dir = String::from_utf16_lossy(dir);
            self.dirs_asked.borrow_mut().push(dir.clone());
            Some(self.sensitive.contains(&dir.as_str()))
        }
    }

    const SUPPORTED: Option<u32> = Some(FILE_SUPPORTS_CASE_SENSITIVE_DIRS | 0x0000_0007);

    fn detect(probe: &FakeProbe, k: LocationKind, name: &str) -> bool {
        detect_case_sensitive(probe, k, &w(name))
    }

    #[test]
    fn case_flag_needs_a_supporting_volume() {
        // Without the volume flag the answer is false and no directory is asked.
        for volume in [Some(0x0000_0007), Some(0), None] {
            let p = FakeProbe::new(volume, &[r"C:\a"]);
            assert!(!detect(&p, fs(Volume::Fixed), r"C:\a"));
            assert_eq!(*p.volume_asked.borrow(), [r"C:\"]);
            assert!(p.dirs_asked.borrow().is_empty());
        }
    }

    #[test]
    fn case_flag_only_for_file_system_locations() {
        for k in [
            shell(ShellKind::Home),
            shell(ShellKind::Other),
            LocationKind::Search,
            LocationKind::ZipFolder,
        ] {
            let p = FakeProbe::new(SUPPORTED, &[r"C:\a", r"C:\"]);
            assert!(!detect(&p, k, r"C:\a"), "{k:?}");
            assert!(p.volume_asked.borrow().is_empty(), "{k:?}");
            assert!(p.dirs_asked.borrow().is_empty(), "{k:?}");
        }
        // A file-system kind with an unrecognisable name is not asked either.
        let p = FakeProbe::new(SUPPORTED, &[]);
        assert!(!detect(&p, fs(Volume::Fixed), "relative"));
        assert!(p.volume_asked.borrow().is_empty());
    }

    #[test]
    fn case_flag_asks_the_folder_then_its_ancestors_and_stops_at_a_hit() {
        let p = FakeProbe::new(SUPPORTED, &[]);
        assert!(!detect(&p, fs(Volume::Fixed), r"C:\a\b\c"));
        assert_eq!(
            *p.dirs_asked.borrow(),
            [r"C:\a\b\c", r"C:\a\b", r"C:\a", r"C:\"]
        );

        // The folder itself.
        let p = FakeProbe::new(SUPPORTED, &[r"C:\a\b\c"]);
        assert!(detect(&p, fs(Volume::Fixed), r"C:\a\b\c"));
        assert_eq!(*p.dirs_asked.borrow(), [r"C:\a\b\c"]);

        // An ancestor makes the folder below it case sensitive too.
        let p = FakeProbe::new(SUPPORTED, &[r"C:\a"]);
        assert!(detect(&p, fs(Volume::Fixed), r"C:\a\b\c\"));
        assert_eq!(*p.dirs_asked.borrow(), [r"C:\a\b\c", r"C:\a\b", r"C:\a"]);

        // A sibling does not.
        let p = FakeProbe::new(SUPPORTED, &[r"C:\other"]);
        assert!(!detect(&p, fs(Volume::Fixed), r"C:\a\b"));
    }

    #[test]
    fn case_flag_paths_for_roots_and_other_forms() {
        let paths = |name: &str| {
            case_probe_paths(&w(name)).map(|(root, dirs)| {
                (
                    String::from_utf16_lossy(&root),
                    dirs.iter()
                        .map(|d| String::from_utf16_lossy(d))
                        .collect::<Vec<_>>(),
                )
            })
        };
        assert_eq!(paths(r"C:\"), Some((r"C:\".into(), vec![r"C:\".into()])));
        assert_eq!(paths("C:"), Some((r"C:\".into(), vec![r"C:\".into()])));
        assert_eq!(
            paths(r"\\?\D:\x"),
            Some((r"D:\".into(), vec![r"\\?\D:\x".into(), r"D:\".into()]))
        );
        assert_eq!(
            paths(r"\\srv\share\a\b"),
            Some((
                r"\\srv\share\".into(),
                vec![
                    r"\\srv\share\a\b".into(),
                    r"\\srv\share\a".into(),
                    r"\\srv\share\".into()
                ]
            ))
        );
        assert_eq!(
            paths(r"\\?\Volume{1234}\dir"),
            Some((
                r"\\?\Volume{1234}\".into(),
                vec![r"\\?\Volume{1234}\dir".into(), r"\\?\Volume{1234}\".into()]
            ))
        );
        assert_eq!(paths("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}"), None);
    }

    #[test]
    fn case_flag_failed_directory_query_is_false() {
        struct Failing;
        impl CaseProbe for Failing {
            fn volume_flags(&self, _: &[u16]) -> Option<u32> {
                SUPPORTED
            }
            fn directory_case_sensitive(&self, _: &[u16]) -> Option<bool> {
                None
            }
        }
        assert!(!detect_case_sensitive(
            &Failing,
            fs(Volume::Fixed),
            &w(r"C:\gone")
        ));
    }

    #[test]
    fn case_flag_on_the_real_system_drive_is_answered() {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let root = w(&format!(r"{sys}\"));
        let flags = SystemCaseProbe.volume_flags(&root);
        assert!(flags.is_some(), "GetVolumeInformationW on {sys}\\");
        let dir = TempDir::new("case");
        let name = wide(&dir.0);
        if flags.is_some_and(|f| f & FILE_SUPPORTS_CASE_SENSITIVE_DIRS != 0) {
            // The query itself works on an existing directory; a fresh temp
            // directory is case insensitive unless its parent was changed.
            let answer = SystemCaseProbe.directory_case_sensitive(&name.0);
            assert!(answer.is_some(), "query for {}", dir.0.display());
            assert_eq!(
                detect_case_sensitive(&SystemCaseProbe, fs(Volume::Fixed), &name.0),
                answer == Some(true) || {
                    // Some ancestor may be case sensitive instead.
                    let (_, dirs) = case_probe_paths(&name.0).unwrap();
                    dirs.iter()
                        .any(|d| SystemCaseProbe.directory_case_sensitive(d) == Some(true))
                }
            );
        } else {
            assert!(!detect_case_sensitive(&SystemCaseProbe, fs(Volume::Fixed), &name.0));
        }
        // A missing directory cannot be asked.
        let missing = wide(&dir.0.join("no-such-dir"));
        assert_eq!(SystemCaseProbe.directory_case_sensitive(&missing.0), None);
    }

    // -----------------------------------------------------------------------
    // Shell capture (real Shell, no Explorer windows)
    // -----------------------------------------------------------------------

    use crate::shell_util::StaGuard;
    use std::path::{Path, PathBuf};

    fn wide(p: &Path) -> WideString {
        use std::os::windows::ffi::OsStrExt;
        WideString(p.as_os_str().encode_wide().collect())
    }

    fn capture(name: &str) -> LocationId {
        try_capture_from_parsing_name(&name.into()).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// A unique temp directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let p =
                std::env::temp_dir().join(format!("esr-loc-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Captures `path`, checks the parsing name is the path, unit for unit,
    /// and that capturing the stored PIDL again gives an identical result.
    fn capture_path(path: &Path) -> LocationId {
        let loc = try_capture_from_parsing_name(&wide(path))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        // Above MAX_PATH, newer builds (measured on 26300) report the path with
        // a `\\?\` prefix, which 22621 did not. The prefix is stored as reported.
        let expected = wide(path);
        let prefixed = WideString(
            r"\\?\"
                .encode_utf16()
                .chain(expected.0.iter().copied())
                .collect(),
        );
        assert!(
            loc.parsing_name == expected
                || (expected.0.len() > 259 && loc.parsing_name == prefixed),
            "parsing name of {}: {:?}",
            path.display(),
            loc.parsing_name
        );
        let again = try_capture_from_pidl(&loc.pidl).expect("capture from stored PIDL");
        assert!(again.identical(&loc), "{again:?} vs {loc:?}");
        loc
    }

    #[test]
    fn captures_windows_folder() {
        let _sta = StaGuard::init().unwrap();
        // Built from the system drive, not from `%windir%`: that variable's
        // letter case can differ from the folder's on-disk case (`C:\WINDOWS`
        // against `C:\Windows` on the 26300 VM), and the Shell reports the latter.
        let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let windir = PathBuf::from(format!(r"{system_drive}\Windows"));
        let loc = capture_path(&windir);
        assert_eq!(loc.kind, fs(Volume::Fixed));
        assert!(loc.search.is_none());
        assert!(!loc.display_name.is_empty());
        assert_eq!(&loc.pidl[loc.pidl.len() - 2..], &[0, 0]);
        assert!(
            matches!(capture_from_pidl(&loc.pidl), ObservedLocation::Known(l) if l.identical(&loc))
        );
    }

    #[test]
    fn captures_unicode_and_case_exactly() {
        let _sta = StaGuard::init().unwrap();
        let tmp = TempDir::new("uni");
        // NFD "é", NFC "é", CJK, an astral emoji, mixed case.
        let dir = tmp
            .0
            .join("Jose\u{301} Jos\u{e9} \u{65e5}\u{672c} \u{1F600} MiXeD");
        std::fs::create_dir(&dir).unwrap();
        let loc = capture_path(&dir);
        assert_eq!(loc.kind, fs(Volume::Fixed));
        assert_eq!(
            loc.display_name,
            WideString::from("Jose\u{301} Jos\u{e9} \u{65e5}\u{672c} \u{1F600} MiXeD")
        );
    }

    #[test]
    fn captures_path_longer_than_max_path() {
        let _sta = StaGuard::init().unwrap();
        let tmp = TempDir::new("long");
        let mut dir = tmp.0.clone();
        while wide(&dir).len() <= 300 {
            dir.push("a-rather-long-folder-name-0123456789");
        }
        std::fs::create_dir_all(&dir).unwrap();
        let loc = capture_path(&dir);
        assert_eq!(
            loc.display_name,
            WideString::from("a-rather-long-folder-name-0123456789")
        );
        assert!(loc.parsing_name.len() > 300);
        assert_eq!(loc.kind, fs(Volume::Fixed));
    }

    #[test]
    fn captures_shell_locations() {
        let _sta = StaGuard::init().unwrap();
        for (name, expected) in [
            (
                "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
                ShellKind::ThisPc,
            ),
            (
                "::{645FF040-5081-101B-9F08-00AA002F954E}",
                ShellKind::RecycleBin,
            ),
            (
                "::{26EE0668-A00A-44D7-9371-BEB064C98683}",
                ShellKind::ControlPanel,
            ),
            (
                "::{21EC2020-3AEA-1069-A2DD-08002B30309D}",
                ShellKind::ControlPanel,
            ),
            ("shell:Libraries", ShellKind::Library),
            (
                "::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\\Documents.library-ms",
                ShellKind::Library,
            ),
        ] {
            let loc = capture(name);
            assert_eq!(loc.kind, shell(expected), "{name} → {loc:?}");
            assert!(!loc.display_name.is_empty(), "{name}");
            let again = try_capture_from_pidl(&loc.pidl).unwrap();
            assert!(again.identical(&loc), "{name}");
        }
    }

    #[test]
    fn captures_home() {
        let _sta = StaGuard::init().unwrap();
        // Home exists from Windows 11 22H2 on (the supported builds).
        let loc = capture("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}");
        assert_eq!(loc.kind, shell(ShellKind::Home));
        assert_eq!(
            loc.parsing_name,
            WideString::from("::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}")
        );
    }

    /// Writes a stored (uncompressed) ZIP with the directory `inner/` and the
    /// empty file `inner/empty.txt` (CRC of no data is 0).
    fn write_zip(path: &Path) {
        fn u16le(v: &mut Vec<u8>, x: u16) {
            v.extend(x.to_le_bytes());
        }
        fn u32le(v: &mut Vec<u8>, x: u32) {
            v.extend(x.to_le_bytes());
        }
        let entries: [(&str, u32); 2] = [("inner/", 0x10), ("inner/empty.txt", 0x20)];
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, attrs) in entries {
            let offset = out.len() as u32;
            let fields = |v: &mut Vec<u8>| {
                u16le(v, 20); // version needed
                u16le(v, 0); // flags
                u16le(v, 0); // stored
                u16le(v, 0); // time
                u16le(v, 0x21); // date 1980-01-01
                u32le(v, 0); // crc
                u32le(v, 0); // compressed size
                u32le(v, 0); // size
                u16le(v, name.len() as u16);
                u16le(v, 0); // extra
            };
            u32le(&mut out, 0x0403_4b50);
            fields(&mut out);
            out.extend(name.as_bytes());

            u32le(&mut central, 0x0201_4b50);
            u16le(&mut central, 20); // version made by
            fields(&mut central);
            u16le(&mut central, 0); // comment
            u16le(&mut central, 0); // disk
            u16le(&mut central, 0); // internal attrs
            u32le(&mut central, attrs);
            u32le(&mut central, offset);
            central.extend(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend(central);
        u32le(&mut out, 0x0605_4b50);
        u16le(&mut out, 0);
        u16le(&mut out, 0);
        u16le(&mut out, entries.len() as u16);
        u16le(&mut out, entries.len() as u16);
        u32le(&mut out, cd_size);
        u32le(&mut out, cd_offset);
        u16le(&mut out, 0);
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn captures_zip_folder() {
        let _sta = StaGuard::init().unwrap();
        let tmp = TempDir::new("zip");
        let archive = tmp.0.join("Archive.zip");
        write_zip(&archive);
        let inner = capture_path(&archive.join("inner"));
        assert_eq!(inner.kind, LocationKind::ZipFolder);
        // The archive itself is a file-system location (see module docs).
        let root = capture_path(&archive);
        assert_eq!(root.kind, fs(Volume::Fixed));
        // A plain folder named like an archive is not one.
        let fake = tmp.0.join("folder.zip").join("sub");
        std::fs::create_dir_all(&fake).unwrap();
        assert_eq!(capture_path(&fake).kind, fs(Volume::Fixed));
    }

    #[test]
    fn captures_search_with_scope() {
        let _sta = StaGuard::init().unwrap();
        let tmp = TempDir::new("search");
        let query = WideString::from("a&b=c %d \u{e9}");
        let url = esr_core::search::build_search_ms(&query, &wide(&tmp.0)).unwrap();
        let loc = try_capture_from_parsing_name(&url).expect("capture search");
        assert_eq!(loc.kind, LocationKind::Search, "{loc:?}");
        let spec = loc.search.as_ref().expect("search spec");
        assert_eq!(spec.query, query);
        assert_eq!(spec.scope.parsing_name, wide(&tmp.0));
        assert_eq!(spec.scope.kind, fs(Volume::Fixed));
        assert!(!spec.scope.pidl.is_empty());
        // Bound as the native search folder (shown in a tab), not as an
        // Internet-URL item (opened in a new window).
        assert!(
            !crate::shell_util::is_internet_url_pidl(&loc.pidl),
            "{:?}",
            loc.pidl
        );
        assert!(is_search_ms(&loc.parsing_name), "{loc:?}");
        let again = try_capture_from_pidl(&loc.pidl).unwrap();
        assert_eq!(again, loc);
        assert_eq!(again.kind, LocationKind::Search);
    }

    #[test]
    fn captures_saved_search_file() {
        let _sta = StaGuard::init().unwrap();
        let tmp = TempDir::new("saved");
        let file = tmp.0.join("Needle.search-ms");
        std::fs::write(
            &file,
            br#"<?xml version="1.0"?>
<persistedQuery version="1.0"><query><conditions><condition type="leafCondition" property="System.Generic.String" operator="wordmatch" propertyType="string" value="needle"/></conditions><scope><include knownFolder="{FDD39AD0-238F-46AF-ADB4-6C85480369C7}"/></scope></query></persistedQuery>
"#,
        )
        .unwrap();
        let loc = capture_path(&file);
        assert_eq!(loc.kind, shell(ShellKind::SavedSearch));
        assert!(loc.search.is_none());
    }

    #[test]
    fn unidentifiable_locations() {
        let _sta = StaGuard::init().unwrap();
        for garbage in [
            &[][..],
            &[5, 0, 1, 2],
            &[7, 0, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0, 0],
        ] {
            assert_eq!(
                capture_from_pidl(garbage),
                ObservedLocation::Unidentifiable,
                "{garbage:?}"
            );
        }
        assert_eq!(
            try_capture_from_pidl(&[5, 0, 1, 2]),
            Err(CaptureError::MalformedPidl)
        );
        for name in ["", "C:\\no\0nul", r"C:\does-not-exist-esr-7f3a\x"] {
            assert_eq!(
                capture_from_parsing_name(&name.into()),
                ObservedLocation::Unidentifiable,
                "{name:?}"
            );
        }
        // A search whose URL cannot be represented, or whose scope cannot be bound.
        let tmp = TempDir::new("badsearch");
        let missing = tmp.0.join("gone");
        let url = esr_core::search::build_search_ms(&"q".into(), &wide(&missing)).unwrap();
        assert_eq!(
            capture_from_parsing_name(&url),
            ObservedLocation::Unidentifiable
        );
    }
}
