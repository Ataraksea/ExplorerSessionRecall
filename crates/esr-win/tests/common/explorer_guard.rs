//! `ExplorerGuard`: makes sure every Explorer window a spike opens is closed
//! again, whether the test passes, fails or panics.
//!
//! **Explorer-opening tests must not run concurrently.** One test can otherwise
//! see (and adopt or sweep) another test's windows. The guard enforces this:
//! [`ExplorerGuard::new`] first takes a session-wide named mutex
//! (`Local\esr_spike_explorer_guard`) and holds it until cleanup is done, so
//! guarded tests serialise across threads and test binaries. Still run one test
//! binary at a time, single-threaded, for example:
//!
//! ```text
//! cargo test -p esr-win --test <name> -- --ignored --test-threads=1
//! ```
//!
//! How it works:
//!
//! * [`ExplorerGuard::new`] records the pre-test set of top-level
//!   `CabinetWClass` windows and creates a unique temp root
//!   `%TEMP%\esr_spike_<name>_<pid>`. Tests open their windows on folders
//!   under that root.
//! * [`ExplorerGuard::adopt`] registers a window the test created. Windows from
//!   the pre-test set can never be adopted.
//! * Cleanup runs in [`ExplorerGuard::finish`] (which reports leftovers as an
//!   error) and otherwise in `Drop` (which also runs on panic). Each adopted
//!   window first gets `WM_SYSCOMMAND SC_CLOSE` (the caption's close button),
//!   which closes the window with all of its tabs at once, and then
//!   `WM_CLOSE` repeatedly, with messages pumped, until it is gone (up to
//!   [`CLOSE_TIMEOUT`]): on a multi-tab window one `WM_CLOSE` closes only the
//!   selected tab. Closing tabs one by one in quick succession crashed
//!   Explorer 22621 (`FileExplorerExtensions.dll` + `0x103f7a`,
//!   `FileExplorerTabControl::TabView_GotFocus` on a null `XamlRoot`, from a
//!   `TabItemsChanged` timer of a window being torn down; task 16.3), so
//!   `WM_CLOSE` is only the fallback. Then a sweep closes every `CabinetWClass` window that is
//!   not in the pre-test set and shows a folder under the temp root (current
//!   folder parsing name via ShellWindows). That catches windows created by a
//!   tab drag-out and Explorer's transient windows. A search-results tab has a
//!   display name ("Search Results in …") as its parsing name; its
//!   `SIGDN_DESKTOPABSOLUTEEDITING` name is the `search-ms:` URL, so a tab
//!   also counts as under the temp root when that URL's scope (decoded with
//!   `esr_core::search::parse_search_ms`) or its raw or percent-decoded text
//!   references the temp root. Windows that existed before the test, or that
//!   are neither adopted nor under the temp root, are never touched.
//! * The temp root is deleted after the windows are closed.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use esr_win::shell_util::{
    StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_cabinet_window,
};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SHGetNameFromIDList, SIGDN, SIGDN_DESKTOPABSOLUTEEDITING, SIGDN_DESKTOPABSOLUTEPARSING,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
    SC_CLOSE, TranslateMessage, WM_CLOSE, WM_SYSCOMMAND,
};
use windows::core::{BOOL, w};

/// How long `ExplorerGuard::new` waits for another guarded test to finish.
const SERIAL_TIMEOUT_MS: u32 = 15 * 60 * 1000;

/// Session-wide mutex that serialises Explorer-opening tests (threads and
/// processes). Released (by the owning thread) when dropped.
struct SerialLock(HANDLE);

impl SerialLock {
    fn acquire(name: &str) -> Self {
        // SAFETY: plain named-mutex creation and wait; the handle is closed in Drop.
        unsafe {
            let h = CreateMutexW(None, false, w!("Local\\esr_spike_explorer_guard"))
                .expect("create the Explorer test mutex");
            let r = WaitForSingleObject(h, SERIAL_TIMEOUT_MS);
            if r != WAIT_OBJECT_0 && r != WAIT_ABANDONED {
                let _ = CloseHandle(h);
                panic!("[guard {name}] another Explorer-opening test is still running");
            }
            Self(h)
        }
    }
}

impl Drop for SerialLock {
    fn drop(&mut self) {
        // SAFETY: the mutex is owned by this thread (tests create and drop the
        // guard on the same thread); the handle is valid.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Maximum time spent closing one window (all of its tabs).
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause between two `WM_CLOSE` posts to the same window.
const CLOSE_STEP: Duration = Duration::from_millis(400);

/// Pumps all pending messages of the calling thread.
pub fn pump_once() {
    let mut msg = MSG::default();
    // SAFETY: standard message pump on the calling thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Pumps messages for `d`.
pub fn pump_for(d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        pump_once();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// All top-level `CabinetWClass` windows (EnumWindows), as raw HWND values.
pub fn top_level_cabinets() -> Vec<isize> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<isize>` passed below.
        let list = unsafe { &mut *(data.0 as *mut Vec<isize>) };
        if is_cabinet_window(hwnd) {
            list.push(hwnd.0 as isize);
        }
        true.into()
    }
    let mut list: Vec<isize> = Vec::new();
    // SAFETY: the callback only runs during this call; `list` outlives it.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// Desktop-absolute parsing name of a PIDL (a file-system path for folders).
pub fn parsing_name(pidl: &[u8]) -> Option<String> {
    shell_name(pidl, SIGDN_DESKTOPABSOLUTEPARSING)
}

/// Desktop-absolute editing name of a PIDL (the `search-ms:` URL for a
/// search-results folder).
pub fn editing_name(pidl: &[u8]) -> Option<String> {
    shell_name(pidl, SIGDN_DESKTOPABSOLUTEEDITING)
}

fn shell_name(pidl: &[u8], sigdn: SIGDN) -> Option<String> {
    // SAFETY: `pidl` is a complete, terminated ITEMIDLIST copy.
    let name = unsafe { SHGetNameFromIDList(pidl.as_ptr().cast::<ITEMIDLIST>(), sigdn) }.ok()?;
    // SAFETY: the returned PWSTR is a valid, CoTaskMem-allocated string, freed once.
    let s = unsafe { name.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(name.0 as *const _)) };
    s
}

/// Percent-decodes `%XX` escapes as UTF-8 (lossy); other text is kept.
fn percent_decode_lossy(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

/// Whether `raw` is still a window (`IsWindow`) and a top-level Explorer window.
fn alive(raw: isize) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd_of(raw))) }.as_bool() && is_cabinet_window(hwnd_of(raw))
}

/// Closes the window with all of its tabs (`WM_SYSCOMMAND SC_CLOSE`), then
/// posts `WM_CLOSE` repeatedly (pumping messages) until the window is gone or
/// `timeout` passes. Returns whether the window is gone.
fn close_fully(raw: isize, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    // SAFETY: posting a message; the HWND was checked to be a cabinet window.
    unsafe {
        let _ = PostMessageW(Some(hwnd_of(raw)), WM_SYSCOMMAND, WPARAM(SC_CLOSE as usize), LPARAM(0));
    }
    let first_end = (Instant::now() + Duration::from_secs(3)).min(end);
    while Instant::now() < first_end && alive(raw) {
        pump_once();
        std::thread::sleep(Duration::from_millis(10));
    }
    while alive(raw) {
        if Instant::now() >= end {
            return false;
        }
        // SAFETY: posting a message; the HWND was checked to be a cabinet window.
        unsafe {
            let _ = PostMessageW(Some(hwnd_of(raw)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        let step_end = (Instant::now() + CLOSE_STEP).min(end);
        while Instant::now() < step_end && alive(raw) {
            pump_once();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    true
}

/// Lower-case path without a `\\?\` prefix or a trailing separator.
fn normalize(path: &str) -> String {
    let p = path.strip_prefix(r"\\?\").unwrap_or(path);
    p.trim_end_matches('\\').to_lowercase()
}

pub struct ExplorerGuard {
    name: String,
    root: PathBuf,
    /// Normalized forms of `root` (as given and canonicalized).
    root_keys: Vec<String>,
    pre: HashSet<isize>,
    adopted: RefCell<Vec<isize>>,
    done: bool,
    /// Held from `new` until cleanup has finished.
    serial: Option<SerialLock>,
}

impl ExplorerGuard {
    /// Waits for any other guarded test, then records the pre-test windows and
    /// creates `%TEMP%\esr_spike_<name>_<pid>`.
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("esr_spike_{name}_{}", std::process::id()));
        Self::new_at(name, root)
    }

    /// Like [`new`](Self::new), but the temp root is `%TEMP%\esr_spike_<root_name>`
    /// whatever the process, so repeated runs show Explorer the same folder
    /// paths. Explorer 22621's sync-status cache crashes Explorer once 128
    /// distinct folders were viewed within 5 minutes (it never evicts), so
    /// tests that open many folders and are run back to back must not use
    /// fresh paths every time. The serial lock keeps two guards from using
    /// the root at once.
    pub fn with_stable_root(name: &str, root_name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("esr_spike_{root_name}"));
        Self::new_at(name, root)
    }

    fn new_at(name: &str, root: PathBuf) -> Self {
        let serial = SerialLock::acquire(name);
        let pre: HashSet<isize> = top_level_cabinets().into_iter().collect();
        std::fs::create_dir_all(&root).expect("create the spike temp root");
        let mut root_keys = vec![normalize(&root.to_string_lossy())];
        if let Ok(c) = std::fs::canonicalize(&root) {
            let k = normalize(&c.to_string_lossy());
            if !root_keys.contains(&k) {
                root_keys.push(k);
            }
        }
        println!(
            "[guard {name}] {} pre-existing Explorer window(s) (never touched); temp root {}",
            pre.len(),
            root.display()
        );
        Self {
            name: name.to_string(),
            root,
            root_keys,
            pre,
            adopted: RefCell::new(Vec::new()),
            done: false,
            serial: Some(serial),
        }
    }

    /// The test's temp root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates (if needed) and returns `root\rel`.
    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(&p).expect("create a spike temp folder");
        p
    }

    /// Whether `hwnd` existed before the test started.
    pub fn is_pre_existing(&self, hwnd: HWND) -> bool {
        self.pre.contains(&(hwnd.0 as isize))
    }

    /// Whether a desktop-absolute parsing name lies under the temp root.
    pub fn is_under_root(&self, parsing_name: &str) -> bool {
        let n = normalize(parsing_name);
        self.root_keys
            .iter()
            .any(|k| n == *k || n.starts_with(&format!("{k}\\")))
    }

    /// Whether a tab's current folder PIDL shows something under the temp
    /// root: its parsing name is under the root, or its editing name is a
    /// `search-ms:` URL whose scope is under the root or whose text (raw or
    /// percent-decoded) mentions the root.
    pub fn pidl_references_root(&self, pidl: &[u8]) -> bool {
        if parsing_name(pidl).is_some_and(|n| self.is_under_root(&n)) {
            return true;
        }
        let Some(edit) = editing_name(pidl) else {
            return false;
        };
        if self.is_under_root(&edit) {
            return true;
        }
        if !edit.to_ascii_lowercase().starts_with("search-ms:") {
            return false;
        }
        let url: esr_core::model::WideString = edit.as_str().into();
        if let Ok(p) = esr_core::search::parse_search_ms(&url)
            && self.is_under_root(&p.scope.to_string_lossy())
        {
            return true;
        }
        let decoded = percent_decode_lossy(&edit).to_lowercase();
        let raw = edit.to_lowercase();
        self.root_keys
            .iter()
            .any(|k| raw.contains(k.as_str()) || decoded.contains(k.as_str()))
    }

    /// Registers a window created by the test so cleanup closes it.
    ///
    /// Panics if the window existed before the test.
    pub fn adopt(&self, hwnd: HWND) {
        let raw = hwnd.0 as isize;
        assert!(
            !self.pre.contains(&raw),
            "[guard {}] refusing to adopt pre-existing window {hwnd:?}",
            self.name
        );
        let mut adopted = self.adopted.borrow_mut();
        if !adopted.contains(&raw) {
            adopted.push(raw);
            println!("[guard {}] adopted window {hwnd:?}", self.name);
        }
    }

    pub fn is_adopted(&self, hwnd: HWND) -> bool {
        self.adopted.borrow().contains(&(hwnd.0 as isize))
    }

    /// Closes an adopted window now (all of its tabs). Returns whether it is gone.
    ///
    /// Panics if the window was not adopted.
    pub fn close(&self, hwnd: HWND) -> bool {
        assert!(
            self.is_adopted(hwnd),
            "[guard {}] close() of a window that was not adopted: {hwnd:?}",
            self.name
        );
        let gone = close_fully(hwnd.0 as isize, CLOSE_TIMEOUT);
        if !gone {
            eprintln!("[guard {}] window {hwnd:?} did not close", self.name);
        }
        gone
    }

    /// Windows not in the pre-test set that show a folder under the temp root
    /// (including searches scoped to it, see [`Self::pidl_references_root`]).
    fn strays(&self) -> Vec<isize> {
        let Ok(groups) = enumerate_all_explorer_windows() else {
            return Vec::new();
        };
        groups
            .into_iter()
            .filter(|g| !self.pre.contains(&(g.top_hwnd.0 as isize)))
            .filter(|g| {
                g.tabs.iter().any(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .is_some_and(|p| self.pidl_references_root(&p))
                })
            })
            .map(|g| g.top_hwnd.0 as isize)
            .collect()
    }

    /// Closes everything, deletes the temp root and returns the leftovers.
    fn cleanup(&mut self) -> Vec<String> {
        self.done = true;
        // Nested STA init is fine (S_FALSE); it keeps ShellWindows usable even if
        // the test's own StaGuard is already gone.
        let _sta = StaGuard::init().ok();
        let tag = format!("[guard {}]", self.name);
        let adopted = self.adopted.borrow().clone();
        for &raw in &adopted {
            if alive(raw) {
                let gone = close_fully(raw, CLOSE_TIMEOUT);
                println!(
                    "{tag} adopted window {:?}: {}",
                    hwnd_of(raw),
                    if gone { "closed" } else { "STILL OPEN" }
                );
            }
        }
        // Sweep (a few passes: closing a window can reveal a transient one).
        let mut quiet_passes = 0;
        for _ in 0..6 {
            pump_for(Duration::from_millis(400));
            let strays = self.strays();
            if strays.is_empty() {
                quiet_passes += 1;
                if quiet_passes >= 2 {
                    break;
                }
                continue;
            }
            quiet_passes = 0;
            for raw in strays {
                let gone = close_fully(raw, CLOSE_TIMEOUT);
                println!(
                    "{tag} swept window {:?} under the temp root: {}",
                    hwnd_of(raw),
                    if gone { "closed" } else { "STILL OPEN" }
                );
            }
        }

        let mut leftovers: Vec<String> = Vec::new();
        for &raw in &adopted {
            if alive(raw) {
                leftovers.push(format!("adopted window {:?} is still open", hwnd_of(raw)));
            }
        }
        for raw in self.strays() {
            if !adopted.contains(&raw) {
                leftovers.push(format!(
                    "window {:?} under {} is still open",
                    hwnd_of(raw),
                    self.root.display()
                ));
            }
        }
        for _ in 0..20 {
            if !self.root.exists() || std::fs::remove_dir_all(&self.root).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if self.root.exists() {
            leftovers.push(format!(
                "temp root {} could not be deleted",
                self.root.display()
            ));
        }
        if leftovers.is_empty() {
            println!("{tag} cleanup complete: no test windows left");
        }
        // The next guarded test may start now.
        self.serial = None;
        leftovers
    }

    /// Runs cleanup and fails if any window created during the test remains.
    pub fn finish(mut self) -> Result<(), String> {
        let leftovers = self.cleanup();
        if leftovers.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "[guard {}] leftovers: {}",
                self.name,
                leftovers.join("; ")
            ))
        }
    }
}

impl Drop for ExplorerGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let leftovers = self.cleanup();
        if !leftovers.is_empty() {
            eprintln!(
                "[guard {}] CLEANUP INCOMPLETE (close manually): {}",
                self.name,
                leftovers.join("; ")
            );
        }
    }
}
