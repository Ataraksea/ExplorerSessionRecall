//! Shared Shell COM helpers: STA initialization, `IShellWindows` enumeration
//! grouped by top-level `CabinetWClass` HWND, tab HWND, and current folder PIDL.
//!
//! These helpers return raw Windows values (HWNDs, PIDL byte buffers, COM
//! interfaces) and do not depend on `esr_core` types. The scanner, navigator,
//! prober, and the `#[ignore]` verification spikes all build on them.
//!
//! All functions that take or return COM interfaces must be called on a thread
//! that has entered an STA through [`StaGuard::init`]. Explorer ignores some
//! calls (for example `IWebBrowser2::Quit`) unless the calling STA pumps
//! messages, which is the caller's responsibility.
//!
//! Requirements 2.1, 2.5.

use std::marker::PhantomData;

use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, CreateBindCtx, IBindCtx, IServiceProvider,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IFolderView, ILGetSize, IPersistFolder2, IShellBrowser, IShellWindows, IWebBrowser2,
    SID_STopLevelBrowser, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{Interface, Result, w};

/// Window class of a top-level File Explorer window.
pub const CABINET_WINDOW_CLASS: &str = "CabinetWClass";
/// Window class of the per-tab child window inside a `CabinetWClass` window.
pub const SHELL_TAB_WINDOW_CLASS: &str = "ShellTabWindowClass";

/// RAII guard for a single-threaded COM apartment on the current thread.
///
/// Dropping the guard calls `CoUninitialize`. The guard is `!Send` because the
/// apartment belongs to the thread that created it.
#[derive(Debug)]
pub struct StaGuard {
    _not_send: PhantomData<*const ()>,
}

impl StaGuard {
    /// Enters an STA on the current thread (`COINIT_APARTMENTTHREADED |
    /// COINIT_DISABLE_OLE1DDE`). Succeeds if the thread is already in an STA.
    /// Fails with `RPC_E_CHANGED_MODE` if the thread is already in the MTA.
    pub fn init() -> Result<Self> {
        // SAFETY: no reserved pointer is passed; flags are valid constants.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            return Err(hr.into());
        }
        // S_OK and S_FALSE both require a matching CoUninitialize.
        hr.ok()?;
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for StaGuard {
    fn drop(&mut self) {
        // SAFETY: balanced with the successful CoInitializeEx in `init` on this
        // same thread (the guard is !Send).
        unsafe { CoUninitialize() };
    }
}

/// One Explorer tab: its ShellWindows browser and its windows.
#[derive(Debug, Clone)]
pub struct ShellTab {
    /// The tab's browser object from the ShellWindows collection.
    pub browser: IWebBrowser2,
    /// The tab's `ShellTabWindowClass` child window, if it could be resolved.
    pub tab_hwnd: Option<HWND>,
}

/// All tabs of one top-level `CabinetWClass` window, in ShellWindows order
/// (which is not the visual tab order; use UIA for that).
#[derive(Debug, Clone)]
pub struct ShellWindowGroup {
    /// The top-level `CabinetWClass` window.
    pub top_hwnd: HWND,
    /// The owning `explorer.exe` process ID. Windows may live in separate
    /// Explorer processes (Req 2.5).
    pub process_id: u32,
    pub tabs: Vec<ShellTab>,
}

/// Creates the system `ShellWindows` collection. Requires an STA.
pub fn shell_windows() -> Result<IShellWindows> {
    // SAFETY: valid CLSID; no aggregation.
    unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
}

/// Enumerates every ShellWindows entry that belongs to a top-level
/// `CabinetWClass` window and groups the entries (tabs) by that window.
///
/// Groups keep the order of their first entry in the collection; tabs keep
/// collection order. Entries that fail to resolve (closing tabs, non-Explorer
/// browsers) are skipped rather than failing the whole enumeration.
pub fn enumerate_explorer_windows(windows: &IShellWindows) -> Result<Vec<ShellWindowGroup>> {
    // SAFETY: plain COM property read on a live interface.
    let count = unsafe { windows.Count()? };
    let mut entries = Vec::with_capacity(count.max(0) as usize);
    for index in 0..count {
        let Some(browser) = item_browser(windows, index) else {
            continue;
        };
        let Some(top) = top_level_hwnd(&browser) else {
            continue;
        };
        if !is_cabinet_window(top) {
            continue;
        }
        let tab_hwnd = tab_hwnd(&browser).ok();
        entries.push((top, ShellTab { browser, tab_hwnd }));
    }
    Ok(group_by_key(entries)
        .into_iter()
        .map(|(top_hwnd, tabs)| ShellWindowGroup {
            top_hwnd,
            process_id: window_process_id(top_hwnd),
            tabs,
        })
        .collect())
}

/// Opens the system ShellWindows collection and enumerates it.
pub fn enumerate_all_explorer_windows() -> Result<Vec<ShellWindowGroup>> {
    enumerate_explorer_windows(&shell_windows()?)
}

/// Returns the `IWebBrowser2` at `index`, or `None` for empty/foreign entries.
fn item_browser(windows: &IShellWindows, index: i32) -> Option<IWebBrowser2> {
    let variant = variant_i4(index);
    // SAFETY: `variant` is a valid VT_I4 VARIANT that owns no resources.
    let dispatch = unsafe { windows.Item(&variant) }.ok()?;
    dispatch.cast::<IWebBrowser2>().ok()
}

/// Builds a `VT_I4` VARIANT. It owns no resources, so it needs no `VariantClear`.
fn variant_i4(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

/// The top-level window reported by `IWebBrowser2::HWND`. All tabs of one
/// Explorer window report the same `CabinetWClass` HWND.
pub fn top_level_hwnd(browser: &IWebBrowser2) -> Option<HWND> {
    // SAFETY: plain COM property read on a live interface.
    let handle = unsafe { browser.HWND() }.ok()?;
    let hwnd = HWND(handle.0 as *mut core::ffi::c_void);
    (!hwnd.is_invalid()).then_some(hwnd)
}

/// The tab's own `IShellBrowser`, via `SID_STopLevelBrowser`.
pub fn shell_browser(browser: &IWebBrowser2) -> Result<IShellBrowser> {
    let provider: IServiceProvider = browser.cast()?;
    // SAFETY: valid service GUID; the result type matches the requested IID.
    unsafe { provider.QueryService(&SID_STopLevelBrowser) }
}

/// The tab's `ShellTabWindowClass` HWND via
/// `SID_STopLevelBrowser → IShellBrowser::GetWindow`.
pub fn tab_hwnd(browser: &IWebBrowser2) -> Result<HWND> {
    let shell_browser = shell_browser(browser)?;
    // SAFETY: plain COM call on a live interface.
    unsafe { shell_browser.GetWindow() }
}

/// The tab's current folder as an absolute PIDL byte buffer (including the
/// 2-byte zero terminator), via `QueryActiveShellView → IFolderView::GetFolder
/// → IPersistFolder2::GetCurFolder`.
pub fn current_folder_pidl(browser: &IWebBrowser2) -> Result<Vec<u8>> {
    let shell_browser = shell_browser(browser)?;
    // SAFETY: plain COM calls on live interfaces.
    let view = unsafe { shell_browser.QueryActiveShellView()? };
    let folder_view: IFolderView = view.cast()?;
    // SAFETY: plain COM call; the result type matches the requested IID.
    let folder: IPersistFolder2 = unsafe { folder_view.GetFolder()? };
    // SAFETY: on success GetCurFolder returns a CoTaskMemAlloc'd PIDL we own.
    let pidl = unsafe { folder.GetCurFolder()? };
    if pidl.is_null() {
        return Err(windows::Win32::Foundation::E_POINTER.into());
    }
    // SAFETY: `pidl` is a valid, non-null PIDL; it is freed exactly once below.
    let bytes = unsafe { pidl_to_bytes(pidl) };
    // SAFETY: allocated by the shell with the COM task allocator.
    unsafe { CoTaskMemFree(Some(pidl as *const core::ffi::c_void)) };
    Ok(bytes)
}

/// Copies a PIDL into an owned byte buffer, including its terminator.
///
/// # Safety
/// `pidl` must point to a valid, terminated `ITEMIDLIST`.
pub unsafe fn pidl_to_bytes(pidl: *const ITEMIDLIST) -> Vec<u8> {
    // SAFETY: caller guarantees a valid PIDL; ILGetSize includes the terminator.
    let size = unsafe { ILGetSize(Some(pidl)) } as usize;
    // SAFETY: the PIDL is `size` readable bytes.
    unsafe { std::slice::from_raw_parts(pidl.cast::<u8>(), size) }.to_vec()
}

/// The window's class name, or an empty string if it cannot be read.
pub fn window_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: the buffer is valid for its length; stale HWNDs just return 0.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

/// Whether a window class name is that of a File Explorer window
/// (`CabinetWClass`). Dialogs (`#32770`), the desktop (`Progman`, `WorkerW`)
/// and the taskbar (`Shell_TrayWnd`) are not (Req 2.6).
pub fn is_cabinet_class(class: &str) -> bool {
    class == CABINET_WINDOW_CLASS
}

/// Whether `hwnd` is a top-level `CabinetWClass` window.
pub fn is_cabinet_window(hwnd: HWND) -> bool {
    // SAFETY: GetAncestor tolerates stale HWNDs.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    root == hwnd && is_cabinet_class(&window_class_name(hwnd))
}

/// The process ID owning `hwnd`, or 0 if the window is gone.
pub fn window_process_id(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer; stale HWNDs leave it at 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Whether `hwnd` has the `WS_VISIBLE` style (and its ancestors do too).
///
/// Note: on the dev machine (Windows 11) every `ShellTabWindowClass` child of a
/// window reported visible, so this alone does not identify the active tab.
pub fn is_window_visible(hwnd: HWND) -> bool {
    // SAFETY: IsWindowVisible tolerates stale HWNDs.
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

/// Whether a parsing name uses the `search-ms:` scheme (ASCII
/// case-insensitive).
pub fn is_search_ms_name(name: &[u16]) -> bool {
    const SCHEME: &[u8] = b"search-ms:";
    name.len() >= SCHEME.len()
        && name
            .iter()
            .zip(SCHEME)
            .all(|(&u, &b)| u8::try_from(u).is_ok_and(|c| c.eq_ignore_ascii_case(&b)))
}

/// The bind context to pass to `SHParseDisplayName` for `name`: for a
/// `search-ms:` URL, one carrying `STR_PARSE_PREFER_FOLDER_BROWSING`; `None`
/// (no bind context) for anything else.
///
/// Measured on 22621.3880: without it, a `search-ms:` URL parses to an
/// Internet-URL item (first ID is the Internet folder,
/// [`INTERNET_FOLDER_ITEM_PREFIX`]) whose browse runs the `search-ms`
/// protocol handler, which opens the search in a **new window** (from
/// `Navigate2`, `BrowseObject(SBSP_SAMEBROWSER)` and URL strings alike). With
/// it, the URL parses to the native search-folder PIDL that Explorer itself
/// uses for a search tab, and `Navigate2` shows the search in the tab.
pub fn parse_bind_ctx(name: &[u16]) -> Option<IBindCtx> {
    if !is_search_ms_name(name) {
        return None;
    }
    // SAFETY: plain COM calls; both bind contexts are owned here.
    unsafe {
        let ctx = CreateBindCtx(0).ok()?;
        // Only the presence of the parameter matters; any object will do.
        let marker = CreateBindCtx(0).ok()?;
        ctx.RegisterObjectParam(w!("Parse Prefer Folder Browsing"), &marker)
            .ok()?;
        Some(ctx)
    }
}

/// First ID of a PIDL that `SHParseDisplayName` produces for a URL it hands
/// to the Internet folder (`{871C5380-42A0-1069-A2EA-08002B30309D}`):
/// `cb = 20`, type `0x1F`, any sort byte, then the CLSID.
pub const INTERNET_FOLDER_ITEM_PREFIX: [u8; 16] = [
    0x80, 0x53, 0x1C, 0x87, 0xA0, 0x42, 0x69, 0x10, 0xA2, 0xEA, 0x08, 0x00, 0x2B, 0x30, 0x30, 0x9D,
];

/// Whether `pidl` is an Internet-URL item (see [`parse_bind_ctx`]), which
/// Explorer opens in a new window instead of the tab it is browsed in.
pub fn is_internet_url_pidl(pidl: &[u8]) -> bool {
    pidl.len() >= 20
        && u16::from_le_bytes([pidl[0], pidl[1]]) >= 20
        && pidl[2] == 0x1F
        && pidl[4..20] == INTERNET_FOLDER_ITEM_PREFIX
}

/// Groups `(key, value)` pairs by key. Groups appear in order of the key's
/// first occurrence; values keep input order within a group.
pub fn group_by_key<K: PartialEq, V>(items: impl IntoIterator<Item = (K, V)>) -> Vec<(K, Vec<V>)> {
    let mut groups: Vec<(K, Vec<V>)> = Vec::new();
    for (key, value) in items {
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, values)) => values.push(value),
            None => groups.push((key, vec![value])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_by_key_keeps_first_seen_order() {
        let groups = group_by_key([(2, 'a'), (1, 'b'), (2, 'c'), (3, 'd'), (1, 'e')]);
        assert_eq!(
            groups,
            vec![(2, vec!['a', 'c']), (1, vec!['b', 'e']), (3, vec!['d'])]
        );
    }

    #[test]
    fn search_ms_names_and_url_items() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert!(is_search_ms_name(&w("search-ms:query=a")));
        assert!(is_search_ms_name(&w("SEARCH-MS:")));
        for n in ["search-ms", "search:query=a", r"C:\search-ms:", ""] {
            assert!(!is_search_ms_name(&w(n)), "{n:?}");
        }
        assert!(parse_bind_ctx(&w(r"C:\Windows")).is_none());
        let mut url = vec![20u8, 0, 0x1F, 0x68];
        url.extend(INTERNET_FOLDER_ITEM_PREFIX);
        url.extend([0, 0]);
        assert!(is_internet_url_pidl(&url));
        url[2] = 0x2E;
        assert!(!is_internet_url_pidl(&url));
        assert!(!is_internet_url_pidl(&[0, 0]));
    }

    /// Without the folder-browsing bind context a `search-ms:` URL binds to
    /// an Internet-URL item; with it, to the native search folder.
    #[test]
    fn search_urls_bind_to_the_native_search_folder() {
        use windows::Win32::UI::Shell::SHParseDisplayName;
        use windows::core::PCWSTR;
        let _sta = StaGuard::init().unwrap();
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let enc: String = dir
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let url = format!("search-ms:crumb=System.Generic.String%3Aneedle&crumb=location:{enc}");
        let z: Vec<u16> = url.encode_utf16().chain([0]).collect();
        let bind = |ctx: Option<IBindCtx>| {
            let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
            // SAFETY: NUL-terminated input, valid out pointer; freed once.
            unsafe {
                SHParseDisplayName(PCWSTR(z.as_ptr()), ctx.as_ref(), &mut out, 0, None)
                    .expect("parse search URL");
                let b = pidl_to_bytes(out);
                CoTaskMemFree(Some(out.cast_const().cast()));
                b
            }
        };
        let ctx = parse_bind_ctx(&z[..z.len() - 1]);
        assert!(ctx.is_some());
        assert!(!is_internet_url_pidl(&bind(ctx)));
        assert!(is_internet_url_pidl(&bind(None)), "plain parse (platform behaviour)");
    }

    #[test]
    fn window_class_filter_accepts_only_cabinet() {
        assert!(is_cabinet_class("CabinetWClass"));
        for other in [
            "#32770",
            "Progman",
            "WorkerW",
            "Shell_TrayWnd",
            "ShellTabWindowClass",
            "ExploreWClass",
            "CabinetWClass ",
            "",
        ] {
            assert!(!is_cabinet_class(other), "{other:?}");
        }
    }

    #[test]
    fn group_by_key_empty() {
        let groups = group_by_key(Vec::<(u8, u8)>::new());
        assert!(groups.is_empty());
    }

    #[test]
    fn variant_i4_has_expected_payload() {
        let v = variant_i4(7);
        // SAFETY: we just built it as VT_I4.
        let inner = unsafe { &v.Anonymous.Anonymous };
        assert_eq!(inner.vt, VT_I4);
        // SAFETY: VT_I4 means lVal is the active field.
        assert_eq!(unsafe { inner.Anonymous.lVal }, 7);
    }

    #[test]
    fn sta_guard_nests() {
        let _outer = StaGuard::init().expect("first STA init");
        let _inner = StaGuard::init().expect("nested STA init (S_FALSE)");
    }

    /// Smoke test against the live desktop: open a few Explorer windows/tabs
    /// first, then run `cargo test -p esr-win -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires an interactive desktop with open Explorer windows"]
    fn smoke_enumerate_open_explorer_windows() {
        let _sta = StaGuard::init().expect("STA init");
        let groups = enumerate_all_explorer_windows().expect("enumerate ShellWindows");
        for group in &groups {
            assert!(is_cabinet_window(group.top_hwnd));
            assert_ne!(group.process_id, 0);
            println!(
                "window {:?} pid {} tabs {}",
                group.top_hwnd,
                group.process_id,
                group.tabs.len()
            );
            for tab in &group.tabs {
                let tab_hwnd = tab.tab_hwnd.expect("tab HWND");
                assert_eq!(window_class_name(tab_hwnd), SHELL_TAB_WINDOW_CLASS);
                let pidl = current_folder_pidl(&tab.browser).expect("current folder PIDL");
                assert!(pidl.len() >= 2);
                assert_eq!(&pidl[pidl.len() - 2..], &[0, 0]);
                println!(
                    "  tab {:?} visible={} pidl_len={}",
                    tab_hwnd,
                    is_window_visible(tab_hwnd),
                    pidl.len()
                );
            }
        }
        println!("{} Explorer window(s)", groups.len());
    }
}
