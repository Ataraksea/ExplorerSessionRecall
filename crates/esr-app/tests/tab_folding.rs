//! Tab folding against real File Explorer (Requirement 14; tasks 27.1-27.7).
//! This file holds the harness (task 27.1) and a smoke test; the folding
//! tests of tasks 27.2-27.7 are added below the harness and use it through
//! [`Harness`].
//!
//! ```text
//! cargo test -p esr-app --test tab_folding -- --ignored --nocapture --test-threads=1
//! ```
//!
//! All tests are `#[ignore]`: they open and close real Explorer windows and
//! share the one real Explorer of the machine, so they run serially (on the
//! Windows VMs) and never in a plain `cargo test`. Do not use the keyboard or
//! mouse while they run.
//!
//! # Harness
//!
//! [`Harness::start`] does, in this order:
//!
//! 1. takes an [`ExplorerGuard`] (named mutex, stable temp root so that
//!    repeated runs show Explorer the same folder paths, see the guard's
//!    docs on the sync-status cache), and **skips** the test (a printed
//!    reason, no failure) when an Explorer window is already open, when a
//!    real Explorer Session Recall instance is running, or when there is no
//!    shell; windows that were open before are never touched;
//! 2. opens one window on a temp folder: the Primary_Window. It exists
//!    before the runtime starts, so it is never a candidate itself;
//! 3. starts the runtime in-process on its own thread, as `restore_timing.rs`
//!    does: a temporary data directory, no Session_File, `ui: false`,
//!    `panic_guard: false`, the settings the test passes (folding is on in
//!    `Settings::default()`), and waits for a checkpoint that lists the
//!    Primary_Window, so the Live_Session tracks it before the test acts.
//!
//! The harness then offers:
//!
//! * **Opening a folder** by each way a user or an application does
//!   ([`OpenMethod`], [`open_folder`]): `explorer.exe <dir>`,
//!   `explorer.exe /select,<file>`, `SHOpenFolderAndSelectItems`, and
//!   `ShellExecute("open")` (a desktop double-click). Each returns a
//!   [`Launch`] with the instant of the call, for completion times.
//! * **Reading the result**: [`Harness::view`] reads a window's tabs and the
//!   selected items of each through `IShellWindows`
//!   (`IFolderView2::GetSelection`), and its tab captions and active tab
//!   through UI Automation (the product's own `TabStripReader`, which the
//!   `esr-win` test helper wraps as `Uia`). [`Harness::locate`] and
//!   [`Harness::wait_for_tab`] find a folder across all windows.
//! * **Visibility**: [`Harness::sampler`] starts the ~1 ms visibility
//!   sampler that `restore_timing.rs` uses (`fold_spike::Sampler`, shown =
//!   visible, not cloaked, not minimized, not layered alpha 0, on screen)
//!   over every window that is not open now; [`max_shown_ms`] and
//!   [`describe_tracks`] read its result.
//! * **The log**: [`Harness::log`] and [`Harness::fold_entries`] parse the
//!   temporary `activity.log`.
//! * **Cleanup**: [`Harness::conclude`] adopts every window the test caused,
//!   stops the runtime with `ESR_EXIT` (sent to this process's own Engine
//!   window only), lets the guard close what is left, deletes the data
//!   directory, and only then raises the test's error. A failing or
//!   panicking test still closes its windows (the guard's `Drop`).

#![cfg(windows)]
#![allow(dead_code)]

/// `esr-win`'s Windows test helpers: `explorer_guard` (the guard, message
/// pumping, window listing) and `fold_spike` (launchers, the visibility
/// sampler, UIA tab strip access).
#[path = "../../esr-win/tests/common/mod.rs"]
mod win_common;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use esr_app::instance::{ENGINE_WINDOW_CLASS, InstanceMessages};
use esr_app::runtime::{RuntimeOptions, Storage, init_process, run, startup_input};
use esr_core::engine::SessionFileRead;
use esr_core::format::{FORMAT_VERSION, parse, serialize};
use esr_core::logfmt::entry::{ParsedLine, parse_line};
use esr_core::model::{
    DisplayRef, Placement, Rect, SaveCause, SessionDoc, ShowState as SavedShowState, Snapshot, Status, TabRec,
    Timestamp, WideString, WindowId, WindowRec, validate_doc,
};
use esr_core::settings::Settings;
use esr_win::display_info::displays;
use esr_win::location::try_capture_from_parsing_name;
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use win_common::explorer_guard::{ExplorerGuard, parsing_name, pump_once, top_level_cabinets};
use win_common::fold_spike::{
    Sampler, Track, Uia, browsers_of, open_folder_and_select, open_primary, selection, shell_execute,
    spawn_explorer,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetShellWindow, GetWindowThreadProcessId, PostMessageW,
};
use windows::core::{HSTRING, PCWSTR};

/// How long a launched folder has to show up somewhere.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Req 14.16: a candidate may be visible for at most this long in total.
pub const MAX_CANDIDATE_VISIBLE_MS: f64 = 100.0;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Lower-case path without a `\\?\` prefix or a trailing separator.
pub fn normalize(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    p.trim_end_matches('\\').to_lowercase()
}

/// The leaf of a path: the tab caption Explorer 22621 shows for a folder.
pub fn leaf(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Pumps messages until `f` yields or `timeout` passes.
pub fn wait<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + timeout;
    loop {
        pump_once();
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Pumps messages for `d`.
pub fn settle(d: Duration) {
    let _ = wait(d, || None::<()>);
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

/// `ESR.Engine` windows as (hwnd, owning pid).
fn engine_windows() -> Vec<(isize, u32)> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: plain window enumeration by class.
        let Ok(hwnd) = (unsafe { FindWindowExW(None, after, &class, PCWSTR::null()) }) else {
            return out;
        };
        if hwnd.is_invalid() {
            return out;
        }
        let mut pid = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        out.push((hwnd.0 as isize, pid));
        after = Some(hwnd);
    }
}

fn own_engine_window() -> Option<isize> {
    engine_windows().into_iter().find(|&(_, pid)| pid == std::process::id()).map(|(h, _)| h)
}

// ---------------------------------------------------------------------------
// Opening a folder (Req 14.1, 14.16: every way of opening a folder)
// ---------------------------------------------------------------------------

/// The ways a folder is opened that the product folds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMethod {
    /// `explorer.exe "<dir>"` as its own process (a shortcut, another app).
    ExplorerExe,
    /// `explorer.exe /select,"<file>"` (needs a file).
    ExplorerSelect,
    /// `SHOpenFolderAndSelectItems(dir, [file])` (needs a file): the "Show in
    /// folder" of most applications.
    ShOpenFolderAndSelectItems,
    /// `ShellExecuteExW("open", dir)`: a desktop-style double-click.
    ShellExecuteOpen,
}

impl OpenMethod {
    pub const ALL: [OpenMethod; 4] = [
        OpenMethod::ExplorerExe,
        OpenMethod::ExplorerSelect,
        OpenMethod::ShOpenFolderAndSelectItems,
        OpenMethod::ShellExecuteOpen,
    ];

    /// Whether the method selects a file in the folder.
    pub fn selects(self) -> bool {
        matches!(self, OpenMethod::ExplorerSelect | OpenMethod::ShOpenFolderAndSelectItems)
    }

    pub fn label(self) -> &'static str {
        match self {
            OpenMethod::ExplorerExe => "explorer.exe <dir>",
            OpenMethod::ExplorerSelect => "explorer.exe /select,<file>",
            OpenMethod::ShOpenFolderAndSelectItems => "SHOpenFolderAndSelectItems",
            OpenMethod::ShellExecuteOpen => "ShellExecute open",
        }
    }
}

/// A folder open that was requested.
pub struct Launch {
    pub method: OpenMethod,
    /// Just before the request was made (the start of completion times).
    pub at: Instant,
    /// The in-process methods run on their own STA thread; this is its result
    /// (the call's duration in ms).
    call: Option<JoinHandle<Result<f64, String>>>,
}

impl Launch {
    /// Waits for the in-process call to return; `None` for `explorer.exe`
    /// launches (the child process is not waited for).
    pub fn call_result(&mut self) -> Option<Result<f64, String>> {
        self.call.take().map(|h| h.join().unwrap_or_else(|_| Err("launcher thread panicked".into())))
    }
}

/// Opens `dir` the way `method` does. `file` (inside `dir`) is required for
/// the methods that select ([`OpenMethod::selects`]).
pub fn open_folder(method: OpenMethod, dir: &Path, file: Option<&Path>) -> Result<Launch, String> {
    let need_file = || file.ok_or_else(|| format!("{} needs a file to select", method.label()));
    let at = Instant::now();
    let call = match method {
        OpenMethod::ExplorerExe => {
            spawn_explorer(&format!("\"{}\"", dir.display()))?;
            None
        }
        OpenMethod::ExplorerSelect => {
            spawn_explorer(&format!("/select,\"{}\"", need_file()?.display()))?;
            None
        }
        OpenMethod::ShOpenFolderAndSelectItems => Some(open_folder_and_select(dir, need_file()?)),
        OpenMethod::ShellExecuteOpen => Some(shell_execute(dir, Some("open"))),
    };
    Ok(Launch { method, at, call })
}

// ---------------------------------------------------------------------------
// Reading windows: IShellWindows and UI Automation
// ---------------------------------------------------------------------------

/// One tab as `IShellWindows` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabView {
    /// Normalized parsing name of the tab's folder (`<unknown>` if unreadable).
    pub location: String,
    /// Normalized parsing names of the items selected in the tab's view.
    pub selection: Vec<String>,
}

/// A window's tabs (ShellWindows order) and its UIA tab strip.
#[derive(Clone, Debug)]
pub struct WindowView {
    pub top: isize,
    pub tabs: Vec<TabView>,
    /// Tab captions in strip order (display names on 22621, parsing names on
    /// 26200/26300; only the exposed items when the strip overflows).
    pub uia_names: Vec<String>,
    /// Index of the selected (active) tab among `uia_names`.
    pub uia_selected: Option<usize>,
}

impl WindowView {
    /// The normalized locations of the tabs, in ShellWindows order.
    pub fn locations(&self) -> Vec<String> {
        self.tabs.iter().map(|t| t.location.clone()).collect()
    }

    /// Index (ShellWindows order) of the tab showing `dir`.
    pub fn tab_of(&self, dir: &Path) -> Option<usize> {
        let want = normalize(&dir.to_string_lossy());
        self.tabs.iter().position(|t| t.location == want)
    }

    /// The active tab's caption, when the strip exposes it.
    pub fn active_caption(&self) -> Option<&str> {
        self.uia_selected.and_then(|i| self.uia_names.get(i)).map(String::as_str)
    }

    /// Whether the active tab's caption is the leaf of `dir` (or its path).
    pub fn active_is(&self, dir: &Path) -> bool {
        self.active_caption()
            .is_some_and(|c| c.eq_ignore_ascii_case(&leaf(dir)) || normalize(c) == normalize(&dir.to_string_lossy()))
    }
}

fn tab_views(top: isize) -> Vec<TabView> {
    browsers_of(top)
        .into_iter()
        .map(|(_, browser)| TabView {
            location: current_folder_pidl(&browser)
                .ok()
                .and_then(|p| parsing_name(&p))
                .map(|n| normalize(&n))
                .unwrap_or_else(|| "<unknown>".into()),
            selection: selection(&browser)
                .iter()
                .filter_map(|p| parsing_name(p))
                .map(|n| normalize(&n))
                .collect(),
        })
        .collect()
}

/// Every top-level Explorer window with the normalized location of each tab
/// (ShellWindows order), without selections or UIA.
pub fn windows_with_locations() -> Vec<(isize, Vec<String>)> {
    enumerate_all_explorer_windows()
        .unwrap_or_default()
        .into_iter()
        .map(|g| {
            let names = g
                .tabs
                .iter()
                .map(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .map(|n| normalize(&n))
                        .unwrap_or_else(|| "<unknown>".into())
                })
                .collect();
            (g.top_hwnd.0 as isize, names)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Visibility
// ---------------------------------------------------------------------------

/// The longest total time any sampled window was shown, in ms.
pub fn max_shown_ms(tracks: &HashMap<isize, Track>, end: Instant) -> f64 {
    tracks.values().map(|t| t.shown_ms(end)).fold(0.0, f64::max)
}

/// One printable line per sampled window.
pub fn describe_tracks(tracks: &HashMap<isize, Track>, end: Instant) -> Vec<String> {
    tracks
        .iter()
        .map(|(raw, t)| {
            format!(
                "window {:?}: shown {:.0} ms in {} interval(s), {} samples, max gap {:?}",
                hwnd_of(*raw),
                t.shown_ms(end),
                t.shown.len(),
                t.samples,
                t.max_gap
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// In-process runtime
// ---------------------------------------------------------------------------

/// The runtime on its own thread (the Engine STA thread), as in
/// `restore_timing.rs`: no UI, no panic guard.
pub struct Runtime {
    thread: Option<JoinHandle<i32>>,
    engine: Option<isize>,
}

impl Runtime {
    /// Starts the runtime on `data_dir` with `settings` and no Session_File.
    pub fn start(data_dir: &Path, settings: Settings) -> Self {
        Self::start_inner(data_dir, settings, false)
    }

    /// Starts the runtime on `data_dir` with `settings`, reading the
    /// Session_File that `data_dir` holds (a Pending_Snapshot to restore),
    /// as `restore_timing.rs` does.
    pub fn start_with_session_file(data_dir: &Path, settings: Settings) -> Self {
        Self::start_inner(data_dir, settings, true)
    }

    fn start_inner(data_dir: &Path, settings: Settings, read_session: bool) -> Self {
        let dir = data_dir.to_path_buf();
        let thread = std::thread::Builder::new()
            .name("esr-engine-test".into())
            .spawn(move || {
                let storage = Storage::open(&dir);
                let session = if read_session { storage.read_session() } else { SessionFileRead::Absent };
                let input = startup_input(session, false);
                let options = RuntimeOptions { ui: false, panic_guard: false, ..RuntimeOptions::default() };
                run(settings, input, storage, options)
            })
            .expect("spawn the runtime");
        let engine = wait(Duration::from_secs(10), own_engine_window);
        Self { thread: Some(thread), engine }
    }

    pub fn has_engine_window(&self) -> bool {
        self.engine.is_some()
    }

    /// Posts `ESR_EXIT` to this process's Engine window and waits for the
    /// runtime to return.
    pub fn stop(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        if let Some(engine) = self.engine {
            let exit = InstanceMessages::register().exit;
            // SAFETY: posting a registered message to this process's own window.
            let _ = unsafe { PostMessageW(Some(hwnd_of(engine)), exit, WPARAM(0), LPARAM(0)) };
        }
        if wait(Duration::from_secs(15), || thread.is_finished().then_some(())).is_none() {
            return Err("the runtime did not exit after ESR_EXIT".into());
        }
        match thread.join() {
            Ok(0) => Ok(()),
            Ok(code) => Err(format!("the runtime exited with code {code}")),
            Err(_) => Err("the runtime thread panicked".into()),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// What [`Harness::start`] produced.
pub enum Started {
    Ready(Box<Harness>),
    /// A precondition does not hold; nothing was opened or touched.
    Skipped(String),
}

/// One test's Explorer, runtime and helpers (see the module docs). Windows
/// are closed by [`conclude`](Self::conclude), or by the fields' `Drop`s when
/// a test panics before reaching it.
pub struct Harness {
    pub name: String,
    pub guard: ExplorerGuard,
    pub data_dir: PathBuf,
    pub runtime: Runtime,
    pub uia: Uia,
    /// The Primary_Window and the folder it was opened on. `0` and an empty
    /// path when the harness was started with a Pending Session_File
    /// ([`Harness::start_with_session`]): no window exists before the test
    /// acts, so there is no Primary_Window.
    pub primary: isize,
    pub primary_dir: PathBuf,
    /// The windows of the Pending Session_File as normalized tab locations,
    /// left to right (empty unless started with one).
    pub saved: Vec<Vec<String>>,
    /// Keeps COM initialised on the test thread for the whole test.
    _sta: StaGuard,
}

/// Builds the Pending Session_File of [`Harness::start_with_session`]: one
/// window per entry of `windows`, one tab per folder name (a temp folder
/// `root\<name>`, created), the first tab active. Returns the document and the
/// normalized tab locations per window.
fn pending_session(guard: &ExplorerGuard, windows: &[Vec<String>]) -> Result<(SessionDoc, Vec<Vec<String>>), String> {
    let all = displays();
    let primary = all.iter().find(|d| d.primary).or(all.first()).ok_or("no display")?.clone();
    let work = primary.work;
    let width = (work.width() - 240).clamp(400, 1500) as i32;
    let height = (work.height() - 160).clamp(300, 850) as i32;
    let mut recs = Vec::new();
    let mut locations = Vec::new();
    for (i, names) in windows.iter().enumerate() {
        let (left, top) = (work.left + 40 + 60 * i as i32, work.top + 30 + 50 * i as i32);
        let mut tabs = Vec::new();
        let mut paths = Vec::new();
        for name in names {
            let dir = guard.dir(name);
            let text = dir.to_str().ok_or("non-UTF-8 temp path")?;
            let location = try_capture_from_parsing_name(&WideString::from(text))
                .map_err(|e| format!("capture {}: {e:?}", dir.display()))?;
            paths.push(normalize(&location.parsing_name.to_string_lossy()));
            tabs.push(TabRec { location });
        }
        recs.push(WindowRec {
            id: WindowId(i as u32 + 1),
            tabs,
            active: 0,
            placement: Placement {
                state: SavedShowState::Normal,
                restored: Rect { left, top, right: left + width, bottom: top + height },
                display: DisplayRef { device: primary.device.clone(), monitor: primary.monitor },
            },
        });
        locations.push(paths);
    }
    let activation = recs.iter().map(|w| w.id).collect();
    let doc = SessionDoc {
        version: FORMAT_VERSION,
        status: Status::Pending,
        failed_attempts: 0,
        saved_at: Timestamp {
            year: 2025,
            month: 1,
            day: 31,
            hour: 9,
            minute: 15,
            second: 2,
            millis: 123,
            offset_minutes: 0,
        },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot { windows: recs, activation },
    };
    validate_doc(&doc).map_err(|e| format!("invalid test snapshot: {e}"))?;
    Ok((doc, locations))
}

impl Harness {
    /// Starts a harness with the default settings (tab folding on).
    pub fn start_default(name: &str) -> Started {
        Self::start(name, Settings::default())
    }

    /// Starts a harness for test `name` with `settings` (see the module docs
    /// for the steps). A setup failure panics; unmet preconditions skip.
    pub fn start(name: &str, settings: Settings) -> Started {
        Self::start_with(name, settings, None)
    }

    /// Like [`start`](Self::start), but for a restoration: no Primary_Window is
    /// opened, the runtime starts with a Pending Session_File holding
    /// `windows` (one window per entry, one tab per folder name, see
    /// [`pending_session`]), and the test opens "File Explorer" itself to
    /// trigger the restoration. No Explorer window exists when this returns.
    pub fn start_with_session(name: &str, settings: Settings, windows: Vec<Vec<String>>) -> Started {
        Self::start_with(name, settings, Some(windows))
    }

    fn start_with(name: &str, settings: Settings, session: Option<Vec<Vec<String>>>) -> Started {
        init_process(); // per-monitor DPI v2, as the product
        let sta = StaGuard::init().expect("STA init");
        let guard = ExplorerGuard::with_stable_root(&format!("tf_{name}"), "tab_folding");

        // Preconditions (never touch what is already open).
        let open = top_level_cabinets();
        if !open.is_empty() {
            return Started::Skipped(format!(
                "{} non-test Explorer window(s) are open; the harness needs to start with none (they were not touched)",
                open.len()
            ));
        }
        let others: Vec<u32> =
            engine_windows().into_iter().map(|(_, p)| p).filter(|&p| p != std::process::id()).collect();
        if !others.is_empty() {
            return Started::Skipped(format!("an Explorer Session Recall instance is running (pid {others:?})"));
        }
        // SAFETY: plain read.
        if unsafe { GetShellWindow() }.is_invalid() {
            return Started::Skipped("no shell (explorer.exe) is running".into());
        }

        let data_dir = std::env::temp_dir().join(format!("esr_tab_folding_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let (runtime, primary, primary_dir, saved) = match session {
            None => {
                // The Primary_Window, before the runtime starts, then the
                // runtime, tracking it.
                let primary_dir = guard.dir("primary");
                let primary = open_primary(&guard, &primary_dir)
                    .unwrap_or_else(|e| panic!("opening the Primary_Window: {e}"));
                let runtime = Runtime::start(&data_dir, settings);
                assert!(runtime.has_engine_window(), "the runtime created no ESR.Engine window");
                let listed = wait(Duration::from_secs(10), || {
                    read_doc(&data_dir).filter(|d| doc_lists(d, &primary_dir))
                });
                assert!(listed.is_some(), "no Session_File checkpoint lists the Primary_Window");
                (runtime, primary, primary_dir, Vec::new())
            }
            Some(windows) => {
                // A Pending Session_File and no window: the runtime waits
                // for a Trigger_Window.
                let (doc, saved) = pending_session(&guard, &windows)
                    .unwrap_or_else(|e| panic!("building the Pending Session_File: {e}"));
                std::fs::create_dir_all(&data_dir)
                    .and_then(|()| std::fs::write(data_dir.join("session.esr"), serialize(&doc)))
                    .unwrap_or_else(|e| panic!("writing the Session_File: {e}"));
                let runtime = Runtime::start_with_session_file(&data_dir, settings);
                assert!(runtime.has_engine_window(), "the runtime created no ESR.Engine window");
                (runtime, 0, PathBuf::new(), saved)
            }
        };
        let uia = Uia::new().unwrap_or_else(|e| panic!("UI Automation: {e}"));
        // Let the runtime attach its hooks and settle before the test acts.
        settle(Duration::from_millis(500));
        Started::Ready(Box::new(Self {
            name: name.to_string(),
            guard,
            data_dir,
            runtime,
            uia,
            primary,
            primary_dir,
            saved,
            _sta: sta,
        }))
    }

    /// A temp folder `root\rel` (created).
    pub fn dir(&self, rel: &str) -> PathBuf {
        self.guard.dir(rel)
    }

    /// A temp folder `root\rel` holding an empty file `file` (for the methods
    /// that select); returns (folder, file path).
    pub fn dir_with_file(&self, rel: &str, file: &str) -> (PathBuf, PathBuf) {
        let dir = self.guard.dir(rel);
        let path = dir.join(file);
        std::fs::write(&path, b"x").unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
        (dir, path)
    }

    /// Opens `dir` with `method`; see [`open_folder`].
    pub fn open(&self, method: OpenMethod, dir: &Path, file: Option<&Path>) -> Result<Launch, String> {
        open_folder(method, dir, file)
    }

    /// The top-level Explorer windows now (the Primary_Window included).
    pub fn windows(&self) -> Vec<isize> {
        top_level_cabinets()
    }

    /// Top-level windows other than the Primary_Window.
    pub fn other_windows(&self) -> Vec<isize> {
        top_level_cabinets().into_iter().filter(|&h| h != self.primary).collect()
    }

    /// Adopts every window that is open now (the guard closes them at the
    /// end). Windows that existed before the test cannot be adopted and are
    /// skipped; none exist (the harness starts without any).
    pub fn adopt_all(&self) {
        for raw in top_level_cabinets() {
            if !self.guard.is_pre_existing(hwnd_of(raw)) {
                self.guard.adopt(hwnd_of(raw));
            }
        }
    }

    /// Starts the visibility sampler over every window not open now, so over
    /// the candidates and windows the test causes (not the Primary_Window).
    /// `Sampler::stop` returns the tracks per window.
    pub fn sampler(&self) -> Sampler {
        Sampler::start(top_level_cabinets().into_iter().collect::<HashSet<_>>())
    }

    /// A window's tabs and selections (IShellWindows) and its tab strip (UIA).
    pub fn view(&self, top: isize) -> WindowView {
        WindowView {
            top,
            tabs: tab_views(top),
            uia_names: self.uia.names(top),
            uia_selected: self.uia.selected(top),
        }
    }

    /// (window, tab index) of the first tab showing `dir`, over all windows.
    pub fn locate(&self, dir: &Path) -> Option<(isize, usize)> {
        let want = normalize(&dir.to_string_lossy());
        windows_with_locations()
            .into_iter()
            .find_map(|(top, locs)| locs.iter().position(|l| *l == want).map(|i| (top, i)))
    }

    /// Waits until some tab shows `dir`; returns where and when it was seen.
    pub fn wait_for_tab(&self, dir: &Path, timeout: Duration) -> Option<(isize, usize, Instant)> {
        wait(timeout, || self.locate(dir).map(|(top, i)| (top, i, Instant::now())))
    }

    /// Waits until the Primary_Window has a tab showing `dir`; returns the
    /// tab index and when it was seen.
    pub fn wait_for_primary_tab(&self, dir: &Path, timeout: Duration) -> Option<(usize, Instant)> {
        let want = normalize(&dir.to_string_lossy());
        wait(timeout, || {
            windows_with_locations()
                .into_iter()
                .find(|(top, _)| *top == self.primary)
                .and_then(|(_, locs)| locs.iter().position(|l| *l == want))
                .map(|i| (i, Instant::now()))
        })
    }

    /// The Activity_Log, one parsed line per entry (empty if none yet).
    pub fn log(&self) -> Vec<ParsedLine> {
        std::fs::read_to_string(self.data_dir.join("activity.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(parse_line)
            .collect()
    }

    /// The raw Activity_Log text, for failure reports.
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(self.data_dir.join("activity.log")).unwrap_or_default()
    }

    /// The `FOLD` entries of the Activity_Log (Req 14.30).
    pub fn fold_entries(&self) -> Vec<ParsedLine> {
        self.log().into_iter().filter(|p| p.event == "FOLD").collect()
    }

    /// Ends the test: adopts every window the test caused, stops the runtime,
    /// closes the windows, deletes the data directory, then raises the
    /// test's own error (with the Activity_Log) or a cleanup error.
    pub fn conclude(self, result: Result<(), String>) {
        let Harness { name, guard, data_dir, mut runtime, uia, .. } = self;
        for raw in top_level_cabinets() {
            if !guard.is_pre_existing(hwnd_of(raw)) {
                guard.adopt(hwnd_of(raw));
            }
        }
        let log = std::fs::read_to_string(data_dir.join("activity.log")).unwrap_or_default();
        let stopped = runtime.stop();
        drop(uia);
        let cleanup = guard.finish();
        let _ = std::fs::remove_dir_all(&data_dir);
        println!("\n=== tab_folding {name} ===");
        if let Err(e) = &result {
            println!("FAILED: {e}\nactivity.log:\n{log}");
        }
        result.unwrap_or_else(|e| panic!("{name}: {e}"));
        stopped.unwrap_or_else(|e| panic!("{name}: {e}"));
        cleanup.unwrap_or_else(|e| panic!("{name}: test windows left open: {e}"));
        assert!(!data_dir.exists(), "temp data directory {} was not deleted", data_dir.display());
    }
}

fn read_doc(data_dir: &Path) -> Option<SessionDoc> {
    parse(&std::fs::read(data_dir.join("session.esr")).ok()?).ok()
}

fn doc_lists(doc: &SessionDoc, folder: &Path) -> bool {
    let target = normalize(&folder.to_string_lossy());
    doc.snapshot
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .any(|t| normalize(&t.location.parsing_name.to_string_lossy()) == target)
}

/// Runs a test body on a started harness, or reports the skip. Usage:
///
/// ```ignore
/// with_harness("name", Settings::default(), |h| { ...; Ok(()) });
/// ```
pub fn with_harness(name: &str, settings: Settings, body: impl FnOnce(&Harness) -> Result<(), String>) {
    match Harness::start(name, settings) {
        Started::Skipped(reason) => println!("SKIPPED {name}: {reason}"),
        Started::Ready(h) => {
            let result = body(&h);
            h.conclude(result);
        }
    }
}

// ---------------------------------------------------------------------------
// Smoke test (task 27.1)
// ---------------------------------------------------------------------------

/// Proves the harness: the runtime starts and tracks the Primary_Window, a
/// folder opened with `explorer.exe <dir>` shows up in some window, and the
/// window view (IShellWindows and UIA) can be read. It does not assert that
/// the folder was folded; the folding tests (tasks 27.2-27.7) do.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn harness_starts_the_runtime_and_opens_a_folder() {
    with_harness("smoke", Settings::default(), |h| {
        let primary = h.view(h.primary);
        println!("primary {:?}: {primary:?}", hwnd_of(h.primary));
        if primary.tab_of(&h.primary_dir).is_none() {
            return Err(format!("the Primary_Window does not show its folder: {primary:?}"));
        }

        let dir = h.dir("opened");
        let sampler = h.sampler();
        let mut launch = h.open(OpenMethod::ExplorerExe, &dir, None)?;
        let found = h.wait_for_tab(&dir, OPEN_TIMEOUT);
        settle(Duration::from_millis(1_000));
        let end = Instant::now();
        let tracks = sampler.stop();
        h.adopt_all();
        let _ = launch.call_result();

        let (top, tab, seen) = found.ok_or_else(|| {
            format!("{} did not open {} within {OPEN_TIMEOUT:?}", launch.method.label(), dir.display())
        })?;
        println!(
            "{} -> tab {tab} of window {:?} ({}) after {} ms",
            launch.method.label(),
            hwnd_of(top),
            if top == h.primary { "the Primary_Window" } else { "a separate window" },
            seen.duration_since(launch.at).as_millis()
        );
        for line in describe_tracks(&tracks, end) {
            println!("  {line}");
        }
        let view = h.view(top);
        println!("  view: {view:?}");
        println!("  FOLD log entries: {}", h.fold_entries().len());
        Ok(())
    });
}

// ===========================================================================
// Task 27.2: folds from each way of opening a folder
// (Req 14.1, 14.6, 14.16, 14.17, 14.18, 14.25)
// ===========================================================================
//
// One test per [`OpenMethod`], each on its own harness (so a failure names the
// way of opening), and one report test that repeats every method and prints
// how often a second `SC_CLOSE` looked necessary.
//
// What a run does (`fold_via`): starts the visibility sampler, opens a folder
// with the method, and polls (about every 20 ms) until the Primary_Window has
// a new tab on the folder, the candidate window is gone, the tab holds the
// requested selection (Req 14.6) and is the active tab. It then checks:
//
// * Req 14.1: the folder is a new tab of the Primary_Window (rightmost when
//   the tab strip shows it), no separate window is left, the log has one
//   `FOLD ... outcome=folded-primary` entry for the folder.
// * Req 14.6: for the methods that select, exactly the requested file is
//   selected in the new tab.
// * Req 14.16: the longest total time any window other than the
//   Primary_Window was shown is at most 100 ms.
// * Req 14.17: completion within 1.5 s of the candidate's first navigation.
//   No earlier candidate is still being folded (each run waits for the
//   previous one), and the Primary_Window is responsive, so neither the
//   +1.0 s per earlier candidate nor the fallback extensions of 14.18 and
//   14.25 apply to these runs; the 1.5 s bound is the one that is checked.
//
// Measurement limits (read before trusting a number):
//
// * "First navigation of the candidate" is the first poll that sees the
//   candidate window showing the folder through `IShellWindows`. If the
//   candidate is gone before a poll sees it, the first poll that sees the
//   folded tab is used instead (reported as `anchor=folded-tab`). Both can
//   only be later than the product's own instant, so completion times are
//   lower bounds on the product's true completion time, by up to one poll
//   (tens of ms, longer while Explorer is busy).
// * The sampler runs on its own thread at about 1 ms; a window shown for less
//   than that is not seen.
// * The FOLD entry does not say how many times `SC_CLOSE` was posted, and no
//   other log entry does either (`LogEntry::Fold` carries location and
//   outcome fields only; a candidate that ignores both posts is logged as
//   `ERROR operation=fold-close`). The count is therefore *derived*: the
//   engine posts the second `SC_CLOSE` exactly `FOLD_CLOSE_RETRY_MS` (2 s)
//   after the first, and the first is posted after the folded tab exists. So a
//   candidate that was gone less than 2 s after the folded tab was first seen
//   cannot have received a second post (`second SC_CLOSE: ruled out`); one
//   that outlived it by 2 s or more may have (`possible`). `possible` is an
//   upper bound, not a count of certain retries.
// * Nothing here has been run on a machine yet: the assertions and timings
//   are unverified until a Windows 11 VM run.

use esr_core::fold::FOLD_CLOSE_RETRY_MS;
use win_common::fold_spike::is_window;

/// Req 14.17: the base bound, from the candidate's first navigation.
pub const FOLD_BOUND_MS: f64 = 1_500.0;
/// How long after the first navigation the fold may take before the run gives
/// up (well above the bound, so slow folds are measured rather than missed).
const FOLD_GIVE_UP: Duration = Duration::from_secs(10);

fn ms_since(a: Instant, b: Instant) -> f64 {
    b.saturating_duration_since(a).as_secs_f64() * 1_000.0
}

/// What `fold_via` measured for one folded folder.
#[derive(Debug)]
pub struct FoldRun {
    pub method: OpenMethod,
    pub dir: PathBuf,
    pub file: Option<PathBuf>,
    /// Tabs of the Primary_Window before the folder was opened.
    pub tabs_before: usize,
    /// The candidate window, if a poll saw it.
    pub candidate: Option<isize>,
    /// Whether the anchor of `completion_ms` is the candidate's first
    /// navigation (`true`) or, when no poll saw it, the folded tab (`false`).
    pub anchor_is_first_nav: bool,
    /// From the launch call to the anchor.
    pub launch_to_anchor_ms: f64,
    /// From the anchor to: tab present, candidate gone, selection, active tab.
    pub completion_ms: f64,
    /// From the folded tab first seen to the candidate gone (0 if gone first).
    pub tab_to_gone_ms: f64,
    /// The `FOLD` entry's `outcome` for this folder.
    pub outcome: Option<String>,
    /// An `ERROR operation=fold-close` was logged (both posts ignored).
    pub close_error_logged: bool,
    /// The Primary_Window after the fold.
    pub primary: WindowView,
    /// Whether the new tab is the last caption of the strip (`None`: the
    /// strip exposes no captions).
    pub rightmost: Option<bool>,
    /// Longest total shown time of any sampled window.
    pub max_shown_ms: f64,
    pub track_lines: Vec<String>,
    /// Windows other than the Primary_Window left open.
    pub leftover_windows: usize,
}

impl FoldRun {
    /// Derived (see the section comment): could the candidate have received a
    /// second `SC_CLOSE`?
    pub fn second_close_possible(&self) -> bool {
        self.tab_to_gone_ms >= FOLD_CLOSE_RETRY_MS as f64
    }

    pub fn print(&self) {
        println!(
            "  {:<30} first nav {:>5.0} ms after launch (anchor={}); completed {:>5.0} ms after it; \
             candidate gone {:>5.0} ms after the folded tab; max shown {:>4.0} ms; second SC_CLOSE: {}{}",
            self.method.label(),
            self.launch_to_anchor_ms,
            if self.anchor_is_first_nav { "first-nav" } else { "folded-tab" },
            self.completion_ms,
            self.tab_to_gone_ms,
            self.max_shown_ms,
            if self.second_close_possible() { "possible" } else { "ruled out" },
            if self.close_error_logged { " (ERROR fold-close logged)" } else { "" },
        );
        for line in &self.track_lines {
            println!("      {line}");
        }
    }

    /// The Req 14.1, 14.6 and 14.16 checks, which hold whatever the timing;
    /// with `bound`, also the Req 14.17 completion bound.
    pub fn check(&self, bound: bool) -> Result<(), String> {
        let want = normalize(&self.dir.to_string_lossy());
        let label = self.method.label();
        match self.outcome.as_deref() {
            Some("folded-primary") => {}
            other => return Err(format!("{label}: the FOLD entry's outcome is {other:?}, want folded-primary (14.1)")),
        }
        if self.primary.tabs.len() != self.tabs_before + 1 {
            return Err(format!(
                "{label}: the Primary_Window has {} tab(s), want {} (14.1): {:?}",
                self.primary.tabs.len(),
                self.tabs_before + 1,
                self.primary.locations()
            ));
        }
        let Some(tab) = self.primary.tab_of(&self.dir) else {
            return Err(format!("{label}: no tab of the Primary_Window shows {want} (14.1)"));
        };
        if self.rightmost == Some(false) {
            return Err(format!("{label}: the new tab is not the rightmost: {:?} (14.1)", self.primary.uia_names));
        }
        if let (Some(file), true) = (&self.file, self.method.selects()) {
            let want_sel = vec![normalize(&file.to_string_lossy())];
            if self.primary.tabs[tab].selection != want_sel {
                return Err(format!(
                    "{label}: selection is {:?}, want exactly {want_sel:?} (14.6)",
                    self.primary.tabs[tab].selection
                ));
            }
        }
        if self.leftover_windows != 0 {
            return Err(format!("{label}: {} window(s) other than the Primary_Window are left open (14.1)", self.leftover_windows));
        }
        if self.max_shown_ms > MAX_CANDIDATE_VISIBLE_MS {
            return Err(format!(
                "{label}: a window was shown for {:.0} ms in total, at most {MAX_CANDIDATE_VISIBLE_MS} ms allowed (14.16): {:?}",
                self.max_shown_ms, self.track_lines
            ));
        }
        if bound && self.completion_ms > FOLD_BOUND_MS {
            return Err(format!(
                "{label}: Tab_Folding completed {:.0} ms after the first navigation, at most {FOLD_BOUND_MS} ms allowed \
                 (14.17); second SC_CLOSE {}",
                self.completion_ms,
                if self.second_close_possible() { "possible" } else { "ruled out" }
            ));
        }
        Ok(())
    }
}

/// Polling state of one `fold_via` run.
#[derive(Default)]
struct Progress {
    candidate: Option<isize>,
    first_nav: Option<Instant>,
    tab_seen: Option<Instant>,
    gone: Option<Instant>,
    done: Option<Instant>,
    last_view: Option<WindowView>,
}

/// Opens a new folder `rel` (with a file in it) with `method`, waits for the
/// fold, and measures it (see the section comment). `Err` when the folder was
/// not folded into the Primary_Window in time or the launch failed.
pub fn fold_via(h: &Harness, method: OpenMethod, rel: &str) -> Result<FoldRun, String> {
    let (dir, file_path) = h.dir_with_file(rel, "target.txt");
    let file = method.selects().then_some(file_path);
    let want = normalize(&dir.to_string_lossy());
    let want_selection = file.as_ref().map(|f| vec![normalize(&f.to_string_lossy())]);
    let tabs_before = windows_with_locations()
        .into_iter()
        .find(|(top, _)| *top == h.primary)
        .map_or(0, |(_, locs)| locs.len());

    let sampler = h.sampler();
    let mut launch = h.open(method, &dir, file.as_deref())?;
    let mut st = Progress::default();
    let finished = wait(OPEN_TIMEOUT.max(FOLD_GIVE_UP), || {
        let now = Instant::now();
        let wins = windows_with_locations();
        // The candidate: the window that is not the Primary_Window.
        if st.candidate.is_none() {
            st.candidate = h.other_windows().into_iter().next();
        }
        if st.first_nav.is_none() {
            let shows = |top: isize| wins.iter().any(|(t, l)| *t == top && l.contains(&want));
            if st.candidate.is_some_and(shows) {
                st.first_nav = Some(now);
            }
        }
        if st.tab_seen.is_none()
            && wins.iter().any(|(t, l)| *t == h.primary && l.contains(&want))
        {
            st.tab_seen = Some(now);
        }
        if st.gone.is_none() {
            let gone = match st.candidate {
                Some(c) => !is_window(c),
                // Never seen: gone as soon as the folded tab is.
                None => st.tab_seen.is_some(),
            };
            if gone {
                st.gone = Some(now);
            }
        }
        if st.tab_seen.is_some() && st.gone.is_some() {
            let view = h.view(h.primary);
            let selection_ok = match (&want_selection, view.tab_of(&dir)) {
                (Some(sel), Some(i)) => view.tabs[i].selection == *sel,
                (Some(_), None) => false,
                (None, _) => true,
            };
            // Captions are only checked when the strip exposes them.
            let active_ok = view.active_caption().is_none() || view.active_is(&dir);
            let ok = selection_ok && active_ok;
            st.last_view = Some(view);
            if ok {
                st.done = Some(Instant::now());
                return Some(());
            }
        }
        None
    });
    settle(Duration::from_millis(300)); // a late second window would show up here
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("{}: the call failed: {e}", method.label()));
    }
    if finished.is_none() {
        return Err(format!(
            "{}: {} was not folded into the Primary_Window with the required selection and active tab within \
             {FOLD_GIVE_UP:?}; candidate {:?}, last view {:?}",
            method.label(),
            dir.display(),
            st.candidate,
            st.last_view
        ));
    }

    let anchor = st.first_nav.or(st.tab_seen).ok_or("no anchor instant")?;
    let done = st.done.ok_or("no completion instant")?;
    let tab_seen = st.tab_seen.ok_or("no folded-tab instant")?;
    let gone = st.gone.ok_or("no candidate-gone instant")?;

    // The FOLD entry is written when the candidate's destruction is handled.
    let fold = wait(Duration::from_secs(3), || {
        h.fold_entries().into_iter().find(|p| p.get_str("location").is_some_and(|l| normalize(&l) == want))
    });
    let close_error_logged =
        h.log().iter().any(|p| p.event == "ERROR" && p.get_str("operation").as_deref() == Some("fold-close"));
    let primary = st.last_view.ok_or("no view of the Primary_Window")?;
    let rightmost = (!primary.uia_names.is_empty()).then(|| {
        primary.uia_names.last().is_some_and(|c| {
            c.eq_ignore_ascii_case(&leaf(&dir)) || normalize(c) == want
        })
    });

    Ok(FoldRun {
        method,
        file,
        tabs_before,
        candidate: st.candidate,
        anchor_is_first_nav: st.first_nav.is_some(),
        launch_to_anchor_ms: ms_since(launch.at, anchor),
        completion_ms: ms_since(anchor, done),
        tab_to_gone_ms: ms_since(tab_seen, gone),
        outcome: fold.and_then(|p| p.get_str("outcome")),
        close_error_logged,
        primary,
        rightmost,
        max_shown_ms: max_shown_ms(&tracks, end),
        track_lines: describe_tracks(&tracks, end),
        leftover_windows: h.other_windows().len(),
        dir,
    })
}

/// One folding test body: fold with `method`, print, check everything.
fn fold_test(h: &Harness, method: OpenMethod) -> Result<(), String> {
    let run = fold_via(h, method, "folded")?;
    println!("Primary_Window tabs after the fold: {:?}", run.primary.locations());
    run.print();
    run.check(true)
}

/// Req 14.1, 14.16, 14.17: `explorer.exe <dir>` becomes a tab of the
/// Primary_Window, shown at most 100 ms, folded within 1.5 s of its first
/// navigation. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn fold_explorer_exe_dir_becomes_a_tab_of_the_primary_window() {
    with_harness("fold_exe", Settings::default(), |h| fold_test(h, OpenMethod::ExplorerExe));
}

/// Req 14.1, 14.6, 14.16, 14.17: `explorer.exe /select,<file>` becomes a tab
/// with exactly that file selected. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn fold_explorer_select_becomes_a_tab_with_the_selection() {
    with_harness("fold_select", Settings::default(), |h| fold_test(h, OpenMethod::ExplorerSelect));
}

/// Req 14.1, 14.6, 14.16, 14.17: `SHOpenFolderAndSelectItems` ("Show in
/// folder") becomes a tab with exactly that file selected. Unverified on a
/// machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn fold_sh_open_folder_and_select_items_becomes_a_tab_with_the_selection() {
    with_harness("fold_shopen", Settings::default(), |h| fold_test(h, OpenMethod::ShOpenFolderAndSelectItems));
}

/// Req 14.1, 14.16, 14.17: a desktop-style `ShellExecute("open")` on a folder
/// becomes a tab of the Primary_Window. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn fold_shell_execute_open_becomes_a_tab_of_the_primary_window() {
    with_harness("fold_shellexec", Settings::default(), |h| fold_test(h, OpenMethod::ShellExecuteOpen));
}

/// Repeats every way of opening a folder `ROUNDS` times in one session and
/// reports how often the candidate outlived the folded tab by the 2 s close
/// retry delay, i.e. how often a second `SC_CLOSE` (Req 14.1's close, whose
/// time Req 14.17, 14.18 and 14.25 count) was possibly needed. It asserts the
/// checks that hold whatever the timing (14.1, 14.6, 14.16) for every run and
/// prints, but does not assert, the completion bound: a run that needed the
/// second `SC_CLOSE` takes at least 2 s and so exceeds 14.17's 1.5 s, which is
/// the finding to report. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn fold_reports_how_often_a_second_sc_close_was_needed() {
    const ROUNDS: usize = 3;
    with_harness("fold_close_report", Settings::default(), |h| {
        let mut runs = Vec::new();
        for round in 0..ROUNDS {
            for method in OpenMethod::ALL {
                let run = fold_via(h, method, &format!("round{round}_{}", runs.len()))?;
                run.print();
                run.check(false)?;
                runs.push(run);
            }
        }
        println!("\nSecond SC_CLOSE per way of opening ({ROUNDS} round(s) each; derived, see the section comment):");
        for method in OpenMethod::ALL {
            let of: Vec<&FoldRun> = runs.iter().filter(|r| r.method == method).collect();
            let possible = of.iter().filter(|r| r.second_close_possible()).count();
            let late = of.iter().filter(|r| r.completion_ms > FOLD_BOUND_MS).count();
            let worst = of.iter().map(|r| r.completion_ms).fold(0.0, f64::max);
            println!(
                "  {:<30} possible {possible}/{}; completed after {FOLD_BOUND_MS} ms: {late}/{}; slowest {worst:.0} ms",
                method.label(),
                of.len(),
                of.len()
            );
        }
        let possible = runs.iter().filter(|r| r.second_close_possible()).count();
        let errors = runs.iter().filter(|r| r.close_error_logged).count();
        println!(
            "  all: second SC_CLOSE possible in {possible}/{} fold(s); `ERROR operation=fold-close` logged in the \
             session: {errors} run(s) saw it. The FOLD entry has no close-attempt field, so these are derived from \
             how long the candidate outlived the folded tab (retry delay {FOLD_CLOSE_RETRY_MS} ms).",
            runs.len()
        );
        Ok(())
    });
}

// ===========================================================================
// Task 27.3: Tab_Switch, a minimized Primary_Window, and three candidates at
// once (Req 14.7, 14.19, 14.21)
// ===========================================================================
//
// * Tab_Switch (14.21, with 14.6): the folder is already open as a tab of the
//   Primary_Window and is not its active tab. Opening it again must activate
//   that tab (no new tab, the tab order unchanged), show exactly the requested
//   selection, close the candidate, bring the Primary_Window to the
//   foreground, and log `FOLD ... outcome=switched matching=primary`. The
//   state is built by folding a second folder first (the folded tab becomes
//   the active one, so the first folder's tab is a Matching_Tab that is not
//   the Active_Tab of the Foreground_Explorer_Window, which is the Primary_Window
//   after a fold). Two switches run in one session, one per direction, one
//   with `explorer.exe <dir>` and one with `explorer.exe /select,<file>`.
// * Minimized Primary_Window (14.7, and 14.21 for a switch): the
//   Primary_Window is minimized with `ShowWindow(SW_MINIMIZE)`; after the fold
//   it must not be iconic, must be back in the show state it had before it was
//   minimized (`GetWindowPlacement`: the same `showCmd` of normal or
//   maximized and the same restored bounds), must have the folder's tab active,
//   and must be the foreground window (`GetForegroundWindow`). Run for a
//   normal and for a maximized Primary_Window, and for a Tab_Switch.
// * Three candidates at once (14.19): three folders are opened with
//   `explorer.exe <dir>` 100 ms apart, so that the later ones open before
//   Tab_Folding of the earlier ones completes. The Primary_Window must end
//   with its own tab followed by the three folded tabs in the order the
//   candidates were created, the last one active, one `folded-primary` FOLD
//   entry per folder, and no window left.
//
// Measurement limits (read before trusting a result):
//
// * Nothing here has been run on a machine yet: every assertion, bound and
//   timing in this section is unverified until a Windows 11 VM run.
// * Keyboard focus (14.7, 14.21) is not checked; only the foreground window is.
// * The order in which the candidates were *created* is taken to be the order
//   in which they were launched. The launches are 100 ms apart and each is a
//   separate `explorer.exe` hand-off, which makes a reversal unlikely but not
//   impossible; a failure on the order alone should be looked at with that in
//   mind. Whether the candidates really overlapped is reported
//   (`max concurrent candidate windows`) but not asserted, as it depends on
//   how fast the fold is.
// * The 14.17 bound is only checked where the candidate's first navigation was
//   seen by a poll (see the 27.2 section), and for three candidates only for
//   the last one, counting both earlier candidates as still being folded
//   (1.5 s + 2 x 1.0 s), which is the loosest bound that 14.17 allows.
// * The tab order is read from the UIA tab strip (the `IShellWindows` order is
//   not known to follow the strip). A strip that exposes no captions makes
//   the order and the active tab unverifiable, which is reported as a failure
//   of these tests rather than skipped.

use win_common::fold_spike::{foreground, window_rect};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, IsIconic, IsZoomed, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWMAXIMIZED, SW_SHOWNORMAL, ShowWindow,
    WINDOWPLACEMENT,
};

/// How long the Primary_Window has to be shown again and be the foreground
/// window after a fold or a switch has been seen to finish.
const REVEAL_GRACE: Duration = Duration::from_millis(2_500);
/// Gap between two launches of the three-candidate test.
const CANDIDATE_STAGGER: Duration = Duration::from_millis(100);
/// Req 14.17 for the last of three overlapping candidates: 1.5 s + 1.0 s for
/// each of the two earlier ones.
const THREE_CANDIDATE_BOUND_MS: f64 = FOLD_BOUND_MS + 2_000.0;

/// The `FOLD` entries of the Activity_Log for `dir`, oldest first.
fn fold_entries_for(h: &Harness, dir: &Path) -> Vec<ParsedLine> {
    let want = normalize(&dir.to_string_lossy());
    h.fold_entries().into_iter().filter(|p| p.get_str("location").is_some_and(|l| normalize(&l) == want)).collect()
}

/// Whether a UIA tab caption belongs to `dir` (the leaf, or the full path).
fn caption_is(caption: &str, dir: &Path) -> bool {
    caption.eq_ignore_ascii_case(&leaf(dir)) || normalize(caption) == normalize(&dir.to_string_lossy())
}

// --- Win32 window state ----------------------------------------------------

/// The show state of the Primary_Window, as Win32 reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShowState {
    /// `WINDOWPLACEMENT.showCmd`: `SW_SHOWNORMAL` (1), `SW_SHOWMAXIMIZED` (3),
    /// or `SW_SHOWMINIMIZED` (2) while minimized.
    pub show_cmd: u32,
    /// `WINDOWPLACEMENT.rcNormalPosition` (the restored bounds) as
    /// (left, top, right, bottom).
    pub restored: (i32, i32, i32, i32),
    pub iconic: bool,
    pub zoomed: bool,
}

/// Reads the show state of `top`.
pub fn show_state(top: isize) -> Result<ShowState, String> {
    let hwnd = hwnd_of(top);
    let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    // SAFETY: valid out pointer with `length` set; a stale handle just fails.
    unsafe { GetWindowPlacement(hwnd, &mut wp) }.map_err(|e| format!("GetWindowPlacement: {e}"))?;
    let r = wp.rcNormalPosition;
    Ok(ShowState {
        show_cmd: wp.showCmd,
        restored: (r.left, r.top, r.right, r.bottom),
        // SAFETY: plain reads.
        iconic: unsafe { IsIconic(hwnd) }.as_bool(),
        zoomed: unsafe { IsZoomed(hwnd) }.as_bool(),
    })
}

/// Maximizes the Primary_Window and waits until it is maximized.
fn maximize_primary(h: &Harness) -> Result<(), String> {
    // SAFETY: `ShowWindow` on a window of the test.
    let _ = unsafe { ShowWindow(hwnd_of(h.primary), SW_MAXIMIZE) };
    wait(Duration::from_secs(3), || show_state(h.primary).ok().filter(|s| s.zoomed && !s.iconic))
        .ok_or("the Primary_Window did not become maximized")?;
    settle(Duration::from_millis(500));
    Ok(())
}

/// Minimizes the Primary_Window with `ShowWindow(SW_MINIMIZE)`, waits until it
/// is iconic and checks that it is not the foreground window then.
fn minimize_primary(h: &Harness) -> Result<(), String> {
    // SAFETY: `ShowWindow` on a window of the test.
    let _ = unsafe { ShowWindow(hwnd_of(h.primary), SW_MINIMIZE) };
    wait(Duration::from_secs(3), || show_state(h.primary).ok().filter(|s| s.iconic))
        .ok_or("the Primary_Window did not become minimized")?;
    settle(Duration::from_millis(500));
    if foreground() == h.primary {
        return Err("the minimized Primary_Window is still the foreground window".into());
    }
    Ok(())
}

/// Waits until the Primary_Window is not iconic and is the foreground window.
/// Returns when that was first seen.
fn wait_revealed(h: &Harness, timeout: Duration) -> Option<Instant> {
    wait(timeout, || {
        let iconic = show_state(h.primary).map(|s| s.iconic).unwrap_or(true);
        (!iconic && foreground() == h.primary).then(Instant::now)
    })
}

/// Req 14.7/14.21: the Primary_Window is back in `before` (normal at its
/// restored bounds, or maximized), not minimized, and the foreground window.
fn check_revealed(h: &Harness, before: ShowState, what: &str) -> Result<(), String> {
    if wait_revealed(h, REVEAL_GRACE).is_none() {
        return Err(format!(
            "{what}: within {REVEAL_GRACE:?} the Primary_Window was not shown and in the foreground (iconic={:?}, \
             foreground is the Primary_Window: {}) (14.7)",
            show_state(h.primary).map(|s| s.iconic),
            foreground() == h.primary
        ));
    }
    settle(Duration::from_millis(300)); // a late second change would show up here
    let after = show_state(h.primary)?;
    println!("  {what}: Primary_Window show state before minimizing {before:?}, now {after:?}");
    if after.iconic {
        return Err(format!("{what}: the Primary_Window is minimized again (14.7)"));
    }
    if after.show_cmd != before.show_cmd || after.zoomed != before.zoomed {
        return Err(format!(
            "{what}: the Primary_Window is not back in its pre-minimize state: showCmd {} (zoomed {}), want {} \
             (zoomed {}) (14.7)",
            after.show_cmd, after.zoomed, before.show_cmd, before.zoomed
        ));
    }
    if after.restored != before.restored {
        return Err(format!(
            "{what}: the restored bounds changed from {:?} to {:?} (14.7)",
            before.restored, after.restored
        ));
    }
    if before.show_cmd == SW_SHOWNORMAL.0 as u32 {
        // A normal window sits at its restored bounds.
        let r = window_rect(hwnd_of(h.primary));
        let rect = (r.left, r.top, r.right, r.bottom);
        if rect != before.restored {
            return Err(format!("{what}: the window is at {rect:?}, not at its restored bounds {:?} (14.7)", before.restored));
        }
    }
    if foreground() != h.primary {
        return Err(format!("{what}: the Primary_Window is not the foreground window (14.7)"));
    }
    Ok(())
}

// --- Tab_Switch ------------------------------------------------------------

/// What `switch_via` measured for one Tab_Switch.
#[derive(Debug)]
pub struct SwitchRun {
    pub method: OpenMethod,
    pub dir: PathBuf,
    pub file: Option<PathBuf>,
    /// The Primary_Window's tabs (ShellWindows order) and strip before.
    pub locations_before: Vec<String>,
    pub strip_before: Vec<String>,
    /// The Primary_Window after the switch.
    pub primary: WindowView,
    /// A poll saw the candidate window showing the folder.
    pub first_nav_seen: bool,
    /// From the candidate's first navigation to the switch being complete
    /// (tab active, selection shown, candidate gone, entry logged).
    pub completion_ms: Option<f64>,
    pub outcome: Option<String>,
    pub matching: Option<String>,
    pub max_shown_ms: f64,
    pub track_lines: Vec<String>,
    pub leftover_windows: usize,
}

impl SwitchRun {
    pub fn print(&self) {
        println!(
            "  {:<30} switch to {}: completed {} after the first navigation; max shown {:>4.0} ms; outcome {:?} \
             matching {:?}",
            self.method.label(),
            leaf(&self.dir),
            self.completion_ms.map_or("(first navigation not seen)".to_string(), |m| format!("{m:>5.0} ms")),
            self.max_shown_ms,
            self.outcome,
            self.matching
        );
        for line in &self.track_lines {
            println!("      {line}");
        }
    }

    /// Req 14.21, 14.6, 14.16 (and 14.17 when the first navigation was seen).
    pub fn check(&self) -> Result<(), String> {
        let label = self.method.label();
        match (self.outcome.as_deref(), self.matching.as_deref()) {
            (Some("switched"), Some("primary")) => {}
            other => {
                return Err(format!("{label}: the FOLD entry is {other:?}, want switched, matching primary (14.21)"));
            }
        }
        if self.primary.locations() != self.locations_before {
            return Err(format!(
                "{label}: the Primary_Window's tabs changed from {:?} to {:?}; a Tab_Switch leaves the tabs and \
                 their order unchanged (14.21)",
                self.locations_before,
                self.primary.locations()
            ));
        }
        if self.primary.uia_names != self.strip_before {
            return Err(format!(
                "{label}: the tab strip changed from {:?} to {:?} (14.21)",
                self.strip_before, self.primary.uia_names
            ));
        }
        if !self.primary.active_is(&self.dir) {
            return Err(format!(
                "{label}: the active tab is {:?}, want {} (14.21)",
                self.primary.active_caption(),
                leaf(&self.dir)
            ));
        }
        if let (Some(file), Some(tab)) = (&self.file, self.primary.tab_of(&self.dir)) {
            let want = vec![normalize(&file.to_string_lossy())];
            if self.primary.tabs[tab].selection != want {
                return Err(format!(
                    "{label}: selection is {:?}, want exactly {want:?} (14.6)",
                    self.primary.tabs[tab].selection
                ));
            }
        }
        if self.leftover_windows != 0 {
            return Err(format!("{label}: {} window(s) other than the Primary_Window are left open (14.21)", self.leftover_windows));
        }
        if self.max_shown_ms > MAX_CANDIDATE_VISIBLE_MS {
            return Err(format!(
                "{label}: a window was shown for {:.0} ms in total, at most {MAX_CANDIDATE_VISIBLE_MS} ms allowed \
                 (14.16): {:?}",
                self.max_shown_ms, self.track_lines
            ));
        }
        if let Some(ms) = self.completion_ms.filter(|&ms| ms > FOLD_BOUND_MS) {
            return Err(format!(
                "{label}: the Tab_Switch completed {ms:.0} ms after the first navigation, at most {FOLD_BOUND_MS} ms \
                 allowed (14.17)"
            ));
        }
        Ok(())
    }
}

/// Opens `dir` again (a folder that is a non-active tab of the Primary_Window)
/// with `method`, waits for the Tab_Switch, and measures it. With `file`
/// (inside `dir`; `ExplorerSelect` and `ShOpenFolderAndSelectItems` select it)
/// the selection is part of the completion condition. The Primary_Window's
/// reveal (not minimized, foreground) is *not* part of this run; callers check
/// it with [`check_revealed`] or [`wait_revealed`].
pub fn switch_via(h: &Harness, method: OpenMethod, dir: &Path, file: Option<&Path>) -> Result<SwitchRun, String> {
    let want = normalize(&dir.to_string_lossy());
    let want_selection = file.filter(|_| method.selects()).map(|f| vec![normalize(&f.to_string_lossy())]);
    let before = h.view(h.primary);
    if before.uia_names.is_empty() {
        return Err("the Primary_Window's tab strip exposes no captions; the active tab cannot be verified".into());
    }
    if before.active_is(dir) {
        return Err(format!("{} is already the active tab; it would not be a Tab_Switch", dir.display()));
    }
    if before.tab_of(dir).is_none() {
        return Err(format!("{} is not a tab of the Primary_Window: {:?}", dir.display(), before.locations()));
    }
    let entries_before = fold_entries_for(h, dir).len();

    let sampler = h.sampler();
    let mut launch = h.open(method, dir, file)?;
    let mut first_nav = None;
    let mut done = None;
    let mut last_view = None;
    let finished = wait(OPEN_TIMEOUT.max(FOLD_GIVE_UP), || {
        let now = Instant::now();
        if first_nav.is_none() {
            let shows = windows_with_locations().iter().any(|(t, l)| *t != h.primary && l.contains(&want));
            if shows {
                first_nav = Some(now);
            }
        }
        if fold_entries_for(h, dir).len() > entries_before && h.other_windows().is_empty() {
            let view = h.view(h.primary);
            let selection_ok = match (&want_selection, view.tab_of(dir)) {
                (Some(sel), Some(i)) => view.tabs[i].selection == *sel,
                (Some(_), None) => false,
                (None, _) => true,
            };
            let ok = view.active_caption().is_some() && view.active_is(dir) && selection_ok;
            last_view = Some(view);
            if ok {
                done = Some(Instant::now());
                return Some(());
            }
        }
        None
    });
    settle(Duration::from_millis(300)); // a late second window would show up here
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("{}: the call failed: {e}", method.label()));
    }
    if finished.is_none() {
        return Err(format!(
            "{}: opening {} did not switch to its tab within {FOLD_GIVE_UP:?}; last view {last_view:?}",
            method.label(),
            dir.display()
        ));
    }
    let done = done.ok_or("no completion instant")?;

    let entry = wait(Duration::from_secs(3), || fold_entries_for(h, dir).into_iter().nth(entries_before));
    Ok(SwitchRun {
        method,
        dir: dir.to_path_buf(),
        file: file.map(Path::to_path_buf),
        locations_before: before.locations(),
        strip_before: before.uia_names,
        primary: last_view.ok_or("no view of the Primary_Window")?,
        first_nav_seen: first_nav.is_some(),
        completion_ms: first_nav.map(|a| ms_since(a, done)),
        outcome: entry.as_ref().and_then(|p| p.get_str("outcome")),
        matching: entry.as_ref().and_then(|p| p.get_str("matching")),
        max_shown_ms: max_shown_ms(&tracks, end),
        track_lines: describe_tracks(&tracks, end),
        leftover_windows: h.other_windows().len(),
    })
}

/// Folds a second folder into the Primary_Window so that the first folder's
/// tab is a Matching_Tab that is not the active tab. Returns the second
/// folder.
fn fold_second_tab(h: &Harness) -> Result<PathBuf, String> {
    let run = fold_via(h, OpenMethod::ExplorerExe, "second")?;
    run.print();
    run.check(false)?;
    let view = h.view(h.primary);
    if view.tabs.len() != 2 || !view.active_is(&run.dir) {
        return Err(format!("setup: want two tabs with the folded one active, got {view:?}"));
    }
    Ok(run.dir)
}

/// Req 14.21 and 14.6: a folder that is already a non-active tab of the
/// Primary_Window is switched to, not opened again; once with a plain
/// `explorer.exe <dir>` (back to the first tab), once with
/// `explorer.exe /select,<file>` (back to the second tab with its file
/// selected). Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn tab_switch_activates_the_existing_tab_in_the_primary_window() {
    with_harness("switch_primary", Settings::default(), |h| {
        let first = h.primary_dir.clone();
        let second = fold_second_tab(h)?;
        let selected = second.join("target.txt"); // written by `fold_via`

        let run = switch_via(h, OpenMethod::ExplorerExe, &first, None)?;
        run.print();
        run.check()?;
        check_revealed_foreground(h, "switch to the first tab")?;

        let run = switch_via(h, OpenMethod::ExplorerSelect, &second, Some(&selected))?;
        run.print();
        run.check()?;
        check_revealed_foreground(h, "switch to the second tab")
    });
}

/// 14.21 brings the Fold_Target to the foreground: not iconic and foreground.
fn check_revealed_foreground(h: &Harness, what: &str) -> Result<(), String> {
    wait_revealed(h, REVEAL_GRACE)
        .map(|_| ())
        .ok_or_else(|| format!("{what}: the Primary_Window is not shown and in the foreground within {REVEAL_GRACE:?} (14.21)"))
}

// --- Minimized Primary_Window ----------------------------------------------

/// Req 14.7: a folder folded while the Primary_Window is minimized (`maximized`
/// selects whether it was maximized before it was minimized) leaves the
/// Primary_Window in its pre-minimize state, with the folded tab active and in
/// the foreground.
fn minimized_fold(h: &Harness, maximized: bool) -> Result<(), String> {
    if maximized {
        maximize_primary(h)?;
    }
    let before = show_state(h.primary)?;
    let want_cmd = if maximized { SW_SHOWMAXIMIZED.0 } else { SW_SHOWNORMAL.0 } as u32;
    if before.show_cmd != want_cmd || before.zoomed != maximized {
        return Err(format!("setup: the Primary_Window is in state {before:?}, want showCmd {want_cmd}"));
    }
    minimize_primary(h)?;

    let run = fold_via(h, OpenMethod::ExplorerExe, "folded_while_minimized")?;
    run.print();
    run.check(true)?;
    check_revealed(h, before, "fold into a minimized Primary_Window")?;
    let view = h.view(h.primary);
    if view.active_caption().is_none() {
        return Err("the tab strip exposes no captions; the active tab cannot be verified (14.7)".into());
    }
    if !view.active_is(&run.dir) {
        return Err(format!("the folded tab is not active: strip {:?}, active {:?} (14.7)", view.uia_names, view.active_caption()));
    }
    Ok(())
}

/// Req 14.7: normal Primary_Window, minimized, then a folder is opened.
/// Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn minimized_primary_returns_to_its_normal_state_with_the_folded_tab_active() {
    with_harness("minimized_normal", Settings::default(), |h| minimized_fold(h, false));
}

/// Req 14.7: maximized Primary_Window, minimized, then a folder is opened;
/// it must come back maximized. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn minimized_maximized_primary_returns_maximized_with_the_folded_tab_active() {
    with_harness("minimized_maximized", Settings::default(), |h| minimized_fold(h, true));
}

/// Req 14.21: a Tab_Switch on a minimized Primary_Window shows it in its
/// pre-minimize state with the matching tab active and in the foreground.
/// Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn tab_switch_on_a_minimized_primary_returns_it_to_its_pre_minimize_state() {
    with_harness("switch_minimized", Settings::default(), |h| {
        let first = h.primary_dir.clone();
        fold_second_tab(h)?;
        wait_revealed(h, REVEAL_GRACE); // the fold's own reveal must be over before the state is read
        let before = show_state(h.primary)?;
        minimize_primary(h)?;

        let run = switch_via(h, OpenMethod::ExplorerExe, &first, None)?;
        run.print();
        run.check()?;
        check_revealed(h, before, "switch on a minimized Primary_Window")
    });
}

// --- Three candidates at once ----------------------------------------------

/// What the polling of the three-candidate test has seen.
#[derive(Default)]
struct Overlap {
    first_nav: [Option<Instant>; 3],
    tab_seen: [Option<Instant>; 3],
    max_candidate_windows: usize,
}

impl Overlap {
    fn observe(&mut self, h: &Harness, dirs: &[PathBuf]) {
        let now = Instant::now();
        let wins = windows_with_locations();
        let others = wins.iter().filter(|(t, _)| *t != h.primary).count();
        self.max_candidate_windows = self.max_candidate_windows.max(others);
        for (i, dir) in dirs.iter().enumerate() {
            let want = normalize(&dir.to_string_lossy());
            if self.first_nav[i].is_none() && wins.iter().any(|(t, l)| *t != h.primary && l.contains(&want)) {
                self.first_nav[i] = Some(now);
            }
            if self.tab_seen[i].is_none() && wins.iter().any(|(t, l)| *t == h.primary && l.contains(&want)) {
                self.tab_seen[i] = Some(now);
            }
        }
    }
}

/// Req 14.19: three folders opened 100 ms apart become three tabs of the
/// Primary_Window in creation order, the last one active. Unverified on a
/// machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn three_candidates_at_once_become_tabs_in_creation_order_with_the_last_active() {
    with_harness("three_candidates", Settings::default(), |h| {
        let dirs: Vec<PathBuf> = (1..=3).map(|i| h.dir(&format!("candidate{i}"))).collect();
        let before = h.view(h.primary);
        if before.uia_names.is_empty() {
            return Err("the Primary_Window's tab strip exposes no captions; the tab order cannot be verified".into());
        }
        let tabs_before = before.tabs.len();

        let sampler = h.sampler();
        let mut seen = Overlap::default();
        let mut launches = Vec::new();
        for (i, dir) in dirs.iter().enumerate() {
            if i > 0 {
                // Keep observing while the previous candidates are folded.
                let _ = wait(CANDIDATE_STAGGER, || {
                    seen.observe(h, &dirs);
                    None::<()>
                });
            }
            launches.push(h.open(OpenMethod::ExplorerExe, dir, None)?);
        }

        let mut last_view = None;
        let mut done = None;
        let finished = wait(Duration::from_secs(20), || {
            seen.observe(h, &dirs);
            if seen.tab_seen.iter().all(Option::is_some) && h.other_windows().is_empty() {
                let view = h.view(h.primary);
                let ok = view.tabs.len() == tabs_before + 3 && view.active_caption().is_some() && view.active_is(&dirs[2]);
                last_view = Some(view);
                if ok {
                    done = Some(Instant::now());
                    return Some(());
                }
            }
            None
        });
        settle(Duration::from_millis(300)); // a late second window or tab change would show up here
        let end = Instant::now();
        let tracks = sampler.stop();
        h.adopt_all();
        for mut launch in launches {
            let _ = launch.call_result();
        }
        let view = h.view(h.primary);
        println!("Primary_Window after the three folds: tabs {:?}, strip {:?}", view.locations(), view.uia_names);
        println!(
            "  max concurrent candidate windows seen: {} (not asserted; 1 means the folds did not overlap)",
            seen.max_candidate_windows
        );
        for line in describe_tracks(&tracks, end) {
            println!("      {line}");
        }
        if finished.is_none() {
            return Err(format!(
                "the three folders were not all folded within 20 s with the last one active; last view {last_view:?}"
            ));
        }

        // 14.19: left-to-right order of creation, the last one active.
        let want_strip: Vec<&PathBuf> = std::iter::once(&h.primary_dir).chain(dirs.iter()).collect();
        let strip_ok = view.uia_names.len() == want_strip.len()
            && view.uia_names.iter().zip(&want_strip).all(|(c, d)| caption_is(c, d));
        if !strip_ok {
            return Err(format!(
                "tab strip is {:?}, want the Primary_Window's own tab followed by {:?} in creation order (14.19)",
                view.uia_names,
                dirs.iter().map(|d| leaf(d)).collect::<Vec<_>>()
            ));
        }
        let mut have = view.locations();
        have.sort();
        let mut want_locs: Vec<String> =
            want_strip.iter().map(|d| normalize(&d.to_string_lossy())).collect();
        want_locs.sort();
        if have != want_locs {
            return Err(format!("the Primary_Window's tabs are {have:?}, want {want_locs:?} (14.19)"));
        }
        if !view.active_is(&dirs[2]) {
            return Err(format!("the active tab is {:?}, want the last candidate {} (14.19)", view.active_caption(), leaf(&dirs[2])));
        }
        if !h.other_windows().is_empty() {
            return Err(format!("{} window(s) other than the Primary_Window are left open (14.19)", h.other_windows().len()));
        }

        // Each candidate handled exactly once, folded into the Primary_Window.
        let logged = wait(Duration::from_secs(3), || {
            dirs.iter().all(|d| !fold_entries_for(h, d).is_empty()).then_some(())
        });
        for dir in &dirs {
            let entries = fold_entries_for(h, dir);
            let outcomes: Vec<Option<String>> = entries.iter().map(|p| p.get_str("outcome")).collect();
            if entries.len() != 1 || outcomes[0].as_deref() != Some("folded-primary") {
                return Err(format!(
                    "{}: FOLD entries {outcomes:?} (logged in time: {}), want exactly one folded-primary (14.19, 14.30)",
                    leaf(dir),
                    logged.is_some()
                ));
            }
        }

        // 14.16: no candidate shown for more than 100 ms.
        let shown = max_shown_ms(&tracks, end);
        if shown > MAX_CANDIDATE_VISIBLE_MS {
            return Err(format!("a candidate was shown for {shown:.0} ms in total, at most {MAX_CANDIDATE_VISIBLE_MS} ms allowed (14.16)"));
        }

        // 14.17 for the last candidate, counting both earlier ones as folding.
        match (seen.first_nav[2], done) {
            (Some(anchor), Some(done)) => {
                let ms = ms_since(anchor, done);
                println!("  the last candidate completed {ms:.0} ms after its first navigation (bound {THREE_CANDIDATE_BOUND_MS} ms)");
                if ms > THREE_CANDIDATE_BOUND_MS {
                    return Err(format!(
                        "the last candidate completed {ms:.0} ms after its first navigation, at most \
                         {THREE_CANDIDATE_BOUND_MS} ms allowed (14.17)"
                    ));
                }
            }
            _ => println!("  the last candidate's first navigation was not seen; the 14.17 bound is not checked"),
        }
        Ok(())
    });
}

// ===========================================================================
// Task 27.4: the foreground exception (Req 14.21, 14.22)
// ===========================================================================
//
// Req 14.22: a Folding_Candidate whose Matching_Tab is the Active_Tab of the
// Foreground_Explorer_Window is folded into a *new* tab of that window, so
// that Ctrl+N and "New window" in the foreground window act as "new tab on
// this folder". A candidate created while another application is in the
// foreground has no Foreground_Explorer_Window, so Req 14.21 applies: a
// Tab_Switch to the existing tab.
//
// DO NOT TOUCH THE KEYBOARD OR MOUSE WHILE THESE TESTS RUN. Ctrl+N is injected
// with `SendInput` into whatever window is in the foreground, and the
// foreground window is switched between the Primary_Window and a window of the
// test; a keystroke or click of yours would go to the wrong window or change
// which window is in the foreground. Ctrl is always released again, also when
// a test panics.
//
// How each trigger is made (the setup is the same for the three Primary_Window
// triggers: the Primary_Window has one tab, which is its Active_Tab and shows
// the folder `D`; the Primary_Window is made the foreground window with the
// product's own zero-effect `SendInput` + `SetForegroundWindow` technique
// ([`make_foreground`], which checks `GetForegroundWindow` and fails the test
// otherwise)):
//
// * **Ctrl+N** ([`Trigger::CtrlN`]): Ctrl down, N down, N up, Ctrl up with
//   `SendInput` (the keys the spike F3 case used), into the foreground
//   Primary_Window. Explorer opens a new window on `D`.
// * **"New window"** ([`Trigger::OpenNewWindowVerb`]): File Explorer on
//   Windows 11 (22H2 and later) has no File menu or ribbon, and the commands
//   that remain ("Open in new window" in a folder's context menu) are not on
//   an addressable, stable UI Automation surface that this harness could
//   invoke reliably, so the test does not drive a menu. It issues the shell
//   command that "Open in new window" / the former File > "Open new window"
//   run, the `opennewwindow` verb of the folder (`ShellExecuteExW` with that
//   verb on `D`, the same call the harness uses to open the Primary_Window),
//   *while the Primary_Window is the foreground window*. To the product this is
//   a new window on `D` whose Foreground_Explorer_Window is the Primary_Window,
//   exactly as for the menu command. What it does not exercise is Explorer's
//   own menu; that, and whether the menu command differs from the verb in how
//   the new window is created, is not covered here. Ctrl+N covers the other
//   keyboard-driven way.
// * **Another application** ([`Trigger::Application`]): a plain top-level
//   window owned by the test ([`AppWindow`], class `STATIC`, not an Explorer
//   window) is made the foreground window, then `D` is opened with
//   `explorer.exe <D>` and with `SHOpenFolderAndSelectItems` ("Show in
//   folder"). The foreground window is not an Explorer_Window at that moment,
//   so there is no Foreground_Explorer_Window.
//
// Each run ([`foreground_case`]) records the Primary_Window's tabs before,
// triggers, polls until a new `FOLD` entry for `D` exists, no other Explorer
// window is open, and the Primary_Window has the expected state, then keeps
// watching for 1.5 s (a late second tab or window would show up there, which
// is what makes "exactly one" a check and not a hope) and verifies:
//
// * folding (Ctrl+N, "New window"): exactly one new tab, rightmost in the tab
//   strip and active, showing `D`; the earlier tabs unchanged; the log has one
//   new entry `FOLD ... outcome=folded-foreground into=foreground`; no other
//   window is left; any window other than the Primary_Window was shown for at
//   most 100 ms in total (Req 14.16); the Primary_Window ends in the
//   foreground;
// * switching (another application): no new tab, the same tabs in the same
//   order, the tab for `D` is the active one (and, for the "Show in folder"
//   open, exactly the requested file is selected in it); one new entry
//   `FOLD ... outcome=switched matching=primary`; no other window is left; at
//   most 100 ms shown; the Primary_Window ends in the foreground (Req 14.21).
//
// Measurement limits (read before trusting a result):
//
// * Nothing here has been run on a machine yet. Every assertion, bound and
//   timing in this section is unverified until a Windows 11 VM run, and the
//   key injection in particular has never been seen to reach Explorer from
//   this harness (the F3 spike shows that Ctrl+N works from `SendInput` after
//   the same foreground technique).
// * The completion time printed is from the trigger (the keys, the call) to
//   the instant a poll saw the finished state; it is not anchored on the
//   candidate's first navigation as in the 27.2 section, and is not asserted.
// * Whether the candidate window was seen at all (`candidate seen`) depends on
//   the polling and is only printed.
// * Keyboard focus is not checked, only the foreground window.
// * "Exactly one" is observed over 1.5 s after the finished state, not for
//   ever.
// * The tab strip must expose its captions (the new tab and the active tab are
//   read from it); a strip without captions fails the test rather than
//   skipping it.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    VK_CONTROL, VK_N,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP, WS_VISIBLE};
use windows::core::w;
use win_common::fold_spike::bring_to_foreground;

/// How long the state after a trigger is watched once it looks finished, to
/// see a second tab or window that would appear late.
const QUIET_WATCH: Duration = Duration::from_millis(1_500);

/// How the folder is opened in the foreground-exception tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Ctrl+N with `SendInput`, with the Primary_Window in the foreground.
    CtrlN,
    /// The `opennewwindow` verb of the folder ("Open in new window", the
    /// command behind "New window"), with the Primary_Window in the
    /// foreground. See the section comment for why this and not a menu.
    OpenNewWindowVerb,
    /// The folder opened by another application (the test's own window is in
    /// the foreground).
    Application(OpenMethod),
}

impl Trigger {
    pub fn label(self) -> String {
        match self {
            Trigger::CtrlN => "Ctrl+N in the foreground Primary_Window".into(),
            Trigger::OpenNewWindowVerb => "\"New window\" (opennewwindow verb) in the foreground Primary_Window".into(),
            Trigger::Application(m) => format!("{} from another application", m.label()),
        }
    }
}

/// What the Primary_Window must look like afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Req 14.22: exactly one new, rightmost, active tab on the folder.
    NewTabInForeground,
    /// Req 14.21: a Tab_Switch; no new tab.
    SwitchToExisting,
}

fn send_key(vk: VIRTUAL_KEY, up: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: one fully initialized INPUT.
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}

/// Ctrl held with `SendInput`; released on drop, so it is never left down.
struct CtrlDown;

impl CtrlDown {
    fn press() -> Self {
        send_key(VK_CONTROL, false);
        Self
    }
}

impl Drop for CtrlDown {
    fn drop(&mut self) {
        send_key(VK_CONTROL, true);
    }
}

/// Ctrl+N into the foreground window.
fn send_ctrl_n() {
    let _ctrl = CtrlDown::press();
    std::thread::sleep(Duration::from_millis(30));
    send_key(VK_N, false);
    std::thread::sleep(Duration::from_millis(30));
    send_key(VK_N, true);
    std::thread::sleep(Duration::from_millis(30));
}

/// A plain top-level window of the test that is not an Explorer window: the
/// "other application" in the foreground. Created and destroyed on the test
/// thread, which pumps messages in [`wait`] and [`settle`].
struct AppWindow(HWND);

impl AppWindow {
    fn open() -> Result<Self, String> {
        // SAFETY: valid predefined class name; no parent, menu, or params.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("ESR tab folding test: another application"),
                WS_POPUP | WS_VISIBLE,
                20,
                20,
                320,
                120,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW(STATIC): {e}"))?;
        Ok(Self(hwnd))
    }

    fn raw(&self) -> isize {
        self.0.0 as isize
    }
}

impl Drop for AppWindow {
    fn drop(&mut self) {
        // SAFETY: created by this thread, destroyed once.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// Makes `target` the foreground window (zero-effect `SendInput` +
/// `SetForegroundWindow`, retried) and lets focus settle. `Err` when it is not
/// the foreground window afterwards: the key injection would go elsewhere.
fn make_foreground(target: isize, what: &str) -> Result<(), String> {
    for _ in 0..5 {
        bring_to_foreground(target);
        settle(Duration::from_millis(200));
        if foreground() == target {
            settle(Duration::from_millis(300)); // keyboard focus lands in the window
            return Ok(());
        }
    }
    Err(format!(
        "could not make {what} the foreground window (foreground is {:?}); was the keyboard or mouse used?",
        hwnd_of(foreground())
    ))
}

/// The fields of one `FOLD` entry that these tests read.
#[derive(Debug)]
pub struct FoldFields {
    pub outcome: Option<String>,
    pub into: Option<String>,
    pub matching: Option<String>,
}

/// What `foreground_case` observed for one trigger.
#[derive(Debug)]
pub struct ForegroundRun {
    pub trigger: Trigger,
    pub expect: Expect,
    pub dir: PathBuf,
    pub file: Option<PathBuf>,
    /// The Primary_Window before the trigger and after the quiet watch.
    pub before: WindowView,
    pub after: WindowView,
    /// The `FOLD` entries for the folder that the trigger added.
    pub entries: Vec<FoldFields>,
    /// Windows other than the Primary_Window left open after the watch.
    pub leftover_windows: usize,
    /// A poll saw a window other than the Primary_Window (not asserted).
    pub candidate_seen: bool,
    /// From the trigger to the poll that saw the finished state (not asserted).
    pub completion_ms: f64,
    pub max_shown_ms: f64,
    pub track_lines: Vec<String>,
}

fn tabs_on(view: &WindowView, dir: &Path) -> usize {
    let want = normalize(&dir.to_string_lossy());
    view.tabs.iter().filter(|t| t.location == want).count()
}

impl ForegroundRun {
    pub fn print(&self) {
        println!(
            "  {}: tabs {} -> {}; strip {:?} -> {:?}; active {:?}; FOLD {:?}; candidate seen: {}; finished {:.0} ms \
             after the trigger; max shown {:.0} ms",
            self.trigger.label(),
            self.before.tabs.len(),
            self.after.tabs.len(),
            self.before.uia_names,
            self.after.uia_names,
            self.after.active_caption(),
            self.entries,
            self.candidate_seen,
            self.completion_ms,
            self.max_shown_ms
        );
        for line in &self.track_lines {
            println!("      {line}");
        }
    }

    /// The Req 14.21/14.22 checks (and 14.6 for a selection, 14.16).
    pub fn check(&self) -> Result<(), String> {
        let label = self.trigger.label();
        let req = match self.expect {
            Expect::NewTabInForeground => "14.22",
            Expect::SwitchToExisting => "14.21",
        };
        if self.entries.len() != 1 {
            return Err(format!("{label}: {} FOLD entries for the folder were logged, want exactly one ({req}): {:?}", self.entries.len(), self.entries));
        }
        let entry = &self.entries[0];
        if self.leftover_windows != 0 {
            return Err(format!("{label}: {} window(s) other than the Primary_Window are left open ({req})", self.leftover_windows));
        }
        if self.max_shown_ms > MAX_CANDIDATE_VISIBLE_MS {
            return Err(format!(
                "{label}: a window was shown for {:.0} ms in total, at most {MAX_CANDIDATE_VISIBLE_MS} ms allowed \
                 (14.16): {:?}",
                self.max_shown_ms, self.track_lines
            ));
        }
        let (b, a) = (&self.before, &self.after);
        match self.expect {
            Expect::NewTabInForeground => {
                if entry.outcome.as_deref() != Some("folded-foreground") || entry.into.as_deref() != Some("foreground") {
                    return Err(format!(
                        "{label}: the FOLD entry is {entry:?}, want outcome folded-foreground into foreground (14.22)"
                    ));
                }
                if a.tabs.len() != b.tabs.len() + 1 {
                    return Err(format!(
                        "{label}: the Primary_Window has {} tab(s) ({:?}), want exactly one new tab, {} (14.22)",
                        a.tabs.len(),
                        a.locations(),
                        b.tabs.len() + 1
                    ));
                }
                if tabs_on(a, &self.dir) != tabs_on(b, &self.dir) + 1 {
                    return Err(format!(
                        "{label}: {} tab(s) show the folder, want {} (14.22): {:?}",
                        tabs_on(a, &self.dir),
                        tabs_on(b, &self.dir) + 1,
                        a.locations()
                    ));
                }
                if a.uia_names.len() != b.uia_names.len() + 1 || a.uia_names[..b.uia_names.len()] != b.uia_names[..] {
                    return Err(format!(
                        "{label}: the tab strip is {:?}, want the earlier tabs {:?} followed by one new tab (14.22)",
                        a.uia_names, b.uia_names
                    ));
                }
                let last = a.uia_names.len() - 1;
                if !caption_is(&a.uia_names[last], &self.dir) {
                    return Err(format!("{label}: the rightmost tab is {:?}, want the folder {} (14.22)", a.uia_names[last], leaf(&self.dir)));
                }
                if a.uia_selected != Some(last) {
                    return Err(format!("{label}: the active tab is {:?}, want the new rightmost tab {last} (14.22)", a.uia_selected));
                }
            }
            Expect::SwitchToExisting => {
                if entry.outcome.as_deref() != Some("switched") || entry.matching.as_deref() != Some("primary") {
                    return Err(format!("{label}: the FOLD entry is {entry:?}, want outcome switched, matching primary (14.21)"));
                }
                if a.tabs.len() != b.tabs.len() || a.locations().len() != b.locations().len() {
                    return Err(format!(
                        "{label}: the Primary_Window has {} tab(s) ({:?}), want {} unchanged; a Tab_Switch adds no tab (14.21)",
                        a.tabs.len(),
                        a.locations(),
                        b.tabs.len()
                    ));
                }
                if a.uia_names != b.uia_names {
                    return Err(format!("{label}: the tab strip changed from {:?} to {:?} (14.21)", b.uia_names, a.uia_names));
                }
                if a.uia_selected != b.uia_selected || !a.active_is(&self.dir) {
                    return Err(format!(
                        "{label}: the active tab is {:?} ({:?}), want the existing tab for the folder, {:?} (14.21)",
                        a.uia_selected,
                        a.active_caption(),
                        b.uia_selected
                    ));
                }
                if let (Some(file), Trigger::Application(m)) = (&self.file, self.trigger) {
                    let want = vec![normalize(&file.to_string_lossy())];
                    if m.selects() && !a.tabs.iter().any(|t| t.location == normalize(&self.dir.to_string_lossy()) && t.selection == want) {
                        return Err(format!("{label}: no tab for the folder has exactly {want:?} selected (14.6): {:?}", a.tabs));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Triggers `trigger` for `dir` while `expected_foreground` is the foreground
/// window, waits for the Primary_Window to reach the state `expect` describes,
/// watches it for [`QUIET_WATCH`], and returns what it saw (see the section
/// comment). `dir` must be the Active_Tab's folder of the Primary_Window.
/// `file` (inside `dir`) is selected by `ShOpenFolderAndSelectItems`.
pub fn foreground_case(
    h: &Harness,
    trigger: Trigger,
    expect: Expect,
    dir: &Path,
    file: Option<&Path>,
    expected_foreground: isize,
) -> Result<ForegroundRun, String> {
    let want = normalize(&dir.to_string_lossy());
    let before = h.view(h.primary);
    if before.uia_names.is_empty() || before.uia_selected.is_none() {
        return Err("the Primary_Window's tab strip exposes no captions; the new and active tab cannot be verified".into());
    }
    if !before.active_is(dir) {
        return Err(format!(
            "setup: the Active_Tab of the Primary_Window is {:?}, want the folder {}",
            before.active_caption(),
            leaf(dir)
        ));
    }
    if !h.other_windows().is_empty() {
        return Err("setup: a window other than the Primary_Window is open".into());
    }
    if foreground() != expected_foreground {
        return Err(format!(
            "setup: the foreground window is {:?}, want {:?}; was the keyboard or mouse used?",
            hwnd_of(foreground()),
            hwnd_of(expected_foreground)
        ));
    }
    let want_selection = match trigger {
        Trigger::Application(m) if m.selects() => file.map(|f| vec![normalize(&f.to_string_lossy())]),
        _ => None,
    };
    let entries_before = fold_entries_for(h, dir).len();

    let sampler = h.sampler();
    let t0 = Instant::now();
    let call = match trigger {
        Trigger::CtrlN => {
            send_ctrl_n();
            None
        }
        Trigger::OpenNewWindowVerb => Some(shell_execute(dir, Some("opennewwindow"))),
        Trigger::Application(method) => {
            let mut launch = h.open(method, dir, file)?;
            launch.call.take()
        }
    };

    let mut candidate_seen = false;
    let mut done = None;
    let mut last_view = None;
    let finished = wait(OPEN_TIMEOUT.max(FOLD_GIVE_UP), || {
        if !candidate_seen && !h.other_windows().is_empty() {
            candidate_seen = true;
        }
        if fold_entries_for(h, dir).len() > entries_before && h.other_windows().is_empty() {
            let view = h.view(h.primary);
            let ok = match expect {
                Expect::NewTabInForeground => {
                    view.tabs.len() == before.tabs.len() + 1
                        && view.uia_names.len() == before.uia_names.len() + 1
                        && view.uia_selected == Some(view.uia_names.len() - 1)
                }
                Expect::SwitchToExisting => {
                    view.active_caption().is_some()
                        && view.active_is(dir)
                        && want_selection.as_ref().is_none_or(|sel| {
                            view.tabs.iter().any(|t| t.location == want && t.selection == *sel)
                        })
                }
            };
            last_view = Some(view);
            if ok {
                done = Some(Instant::now());
                return Some(());
            }
        }
        None
    });
    // A late second tab or window would show up here ("exactly one").
    settle(QUIET_WATCH);
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = call.map(|c| c.join().unwrap_or_else(|_| Err("launcher thread panicked".into()))) {
        return Err(format!("{}: the call failed: {e}", trigger.label()));
    }
    if finished.is_none() {
        return Err(format!(
            "{}: the Primary_Window did not reach the expected state ({expect:?}) within {FOLD_GIVE_UP:?}; candidate \
             seen: {candidate_seen}; last view {last_view:?}; before {before:?}",
            trigger.label()
        ));
    }
    let done = done.ok_or("no completion instant")?;
    let after = h.view(h.primary);

    // The FOLD entry is written when the candidate's destruction is handled.
    let entries = wait(Duration::from_secs(3), || {
        let all = fold_entries_for(h, dir);
        (all.len() > entries_before).then_some(all)
    })
    .unwrap_or_default();
    let entries = entries
        .iter()
        .skip(entries_before)
        .map(|p| FoldFields { outcome: p.get_str("outcome"), into: p.get_str("into"), matching: p.get_str("matching") })
        .collect();

    Ok(ForegroundRun {
        trigger,
        expect,
        dir: dir.to_path_buf(),
        file: file.map(Path::to_path_buf),
        before,
        after,
        entries,
        leftover_windows: h.other_windows().len(),
        candidate_seen,
        completion_ms: ms_since(t0, done),
        max_shown_ms: max_shown_ms(&tracks, end),
        track_lines: describe_tracks(&tracks, end),
    })
}

/// Makes the Primary_Window the foreground window, triggers `trigger` in it
/// and checks Req 14.22: exactly one new tab on the same folder.
fn new_tab_in_foreground_window(h: &Harness, trigger: Trigger) -> Result<(), String> {
    let dir = h.primary_dir.clone();
    make_foreground(h.primary, "the Primary_Window")?;
    let run = foreground_case(h, trigger, Expect::NewTabInForeground, &dir, None, h.primary)?;
    println!("Primary_Window tabs after the trigger: {:?}", run.after.locations());
    run.print();
    run.check()?;
    check_revealed_foreground(h, &trigger.label())
}

/// Req 14.22: Ctrl+N in the foreground Primary_Window, on the folder of its
/// Active_Tab, gives exactly one new tab there (and no separate window).
/// Needs the keyboard to itself. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn ctrl_n_in_the_foreground_window_gives_exactly_one_new_tab_on_the_same_folder() {
    with_harness("fg_ctrl_n", Settings::default(), |h| new_tab_in_foreground_window(h, Trigger::CtrlN));
}

/// Req 14.22: "New window" (the `opennewwindow` verb, see the section
/// comment) in the foreground Primary_Window, on the folder of its Active_Tab,
/// gives exactly one new tab there. Needs the keyboard to itself. Unverified
/// on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn new_window_in_the_foreground_window_gives_exactly_one_new_tab_on_the_same_folder() {
    with_harness("fg_new_window", Settings::default(), |h| {
        new_tab_in_foreground_window(h, Trigger::OpenNewWindowVerb)
    });
}

/// Req 14.21 and 14.22: the folder of the Primary_Window's Active_Tab opened
/// by another application (the test's own non-Explorer window is in the
/// foreground), with `explorer.exe <dir>` and as "Show in folder", switches to
/// the existing tab: no new tab, and the requested file selected. Needs the
/// keyboard to itself. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn the_same_folder_opened_from_another_application_switches_to_the_existing_tab() {
    with_harness("fg_other_app", Settings::default(), |h| {
        let dir = h.primary_dir.clone();
        let file = dir.join("select_me.txt");
        std::fs::write(&file, b"x").map_err(|e| format!("create {}: {e}", file.display()))?;
        let app = AppWindow::open()?;
        for method in [OpenMethod::ExplorerExe, OpenMethod::ShOpenFolderAndSelectItems] {
            make_foreground(app.raw(), "the test's own (non-Explorer) window")?;
            let run =
                foreground_case(h, Trigger::Application(method), Expect::SwitchToExisting, &dir, Some(&file), app.raw())?;
            run.print();
            run.check()?;
            check_revealed_foreground(h, &run.trigger.label())?;
        }
        Ok(())
    });
}

/// Req 14.21 and 14.22 side by side, on a Primary_Window with two tabs on the
/// same folder: Ctrl+N in the foreground Primary_Window adds a tab, and
/// `explorer.exe <dir>` from another application afterwards adds none but
/// switches to the existing (active, rightmost) tab. Needs the keyboard to
/// itself. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn the_foreground_exception_adds_a_tab_but_another_application_does_not() {
    with_harness("fg_contrast", Settings::default(), |h| {
        let dir = h.primary_dir.clone();
        new_tab_in_foreground_window(h, Trigger::CtrlN)?;

        let app = AppWindow::open()?;
        make_foreground(app.raw(), "the test's own (non-Explorer) window")?;
        let run = foreground_case(
            h,
            Trigger::Application(OpenMethod::ExplorerExe),
            Expect::SwitchToExisting,
            &dir,
            None,
            app.raw(),
        )?;
        run.print();
        run.check()?;
        if run.after.tabs.len() != 2 {
            return Err(format!("the Primary_Window has {} tab(s), want the 2 that Ctrl+N made", run.after.tabs.len()));
        }
        check_revealed_foreground(h, &run.trigger.label())
    });
}

// ===========================================================================
// Task 27.5: windows that stay separate (Req 14.3 b and g, 14.4)
// ===========================================================================
//
// Two windows must never be folded, although another Explorer window is open
// on the desktop:
//
// * a window formed by **dragging a tab out** of another window (Req 14.3 b);
// * a window opened with **Ctrl+Shift held** (the Fold_Bypass_Keys, Req 14.3 g
//   and 14.4).
//
// Each must (1) stay open as its own window with the one tab, Location and
// selection Explorer gave it, (2) be revealed without delay, i.e. the product
// does not keep it at alpha 0 once Explorer shows it, and (3) be tracked by the
// Live_Session as any other window, so it is in the Session_File checkpoint
// next to the Primary_Window.
//
// DO NOT TOUCH THE KEYBOARD OR MOUSE WHILE THESE TESTS RUN. The Ctrl+Shift
// tests hold Ctrl and Shift down with `SendInput` for about a second (Windows'
// "Ctrl+Shift switches the input language" hotkey, if configured, fires when
// they are released), and the drag-out test moves the real cursor and presses
// the left button. Both keys and the button are released again, and the cursor
// restored, also when a test panics (`CtrlShiftDown`, `Mouse`).
//
// How each trigger is made (the choices follow the F3 spike,
// `crates/esr-win/tests/spike_f3_signals.rs`, which measured both on 22621 and
// 26300):
//
// * **Ctrl+Shift** ([`bypass_case`]): Ctrl down, Shift down, 30 ms pause, then
//   the folder is opened with a way that [`OpenMethod`] names, and the keys are
//   held until a poll has seen the new window show the folder plus
//   [`HOLD_AFTER_SEEN`]. The product reads the keys at the window's first
//   signal: 6-25 ms after a shell open, about 130 ms (up to ~475 ms under load)
//   after `explorer.exe <path>` from another process (design.md, "First signal
//   and classification"), which is why they are held until the window exists
//   and not released after a fixed short time. Three ways of opening, each in
//   its own test: `ShellExecute("open")` (what F3 used), `explorer.exe <dir>`
//   (the slowest first signal), and `SHOpenFolderAndSelectItems` (the
//   selection must survive, Req 14.4).
// * **Drag-out** ([`drag_out_case`]): the same technique as F3 and R5. The
//   Primary_Window is placed on the left half of the screen with a second tab
//   (added with the "new tab" command and `Navigate2`, as the spikes do), a
//   plain topmost "drop pad" window without a drop target is placed on the right
//   half, and the second tab's UI Automation item (found by `Uia::items`, its
//   bounding rectangle) is dragged with `SendInput` mouse events (absolute,
//   virtual-desk coordinates, a few 2 px steps past the drag threshold, then 30
//   steps of 15 ms, a 400 ms hover, then the button is released) onto the pad.
//   Dropping on a window that is not a tab strip makes Explorer form a new
//   window from the tab. Before pressing, the window under the start point
//   must be the Primary_Window. "Move tab to new window" is not offered by
//   Explorer on any build the design lists, so there is no menu variant.
//
// What each run checks ([`verify_separate`]):
//
// * only the separate window exists besides the Primary_Window and it is still
//   open after a 3 s watch (a fold would have closed it within 1.5 s);
// * the separate window has exactly one tab, showing the folder, with exactly
//   the requested file selected where one was requested; the Primary_Window has
//   exactly its own tab left (Ctrl+Shift: unchanged; drag-out: the dragged tab
//   is gone from it);
// * the Activity_Log has no `FOLD` entry at all. The design logs one `FOLD`
//   entry per *candidate* that is folded, switched or left open
//   (`not-folded`); a window excluded at its first signal (Ctrl+Shift) or
//   handed over on `TabDetached` (drag-out) is not a candidate that completes
//   and the engine's drag-out path emits only `Reveal` and `RescanWindow`, so
//   the expected state is "no folded entry", not a particular outcome;
// * visibility: the sampler (`Harness::sampler`) shows the window shown at the
//   end, and the time it was visible to Explorer but held at layered alpha 0
//   ([`held_back_ms`], from the sampler's state-change log) is at most
//   [`REVEAL_MAX_MS`]. For Ctrl+Shift the window must also never have been at
//   alpha 0 at all (it is never hidden). Req 14.16's 100 ms limit is for
//   windows that are *folded*; a separate window has no such limit, and
//   [`REVEAL_MAX_MS`] is a tolerance chosen for this test (the hider's style
//   change takes 31-94 ms to complete in F1), not a number from the
//   requirements;
// * tracking: the Session_File checkpoint lists exactly two windows, one with
//   the Primary_Window's folder and one with the separate window's folder, and
//   nothing else; the dragged folder is not in the Primary_Window's entry.
//
// Measurement limits (read before trusting a result):
//
// * Nothing here has been run on a machine yet: every assertion, bound,
//   timing and the drag itself are unverified until a Windows 11 VM run. The
//   drag in particular depends on the screen being at least 1100x600 and on
//   nothing else covering the Primary_Window's tab strip.
// * The sampler's state-change log keeps at most 40 changes per window (a
//   window that moves or resizes a lot can fill it). [`held_back_ms`] reads
//   only that log; a state after the 40th change is not seen.
// * The Session_File is read after the 3 s watch, so the 1 s of Requirement 2
//   is not measured; the test shows that the checkpoint lists the window
//   within [`TRACK_WAIT`] of the watch ending.
// * The window's Window_Placement ("as File Explorer opened it", Req 14.4) is
//   not compared: there is no reference to compare against.
// * Keyboard focus is not checked.

use win_common::explorer_guard::pump_for;
use win_common::fold_spike::{
    browser_of_tab, navigate, pidl_of_path, post_new_tab, tab_children, window_shows,
};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SetCursorPos,
    SetWindowPos, WINDOW_STYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WindowFromPoint,
};

/// How long a separate window is watched after it appeared (a fold would have
/// closed it well within this, Req 14.17).
const SEPARATE_WATCH: Duration = Duration::from_secs(3);
/// How long Ctrl+Shift stay down after a poll saw the new window.
const HOLD_AFTER_SEEN: Duration = Duration::from_millis(300);
/// How long the Session_File checkpoint may take to list the window.
const TRACK_WAIT: Duration = Duration::from_secs(5);
/// The longest a separate window may be visible to Explorer yet held at alpha
/// 0 (a test tolerance, see the section comment).
const REVEAL_MAX_MS: f64 = 100.0;

// --- Input ------------------------------------------------------------------

/// Ctrl and Shift held with `SendInput`; both released on drop, so they are
/// never left down.
struct CtrlShiftDown;

impl CtrlShiftDown {
    fn press() -> Self {
        send_key(VK_CONTROL, false);
        send_key(VK_SHIFT, false);
        Self
    }
}

impl Drop for CtrlShiftDown {
    fn drop(&mut self) {
        send_key(VK_SHIFT, true);
        send_key(VK_CONTROL, true);
    }
}

/// The real mouse for a drag; the button is released and the cursor put back
/// on drop.
struct Mouse {
    saved: POINT,
    left_down: bool,
}

impl Mouse {
    fn new() -> Self {
        let mut saved = POINT::default();
        // SAFETY: valid out pointer.
        let _ = unsafe { GetCursorPos(&mut saved) };
        Self { saved, left_down: false }
    }

    /// One absolute mouse event at `p` (virtual-desk coordinates).
    fn send(flags: MOUSE_EVENT_FLAGS, p: POINT) {
        // SAFETY: plain metric reads.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) as i64,
                GetSystemMetrics(SM_YVIRTUALSCREEN) as i64,
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2) as i64,
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2) as i64,
            )
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: ((p.x as i64 - vx) * 65535 / (vw - 1)) as i32,
                    dy: ((p.y as i64 - vy) * 65535 / (vh - 1)) as i32,
                    mouseData: 0,
                    dwFlags: flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: one fully initialized INPUT.
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    /// Presses on `from`, moves past the drag threshold, then to `to` in 30
    /// steps, hovers, and releases (the F3 and R5 drag).
    fn drag(&mut self, from: POINT, to: POINT) {
        Self::send(MOUSE_EVENT_FLAGS(0), from);
        pump_for(Duration::from_millis(150));
        self.left_down = true;
        Self::send(MOUSEEVENTF_LEFTDOWN, from);
        pump_for(Duration::from_millis(150));
        for i in 1..=5 {
            Self::send(MOUSE_EVENT_FLAGS(0), POINT { x: from.x, y: from.y + 2 * i });
            pump_for(Duration::from_millis(25));
        }
        let start = POINT { x: from.x, y: from.y + 10 };
        for i in 1..=30 {
            Self::send(
                MOUSE_EVENT_FLAGS(0),
                POINT { x: start.x + (to.x - start.x) * i / 30, y: start.y + (to.y - start.y) * i / 30 },
            );
            pump_for(Duration::from_millis(15));
        }
        Self::send(MOUSE_EVENT_FLAGS(0), POINT { x: to.x + 3, y: to.y });
        pump_for(Duration::from_millis(150));
        Self::send(MOUSE_EVENT_FLAGS(0), to);
        pump_for(Duration::from_millis(400));
        Self::send(MOUSEEVENTF_LEFTUP, to);
        self.left_down = false;
    }
}

impl Drop for Mouse {
    fn drop(&mut self) {
        if self.left_down {
            let mut p = POINT::default();
            // SAFETY: valid out pointer.
            let _ = unsafe { GetCursorPos(&mut p) };
            Self::send(MOUSEEVENTF_LEFTUP, p);
        }
        // SAFETY: plain call.
        let _ = unsafe { SetCursorPos(self.saved.x, self.saved.y) };
    }
}

/// A plain topmost window of the test with no drop target: a tab dropped on it
/// becomes a new Explorer window. `STATIC` with `SS_NOTIFY` (0x100), which is
/// hit-testable (a plain `STATIC` is transparent to the mouse). Created and
/// destroyed on the test thread, which pumps messages.
struct DropPad(HWND);

impl DropPad {
    fn create(r: &RECT) -> Result<Self, String> {
        // SAFETY: valid predefined class name; no parent, menu, or params.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("ESR tab folding test: drop pad"),
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(0x0000_0100),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW(STATIC): {e}"))?;
        Ok(Self(hwnd))
    }
}

impl Drop for DropPad {
    fn drop(&mut self) {
        // SAFETY: created by this thread, destroyed once.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// The top-level window under `p` (0 when none).
fn root_window_at(p: POINT) -> isize {
    // SAFETY: plain calls.
    unsafe {
        let w = WindowFromPoint(p);
        if w.is_invalid() { 0 } else { GetAncestor(w, GA_ROOT).0 as isize }
    }
}

// --- Reading the sampler and the Session_File --------------------------------

/// One state change from `Track::changes` ("t_ms state"): (time since the
/// window was first seen in ms, visible to Explorer yet held at alpha 0,
/// layered alpha 0).
fn state_changes(track: &Track) -> Vec<(f64, bool, bool)> {
    track
        .changes
        .iter()
        .filter_map(|line| {
            let (t, rest) = line.trim_start().split_once(" ms ")?;
            let t = t.trim().parse::<f64>().ok()?;
            let alpha0 = rest.contains("alpha=Some(0)");
            let held = alpha0 && rest.contains("vis=1") && rest.contains("cloak=0");
            Some((t, held, alpha0))
        })
        .collect()
}

/// Total time the window was visible to Explorer (`WS_VISIBLE`, not cloaked)
/// but at layered alpha 0, i.e. kept from the user by the hider.
pub fn held_back_ms(track: &Track, end: Instant) -> f64 {
    let Some(first) = track.first_seen else {
        return 0.0;
    };
    let end_ms = ms_since(first, end);
    let changes = state_changes(track);
    changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.1)
        .map(|(i, c)| (changes.get(i + 1).map_or(end_ms, |n| n.0) - c.0).max(0.0))
        .sum()
}

/// Whether the window was at layered alpha 0 in any recorded state.
pub fn ever_alpha0(track: &Track) -> bool {
    state_changes(track).iter().any(|c| c.2)
}

/// The Session_File's windows as sorted lists of normalized tab locations,
/// the windows sorted too (the order of windows is not under test).
fn doc_layout(doc: &SessionDoc) -> Vec<Vec<String>> {
    let mut windows: Vec<Vec<String>> = doc
        .snapshot
        .windows
        .iter()
        .map(|w| w.tabs.iter().map(|t| normalize(&t.location.parsing_name.to_string_lossy())).collect())
        .collect();
    windows.sort();
    windows
}

// --- Checks shared by both ways of getting a separate window ----------------

/// What `verify_separate` checks one run against.
struct SeparateCase<'a> {
    what: &'a str,
    /// The requirement(s) named in failure messages.
    req: &'a str,
    /// The folder the separate window shows.
    dir: &'a Path,
    /// The file that must be the one selected there, if one was requested.
    selected: Option<&'a Path>,
    /// The separate window.
    win: isize,
    /// The window was never a candidate, so it must never have been hidden.
    never_hidden: bool,
}

/// The shared checks of the section comment, after the watch.
fn verify_separate(
    h: &Harness,
    c: &SeparateCase,
    tracks: &HashMap<isize, Track>,
    end: Instant,
) -> Result<(), String> {
    let (what, req) = (c.what, c.req);
    let want = normalize(&c.dir.to_string_lossy());
    let primary_want = normalize(&h.primary_dir.to_string_lossy());

    // Still open, and the only window besides the Primary_Window.
    let others = h.other_windows();
    if others != [c.win] || !is_window(c.win) {
        return Err(format!(
            "{what}: windows other than the Primary_Window are {others:?}, want only the separate window {:?} still \
             open ({req})",
            hwnd_of(c.win)
        ));
    }

    // Its tab, Location and selection.
    let view = h.view(c.win);
    println!("  {what}: separate window {:?}: {view:?}", hwnd_of(c.win));
    if view.tabs.len() != 1 || view.tab_of(c.dir) != Some(0) {
        return Err(format!("{what}: the separate window has tabs {:?}, want exactly one showing {want} ({req})", view.locations()));
    }
    if let Some(file) = c.selected {
        let sel = vec![normalize(&file.to_string_lossy())];
        if view.tabs[0].selection != sel {
            return Err(format!("{what}: selection is {:?}, want exactly {sel:?} ({req})", view.tabs[0].selection));
        }
    }
    let primary = h.view(h.primary);
    println!("  {what}: Primary_Window {primary:?}");
    if primary.locations() != [primary_want.clone()] {
        return Err(format!(
            "{what}: the Primary_Window's tabs are {:?}, want only its own tab {primary_want} ({req})",
            primary.locations()
        ));
    }

    // The log: nothing was folded.
    let folds: Vec<(Option<String>, Option<String>)> =
        h.fold_entries().iter().map(|p| (p.get_str("location"), p.get_str("outcome"))).collect();
    if !folds.is_empty() {
        return Err(format!("{what}: the Activity_Log has FOLD entries {folds:?}, want none ({req})"));
    }

    // Revealed without delay.
    let track = tracks
        .get(&c.win)
        .ok_or_else(|| format!("{what}: the sampler never saw the separate window ({req})"))?;
    let shown_at_end = track.shown.last().is_some_and(|(_, b)| b.is_none());
    let held = held_back_ms(track, end);
    let first_shown = track.first_seen.zip(track.shown.first()).map(|(f, (a, _))| ms_since(f, *a));
    println!(
        "  {what}: shown at the end: {shown_at_end}; first shown {first_shown:?} ms after it was first seen; visible but \
         held at alpha 0 for {held:.0} ms; ever at alpha 0: {}",
        ever_alpha0(track)
    );
    for line in describe_tracks(tracks, end) {
        println!("      {line}");
    }
    for change in &track.changes {
        println!("      {change}");
    }
    if !shown_at_end {
        return Err(format!("{what}: the separate window is not shown at the end ({req})"));
    }
    if held > REVEAL_MAX_MS {
        return Err(format!(
            "{what}: the window was visible to Explorer but held at alpha 0 for {held:.0} ms, at most {REVEAL_MAX_MS} ms \
             allowed ({req})"
        ));
    }
    if c.never_hidden && ever_alpha0(track) {
        return Err(format!("{what}: the window was at alpha 0, but it is never a Folding_Candidate ({req})"));
    }

    // Tracked: both windows, one tab each, in the checkpoint.
    let mut layout = vec![vec![primary_want], vec![want]];
    layout.sort();
    let tracked = wait(TRACK_WAIT, || read_doc(&h.data_dir).filter(|d| doc_layout(d) == layout));
    if tracked.is_none() {
        return Err(format!(
            "{what}: within {TRACK_WAIT:?} the Session_File did not list exactly the windows {layout:?}; it lists {:?} \
             ({req})",
            read_doc(&h.data_dir).map(|d| doc_layout(&d))
        ));
    }
    println!("  {what}: the Session_File lists {layout:?}");
    Ok(())
}

// --- Ctrl+Shift --------------------------------------------------------------

/// Opens a new folder with `method` while Ctrl and Shift are held, and checks
/// that it stays a separate window (Req 14.3 g, 14.4).
fn bypass_case(h: &Harness, method: OpenMethod) -> Result<(), String> {
    let (dir, file) = h.dir_with_file("ctrl_shift", "target.txt");
    let file = method.selects().then_some(file);
    let before = h.view(h.primary);
    if before.tabs.len() != 1 || !h.other_windows().is_empty() {
        return Err(format!("setup: want the Primary_Window with one tab and no other window, got {before:?}"));
    }

    let sampler = h.sampler();
    let keys = CtrlShiftDown::press();
    settle(Duration::from_millis(30));
    let t0 = Instant::now();
    let mut launch = h.open(method, &dir, file.as_deref())?;
    // Hold the keys until the window exists (see the section comment).
    let seen = h.wait_for_tab(&dir, OPEN_TIMEOUT);
    settle(HOLD_AFTER_SEEN);
    drop(keys);
    settle(SEPARATE_WATCH);
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("{}: the call failed: {e}", method.label()));
    }
    let (win, _, at) = seen.ok_or_else(|| format!("{} did not open {} within {OPEN_TIMEOUT:?}", method.label(), dir.display()))?;
    println!("{} with Ctrl+Shift held: {} ms to the first poll that saw the folder, in window {:?}", method.label(), at.duration_since(t0).as_millis(), hwnd_of(win));
    if win == h.primary {
        return Err(format!("{} with Ctrl+Shift held opened a tab in the Primary_Window (14.4)", method.label()));
    }
    let what = format!("Ctrl+Shift + {}", method.label());
    verify_separate(
        h,
        &SeparateCase { what: &what, req: "14.3 g, 14.4", dir: &dir, selected: file.as_deref(), win, never_hidden: true },
        &tracks,
        end,
    )
}

/// Req 14.3 g, 14.4: `ShellExecute("open")` on a folder with Ctrl+Shift held
/// stays a separate window. Needs the keyboard to itself. Unverified on a
/// machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn ctrl_shift_shell_execute_open_stays_a_separate_window() {
    with_harness("bypass_shellexec", Settings::default(), |h| bypass_case(h, OpenMethod::ShellExecuteOpen));
}

/// Req 14.3 g, 14.4: `explorer.exe <dir>` with Ctrl+Shift held stays a
/// separate window (the way with the latest first signal). Needs the keyboard
/// to itself. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn ctrl_shift_explorer_exe_stays_a_separate_window() {
    with_harness("bypass_exe", Settings::default(), |h| bypass_case(h, OpenMethod::ExplorerExe));
}

/// Req 14.3 g, 14.4: "Show in folder" (`SHOpenFolderAndSelectItems`) with
/// Ctrl+Shift held stays a separate window with its selection. Needs the
/// keyboard to itself. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn ctrl_shift_show_in_folder_stays_a_separate_window_with_its_selection() {
    with_harness("bypass_shopen", Settings::default(), |h| bypass_case(h, OpenMethod::ShOpenFolderAndSelectItems));
}

// --- Drag-out ----------------------------------------------------------------

/// Adds a tab showing `dir` to the window `top` (the spikes' technique: the
/// "new tab" command, then `Navigate2`).
fn add_tab(top: isize, dir: &Path) -> Result<(), String> {
    let pidl = pidl_of_path(dir)?;
    let base: HashSet<isize> = tab_children(top).into_iter().collect();
    post_new_tab(top)?;
    let new_tab = wait(Duration::from_secs(4), || {
        tab_children(top).into_iter().find(|c| !base.contains(c)).filter(|&t| browser_of_tab(top, t).is_some())
    })
    .ok_or("the new tab did not register")?;
    let browser = browser_of_tab(top, new_tab).ok_or("the new tab has no browser")?;
    navigate(&browser, &pidl)?;
    wait(Duration::from_secs(4), || window_shows(top, &pidl).then_some(())).ok_or("the new tab did not navigate")?;
    settle(Duration::from_millis(500));
    Ok(())
}

/// Where the Primary_Window and the drop pad go: the window on the left half
/// of the screen, the pad on the right half, the drop at the pad's centre.
struct DragLayout {
    /// (x, y, width, height) of the Primary_Window.
    window: (i32, i32, i32, i32),
    pad: RECT,
    drop: POINT,
}

fn drag_layout() -> Result<DragLayout, String> {
    // SAFETY: plain metric reads.
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if sw < 1100 || sh < 600 {
        return Err(format!("the screen is {sw}x{sh}; the drag-out test needs at least 1100x600"));
    }
    let half = sw / 2;
    let pad = RECT { left: half + 20, top: 60, right: sw - 20, bottom: sh - 100 };
    Ok(DragLayout {
        window: (20, 20, half - 40, (sh - 120).min(700)),
        drop: POINT { x: (pad.left + pad.right) / 2, y: (pad.top + pad.bottom) / 2 },
        pad,
    })
}

/// Drags the second tab of the Primary_Window out onto the drop pad and checks
/// that the new window stays separate (Req 14.3 b).
fn drag_out_case(h: &Harness) -> Result<(), String> {
    let dragged = h.dir("dragged");
    let want = normalize(&dragged.to_string_lossy());
    let layout = drag_layout()?;
    let (x, y, w, hh) = layout.window;
    // SAFETY: plain call on the test's own window.
    unsafe {
        let _ = SetWindowPos(hwnd_of(h.primary), None, x, y, w, hh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    settle(Duration::from_millis(500));

    // A second tab, which the Live_Session has to know before it is dragged.
    add_tab(h.primary, &dragged)?;
    let mut two = vec![normalize(&h.primary_dir.to_string_lossy()), want.clone()];
    two.sort();
    let tracked = wait(TRACK_WAIT, || {
        read_doc(&h.data_dir).filter(|d| doc_layout(d) == [two.clone()])
    });
    if tracked.is_none() {
        return Err(format!(
            "setup: within {TRACK_WAIT:?} the Session_File did not list one window with tabs {two:?}; it lists {:?}",
            read_doc(&h.data_dir).map(|d| doc_layout(&d))
        ));
    }
    settle(Duration::from_millis(500)); // the tab's window is mapped to its parent
    make_foreground(h.primary, "the Primary_Window")?;

    // The second tab's item and the point to press on.
    let items = h.uia.items(h.primary);
    if items.len() != 2 {
        return Err(format!("setup: the tab strip exposes {} tab item(s), want 2", items.len()));
    }
    // SAFETY: plain UIA property read.
    let r = unsafe { items[1].CurrentBoundingRectangle() }.map_err(|e| format!("tab item rectangle: {e}"))?;
    let from = POINT { x: r.left + ((r.right - r.left) / 3).min(40), y: (r.top + r.bottom) / 2 };
    if root_window_at(from) != h.primary {
        return Err(format!(
            "setup: the window under the tab at ({}, {}) is {:?}, not the Primary_Window; is something covering it?",
            from.x,
            from.y,
            hwnd_of(root_window_at(from))
        ));
    }
    let pad = DropPad::create(&layout.pad)?;
    settle(Duration::from_millis(300));

    // The drag.
    let sampler = h.sampler();
    let mut mouse = Mouse::new();
    mouse.drag(from, layout.drop);
    let dropped = Instant::now();
    let found = wait(OPEN_TIMEOUT, || {
        windows_with_locations()
            .into_iter()
            .find(|(top, locs)| *top != h.primary && locs.contains(&want))
            .map(|(top, _)| (top, Instant::now()))
    });
    drop(pad);
    settle(SEPARATE_WATCH);
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    let (win, seen) = found.ok_or_else(|| format!("no window showing {want} appeared within {OPEN_TIMEOUT:?} of the drop"))?;
    println!(
        "Drag-out: a poll saw the dragged folder in the new window {:?} {} ms after the button was released",
        hwnd_of(win),
        seen.duration_since(dropped).as_millis()
    );
    verify_separate(
        h,
        &SeparateCase { what: "Drag-out", req: "14.3 b", dir: &dragged, selected: None, win, never_hidden: false },
        &tracks,
        end,
    )
}

/// Req 14.3 b: a tab dragged out of the Primary_Window becomes a separate
/// window that is not folded back, is revealed without delay and is tracked
/// next to the Primary_Window (the dragged tab is no longer in the
/// Primary_Window's entry). Needs the mouse to itself. Unverified on a
/// machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn a_tab_dragged_out_stays_a_separate_window_revealed_without_delay() {
    with_harness("drag_out", Settings::default(), drag_out_case);
}

// ===========================================================================
// Task 27.6: a second virtual desktop (Req 14.9, 14.11)
// ===========================================================================
//
// Each Virtual_Desktop has its own Primary_Window (Req 14.9): a folder opened
// on a desktop that has no Explorer window of its own is not folded into the
// window of another desktop, and a folder opened on the first desktop is still
// folded into the first desktop's window while the second desktop has one.
//
// DO NOT TOUCH THE KEYBOARD OR MOUSE WHILE THIS TEST RUNS. It injects Win+Ctrl+D
// (create a desktop and switch to it), Win+Ctrl+Left / Win+Ctrl+Right (switch)
// and Win+Ctrl+F4 (close the current desktop) with `SendInput`, so the screen
// changes desktop under you. Run it on a VM, not on a machine in use. Win-key
// shortcuts can be intercepted by the machine (a policy, a remote desktop
// client or another tool that owns the Win key); the test does not fail then,
// it SKIPS with a printed reason when the creation of the desktop cannot be
// confirmed.
//
// The steps ([`second_desktop_case`]):
//
// 1. The Primary_Window `P` is on desktop 1 (checked with
//    `IVirtualDesktopManager::IsWindowOnCurrentVirtualDesktop`, through the
//    product's own `esr_win::desktops::VirtualDesktops`; if the manager is not
//    available or `P` is not reported on the current desktop, the test skips).
// 2. Win+Ctrl+D. The creation is confirmed when `P` stops being on the
//    current desktop; otherwise the test skips. A plain top-level window of the
//    test (the "marker", [`AppWindow`]) is then created on desktop 2: it is how
//    the test finds desktop 2 again, to leave it and to close it.
// 3. On desktop 2, `explorer.exe B` (a folder). No Explorer window is on this
//    desktop, so the window is not a Folding_Candidate (design, "First signal
//    and classification", rule 2): it stays a separate window `W`, is not
//    hidden, has no `FOLD` entry, `P` is untouched. `W` is on desktop 2 and
//    `P` is not.
// 4. Still on desktop 2, `explorer.exe D`: `W` is now desktop 2's
//    Primary_Window, so D is folded into `W` (a new tab of `W`, the candidate
//    gone, `P` untouched). This is what shows that desktop 2 "gets its own
//    Primary_Window" and that `P` is not used for it.
// 5. Win+Ctrl+Left until `P` is on the current desktop, then `explorer.exe C`:
//    C is folded into `P` (a new tab of `P`, the candidate gone), not into
//    `W`, whose two tabs are unchanged and which is not on the current desktop.
// 6. Win+Ctrl+Right until the marker is current, then Win+Ctrl+F4 closes
//    desktop 2; Windows moves its windows to desktop 1 (the test's windows are
//    then closed by the harness as usual).
//
// Cleanup in every path: [`SecondDesktop`] closes desktop 2 and returns to
// desktop 1 in its `Drop`, so a failed assertion, an `Err` and a panic all
// leave the machine on desktop 1 with desktop 2 closed. Win, Ctrl and the
// other keys of a chord are released in reverse order by [`Chord`]'s `Drop`.
// Win+Ctrl+F4 is only ever sent while `P` is *not* on the current desktop, so
// the test cannot close the first desktop.
//
// Why `fold_via` is not used: it assumes the Primary_Window is the only window
// that is open and treats any other window as the candidate, which is not true
// here (`W` stays open). `fold_on_desktop` does the same job for a chosen
// target window: it opens the folder with `explorer.exe`, waits for a tab of
// the target and for the candidate to be gone and for the `FOLD` entry, keeps
// watching for [`QUIET_WATCH`], and checks the tabs of every window, the log
// and the shown time.
//
// Measurement limits (read before trusting a result):
//
// * Nothing here has been run on a machine yet: every assertion, timing and
//   the key injection (the Win-key chords above in particular) is unverified
//   until a Windows 11 VM run. The test was only compiled, not run, on the
//   development machine, because switching desktops and injecting keys there
//   is disruptive.
// * The desktop of `W` and of the marker is read with
//   `IsWindowOnCurrentVirtualDesktop` while that desktop is current; this
//   needs a thread with COM, which the harness has. Windows put a new
//   top-level window on the current desktop (design, "Virtual desktops").
// * The test assumes it starts with the desktop layout the user has; the
//   chords move one desktop at a time and repeat until the wanted window is
//   current, so extra desktops do not break the navigation, but they are
//   untested. Closing desktop 2 is checked by the marker having moved to the
//   current desktop after returning to desktop 1; an unconfirmed close is
//   reported (close the extra desktop by hand with Win+Ctrl+F4).
// * The all-desktops case of Req 14.11 is not exercised here (the shell offers
//   no stable way to pin a window to all desktops from a test); the
//   `desktops.rs` unit tests cover the `DesktopView` computation.
// * Keyboard focus and the foreground window are not checked.

use esr_win::desktops::{DesktopQuery, VirtualDesktops};
use windows::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_EXTENDEDKEY, VK_D, VK_F4, VK_LEFT, VK_LWIN, VK_RIGHT};

/// How long a switch of desktop is waited for before the chord is repeated.
const DESKTOP_SWITCH_WAIT: Duration = Duration::from_millis(2_500);
/// Pause after a switch (the switching animation) before the test acts.
const DESKTOP_ANIMATION: Duration = Duration::from_millis(700);
/// How many one-desktop hops a navigation tries before it gives up.
const DESKTOP_HOPS: usize = 8;

/// One key of a chord: the key and whether it is an extended key (arrows).
type ChordKey = (VIRTUAL_KEY, bool);

const WIN_CTRL_D: [ChordKey; 3] = [(VK_LWIN, false), (VK_CONTROL, false), (VK_D, false)];
const WIN_CTRL_F4: [ChordKey; 3] = [(VK_LWIN, false), (VK_CONTROL, false), (VK_F4, false)];

fn send_chord_key(vk: VIRTUAL_KEY, extended: bool, up: bool) {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // SAFETY: one fully initialized INPUT.
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}

/// Keys held with `SendInput`, pressed in order and released in reverse order
/// on drop, so none is ever left down (also when a test panics). A key that
/// was not pressed yet is not released.
struct Chord(Vec<ChordKey>);

impl Chord {
    fn press(keys: &[ChordKey]) -> Self {
        let mut chord = Self(Vec::new());
        for &(vk, extended) in keys {
            send_chord_key(vk, extended, false);
            chord.0.push((vk, extended));
            std::thread::sleep(Duration::from_millis(30));
        }
        chord
    }
}

impl Drop for Chord {
    fn drop(&mut self) {
        for &(vk, extended) in self.0.iter().rev() {
            send_chord_key(vk, extended, true);
        }
    }
}

/// Presses and releases a chord.
fn tap_chord(keys: &[ChordKey]) {
    let _chord = Chord::press(keys);
    std::thread::sleep(Duration::from_millis(30));
}

/// Whether `top` is on the current virtual desktop (`None`: the call failed).
fn on_current(vd: &VirtualDesktops, top: isize) -> Option<bool> {
    vd.on_current_desktop(top as usize as u64).ok()
}

/// Presses Win+Ctrl+`towards` (Left or Right), one desktop at a time, until
/// `target` is on the current desktop.
fn go_until(vd: &VirtualDesktops, towards: VIRTUAL_KEY, target: isize, what: &str) -> Result<(), String> {
    for _ in 0..DESKTOP_HOPS {
        if on_current(vd, target) == Some(true) {
            return Ok(());
        }
        tap_chord(&[(VK_LWIN, false), (VK_CONTROL, false), (towards, true)]);
        if wait(DESKTOP_SWITCH_WAIT, || (on_current(vd, target) == Some(true)).then_some(())).is_some() {
            settle(DESKTOP_ANIMATION);
            return Ok(());
        }
    }
    Err(format!(
        "could not switch to the desktop of {what} with Win+Ctrl+{} after {DESKTOP_HOPS} tries; was the keyboard used, or \
         is the shortcut intercepted?",
        if towards == VK_LEFT { "Left" } else { "Right" }
    ))
}

/// What [`SecondDesktop::create`] produced.
enum Created {
    Ready(SecondDesktop),
    /// A precondition does not hold or the desktop could not be confirmed;
    /// nothing was left behind.
    Skipped(String),
}

/// The second virtual desktop, created with Win+Ctrl+D and closed again in
/// `Drop` (see the section comment).
struct SecondDesktop {
    vd: VirtualDesktops,
    /// The Primary_Window, which stays on desktop 1.
    first: isize,
    /// A window of the test created on desktop 2, to find it again.
    marker: Option<AppWindow>,
    closed: bool,
}

impl SecondDesktop {
    fn create(first: isize) -> Created {
        let vd = match VirtualDesktops::new() {
            Ok(vd) => vd,
            Err(e) => return Created::Skipped(format!("IVirtualDesktopManager is not available: {e}")),
        };
        match on_current(&vd, first) {
            Some(true) => {}
            other => {
                return Created::Skipped(format!(
                    "IsWindowOnCurrentVirtualDesktop does not report the Primary_Window on the current desktop ({other:?})"
                ));
            }
        }
        tap_chord(&WIN_CTRL_D);
        let switched = wait(DESKTOP_SWITCH_WAIT, || (on_current(&vd, first) == Some(false)).then_some(()));
        if switched.is_none() {
            return Created::Skipped(
                "Win+Ctrl+D did not create and switch to a second desktop within 2.5 s (the shortcut may be intercepted \
                 by this machine, or virtual desktops are disabled); the Primary_Window is still on the current desktop"
                    .into(),
            );
        }
        settle(DESKTOP_ANIMATION);
        // From here a desktop exists: whatever happens, `Drop` closes it.
        let mut desk = SecondDesktop { vd, first, marker: None, closed: false };
        match AppWindow::open() {
            Ok(marker) => {
                let on_second = wait(Duration::from_secs(2), || (on_current(&desk.vd, marker.raw()) == Some(true)).then_some(()));
                desk.marker = Some(marker);
                if on_second.is_none() {
                    return Created::Skipped("the marker window created on the new desktop is not reported there".into());
                }
            }
            Err(e) => return Created::Skipped(format!("could not create the marker window on the new desktop: {e}")),
        }
        Created::Ready(desk)
    }

    /// Switches to desktop 1.
    fn go_first(&self) -> Result<(), String> {
        go_until(&self.vd, VK_LEFT, self.first, "the Primary_Window (desktop 1)")
    }

    /// Closes desktop 2 and returns to desktop 1. Safe to call twice; never
    /// sends Win+Ctrl+F4 while the Primary_Window's desktop is current.
    fn close(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        if on_current(&self.vd, self.first) != Some(false) {
            let marker = self.marker.as_ref().ok_or("no marker window to find desktop 2 with")?.raw();
            go_until(&self.vd, VK_RIGHT, marker, "the marker window (desktop 2)")?;
        }
        if on_current(&self.vd, self.first) != Some(false) {
            return Err("refusing to close the current desktop: the Primary_Window is on it".into());
        }
        tap_chord(&WIN_CTRL_F4);
        settle(DESKTOP_ANIMATION);
        self.go_first()?;
        // Closing moves the windows of the closed desktop to desktop 1.
        let marker_moved = match &self.marker {
            Some(m) => wait(Duration::from_secs(2), || (on_current(&self.vd, m.raw()) == Some(true)).then_some(())).is_some(),
            None => true,
        };
        if !marker_moved {
            return Err("desktop 2 may still be open (the marker window is not on desktop 1); close it by hand with Win+Ctrl+F4".into());
        }
        self.closed = true;
        Ok(())
    }
}

impl Drop for SecondDesktop {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            println!("WARNING: cleaning up the second virtual desktop: {e}");
        }
    }
}

/// The normalized tab locations of the window `top` (empty if it is gone).
fn locations_of(top: isize) -> Vec<String> {
    windows_with_locations().into_iter().find(|(t, _)| *t == top).map(|(_, l)| l).unwrap_or_default()
}

/// Opens `dir` with `explorer.exe` and checks that it is folded into `target`
/// (a window on the current desktop): one new rightmost, active tab there,
/// the candidate gone, every other window unchanged, one
/// `FOLD ... outcome=folded-primary` entry, shown for at most 100 ms (Req
/// 14.9, 14.16).
fn fold_on_desktop(h: &Harness, what: &str, dir: &Path, target: isize) -> Result<(), String> {
    let want = normalize(&dir.to_string_lossy());
    let windows_before = h.windows();
    let layout_before = windows_with_locations();
    let target_before = layout_before
        .iter()
        .find(|(t, _)| *t == target)
        .map(|(_, l)| l.clone())
        .ok_or_else(|| format!("{what}: setup: the target window {:?} is not listed by IShellWindows", hwnd_of(target)))?;
    let entries_before = fold_entries_for(h, dir).len();

    let sampler = h.sampler();
    let mut launch = h.open(OpenMethod::ExplorerExe, dir, None)?;
    let folded = wait(OPEN_TIMEOUT.max(FOLD_GIVE_UP), || {
        let landed = locations_of(target).contains(&want);
        let candidate_gone = h.windows().iter().all(|w| windows_before.contains(w));
        (landed && candidate_gone && fold_entries_for(h, dir).len() > entries_before).then_some(())
    });
    settle(QUIET_WATCH); // a late second tab or window would show up here
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("{what}: the call failed: {e}"));
    }
    if folded.is_none() {
        return Err(format!(
            "{what}: {} was not folded into {:?} within {FOLD_GIVE_UP:?} (14.9); windows now {:?}, layout {:?}, before {layout_before:?}",
            dir.display(),
            hwnd_of(target),
            h.windows(),
            windows_with_locations()
        ));
    }

    // The windows: no new one left, none lost.
    let mut now = h.windows();
    let mut before = windows_before.clone();
    now.sort();
    before.sort();
    if now != before {
        return Err(format!("{what}: the open Explorer windows are {now:?}, want {before:?}: the candidate was left open (14.9)"));
    }
    // The target: exactly one new rightmost tab on the folder, earlier tabs unchanged.
    let target_after = locations_of(target);
    let mut expected = target_before.clone();
    expected.push(want.clone());
    if target_after != expected {
        return Err(format!("{what}: the target window's tabs are {target_after:?}, want {expected:?} (14.9)"));
    }
    // Every other window: unchanged, in particular without the folder.
    for (top, locs) in layout_before.iter().filter(|(t, _)| *t != target) {
        let after = locations_of(*top);
        if after != *locs {
            return Err(format!(
                "{what}: window {:?} had tabs {locs:?} and has {after:?}; a folder was folded into the wrong desktop's window (14.9)",
                hwnd_of(*top)
            ));
        }
    }
    // The target's active tab (checked when the strip exposes captions).
    let view = h.view(target);
    if view.active_caption().is_some() && !view.active_is(dir) {
        return Err(format!("{what}: the active tab of the target is {:?}, want {} (14.9)", view.active_caption(), leaf(dir)));
    }
    // The log.
    let entries: Vec<(Option<String>, Option<String>)> =
        fold_entries_for(h, dir).iter().skip(entries_before).map(|p| (p.get_str("outcome"), p.get_str("reason"))).collect();
    if entries.len() != 1 || entries[0].0.as_deref() != Some("folded-primary") {
        return Err(format!("{what}: the new FOLD entries for the folder are {entries:?}, want exactly one folded-primary (14.9)"));
    }
    let shown = max_shown_ms(&tracks, end);
    println!("  {what}: folded into {:?}, tabs now {target_after:?}; max shown {shown:.0} ms", hwnd_of(target));
    for line in describe_tracks(&tracks, end) {
        println!("      {line}");
    }
    if shown > MAX_CANDIDATE_VISIBLE_MS {
        return Err(format!(
            "{what}: a window was shown for {shown:.0} ms in total, at most {MAX_CANDIDATE_VISIBLE_MS} ms allowed (14.16)"
        ));
    }
    Ok(())
}

/// Step 3: on desktop 2, a folder opened with no Explorer window on that
/// desktop stays a separate window, which is returned.
fn open_on_empty_desktop(h: &Harness, desk: &SecondDesktop) -> Result<isize, String> {
    let dir = h.dir("desk2_first");
    let want = normalize(&dir.to_string_lossy());
    let primary_want = normalize(&h.primary_dir.to_string_lossy());

    let sampler = h.sampler();
    let mut launch = h.open(OpenMethod::ExplorerExe, &dir, None)?;
    let found = h.wait_for_tab(&dir, OPEN_TIMEOUT);
    settle(SEPARATE_WATCH); // a fold would have closed the window within 1.5 s
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("the call failed: {e}"));
    }
    let (win, _, _) =
        found.ok_or_else(|| format!("no window showing {want} appeared on desktop 2 within {OPEN_TIMEOUT:?}"))?;
    if win == h.primary {
        return Err("a folder opened on desktop 2 was folded into desktop 1's Primary_Window (14.9)".into());
    }

    let others = h.other_windows();
    if others != [win] || !is_window(win) {
        return Err(format!("windows other than the Primary_Window are {others:?}, want only {:?} still open (14.9)", hwnd_of(win)));
    }
    if locations_of(win) != [want.clone()] {
        return Err(format!("the desktop-2 window has tabs {:?}, want exactly {want} (14.9)", locations_of(win)));
    }
    if locations_of(h.primary) != [primary_want.clone()] {
        return Err(format!("the Primary_Window's tabs are {:?}, want only {primary_want} (14.9)", locations_of(h.primary)));
    }
    // Desktops: the new window is on desktop 2 (current), the Primary_Window is not.
    if on_current(&desk.vd, win) != Some(true) || on_current(&desk.vd, h.primary) != Some(false) {
        return Err(format!(
            "desktop membership: new window current {:?}, Primary_Window current {:?}; want Some(true) and Some(false)",
            on_current(&desk.vd, win),
            on_current(&desk.vd, h.primary)
        ));
    }
    // It was never a Folding_Candidate: no FOLD entry, not kept hidden.
    let folds: Vec<(Option<String>, Option<String>)> =
        h.fold_entries().iter().map(|p| (p.get_str("location"), p.get_str("outcome"))).collect();
    if !folds.is_empty() {
        return Err(format!("the Activity_Log has FOLD entries {folds:?}, want none for a window with no Explorer window on its desktop (14.9)"));
    }
    let track = tracks.get(&win).ok_or("the sampler never saw the desktop-2 window")?;
    let shown_at_end = track.shown.last().is_some_and(|(_, b)| b.is_none());
    let held = held_back_ms(track, end);
    println!(
        "  desktop 2: {:?} stayed a separate window showing {want}; shown at the end: {shown_at_end}; held at alpha 0 for \
         {held:.0} ms; ever at alpha 0: {}",
        hwnd_of(win),
        ever_alpha0(track)
    );
    if !shown_at_end {
        return Err("the desktop-2 window is not shown at the end (14.9)".into());
    }
    if held > REVEAL_MAX_MS {
        return Err(format!("the desktop-2 window was held at alpha 0 for {held:.0} ms, at most {REVEAL_MAX_MS} ms allowed"));
    }
    Ok(win)
}

/// Steps 3 to 5 (see the section comment).
fn on_both_desktops(h: &Harness, desk: &SecondDesktop) -> Result<(), String> {
    // 3. Desktop 2 has no Explorer window: the folder gets its own window.
    let second = open_on_empty_desktop(h, desk)?;

    // 4. That window is now desktop 2's Primary_Window.
    let on_two = h.dir("desk2_second");
    fold_on_desktop(h, "desktop 2", &on_two, second)?;

    // 5. Back on desktop 1, a folder still folds into the Primary_Window.
    desk.go_first()?;
    if on_current(&desk.vd, second) != Some(false) {
        return Err(format!("after switching to desktop 1 the desktop-2 window is reported on the current desktop: {:?}", on_current(&desk.vd, second)));
    }
    let on_one = h.dir("desk1_folded");
    fold_on_desktop(h, "desktop 1", &on_one, h.primary)?;
    println!("  desktop 2's window kept its tabs: {:?}", locations_of(second));
    Ok(())
}

/// The whole test body (see the section comment): skips, without failing,
/// when the second desktop cannot be created and confirmed.
fn second_desktop_case(h: &Harness) -> Result<(), String> {
    let before = h.view(h.primary);
    if before.tabs.len() != 1 || !h.other_windows().is_empty() {
        return Err(format!("setup: want the Primary_Window with one tab and no other window, got {before:?}"));
    }
    let mut desk = match SecondDesktop::create(h.primary) {
        Created::Skipped(reason) => {
            println!("SKIPPED second_desktop: {reason}");
            return Ok(());
        }
        Created::Ready(desk) => desk,
    };
    let result = on_both_desktops(h, &desk);
    // On success a failed close is the test's failure; after a failure it is
    // only reported (`Drop` tries again).
    let closed = desk.close();
    match (result, closed) {
        (Err(e), Err(c)) => Err(format!("{e} (and closing the second desktop failed: {c})")),
        (Err(e), Ok(())) => Err(e),
        (Ok(()), Err(c)) => Err(format!("closing the second desktop failed: {c}")),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Req 14.9 and 14.11: a second virtual desktop (Win+Ctrl+D) gets its own
/// Primary_Window; a folder opened there is not folded into the first
/// desktop's window, a second folder opened there is folded into that window,
/// and a folder opened on the first desktop is still folded into the first
/// desktop's Primary_Window. Switches desktops with injected Win-key
/// shortcuts, so the keyboard must not be used; skips when the desktop cannot
/// be created and confirmed. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn a_second_virtual_desktop_gets_its_own_primary_window() {
    with_harness("second_desktop", Settings::default(), second_desktop_case);
}

// ===========================================================================
// Task 27.7: restoration in progress, folding off, and the log
// (Req 12.10, 14.3 c, d and g, 14.30)
// ===========================================================================
//
// Three things that must NOT fold, and the log that records what did:
//
// * **Restoration in progress (Req 14.3 c, d; design, "First signal and
//   classification", rule 3).** A folder opened by another process while a
//   restoration runs is not a Folding_Candidate: folding it into the
//   Trigger_Window would interfere with restoring it. It stays a separate
//   window with its own single tab, is never hidden, and the restoration
//   completes as if nothing had happened ([`restoration_in_progress_case`]).
// * **Folding off (Req 12.10).** With `tab_folding_enabled=false` a folder
//   opened while another window is open stays a separate window with the tab,
//   Location and selection Explorer gave it, is never hidden, and is tracked
//   as usual ([`folding_off_case`], which reuses [`verify_separate`]).
// * **The Activity_Log (Req 14.30).** One `FOLD` entry per Folding_Candidate
//   whose fold completes or stops, with its Location, its outcome and the
//   fields that outcome has ([`fold_log_case`]).
//
// How the restoration is started: [`Harness::start_with_session`] writes a
// Pending Session_File (one window, [`RESTORE_TABS`] local folders) into the
// temp data directory and starts the runtime on it, as `restore_timing.rs`
// does; no Explorer window exists then. The test "opens File Explorer" with
// `explorer.exe` and no argument ([`launch_default_explorer`]: a
// Default_Launch, the same call as `restore_timing.rs`), which is the
// Trigger_Window; as soon as it exists the test opens another folder with
// `explorer.exe <dir>`. That window's first signal arrives at least ~130 ms
// later, while the restoration (about 0.3 s + 0.27 s per tab, so ~3 s for ten
// tabs) is running or, at the least, while the Trigger_Window's restoration
// is waiting to start; both are rule 3. The test fails its setup, rather than
// passing, when the `RESTORE` entry was already in the log when the folder's
// window was first seen.
//
// What the restoration test checks:
//
// * the `RESTORE` entry shows `windows=1 tabs=<RESTORE_TABS> omitted=0` and no
//   `ERROR operation=restore-…` entry exists: the restoration completed;
// * exactly two Explorer windows exist: the Trigger_Window holding exactly the
//   saved folders, and the folder's window holding exactly its one folder
//   (not folded, not a tab of the restored window);
// * the Activity_Log has no `FOLD` entry (the window was excluded, it was
//   never a candidate that completes or stops, see the next section);
// * the folder's window was never at layered alpha 0 and is shown at the end
//   (it was never hidden), per the sampler.
//
// What the folding-off tests check: [`verify_separate`] with `never_hidden`:
// the window is still open after a 3 s watch, has exactly its folder (and the
// requested selection), the Primary_Window keeps its one tab, no `FOLD` entry,
// revealed without delay, never at alpha 0, and the Session_File lists both
// windows.
//
// What the log test checks: four candidates in one session, then one window
// that is not a candidate:
//
//   1. `explorer.exe <dir A>`                     -> `folded-primary`
//   2. `explorer.exe /select,<file in dir B>`     -> `folded-primary`
//   3. `explorer.exe <the Primary_Window's dir>`  -> `switched matching=primary`
//   4. `explorer.exe /select,<file in dir A>`     -> `switched matching=primary`
//   5. `explorer.exe <dir C>` with Ctrl+Shift held -> excluded (14.3 g), a
//      separate window, **no** `FOLD` entry.
//
// After the sequence the log must hold exactly four `FOLD` entries, in that
// order, each with the Location of its folder, the expected `outcome` and
// exactly the fields of the design's table (`folded-primary`: none besides
// `location` and `outcome`; `switched`: `matching`), and the entry of step 5
// must be absent. The line format (`timestamp`, `FOLD`, escaped `key=value`
// fields) is checked by parsing each line with the product's own
// `parse_line`, which rejects a malformed line, so a `FOLD` line that did not
// parse would be missing from the count.
//
// Not covered, and why:
//
// * The other outcomes (`folded-fallback`, `folded-foreground`,
//   `switch-failed-folded`, and `not-folded` with each reason) need an
//   unresponsive or closing window, a failed activation or an 8 s first
//   navigation timeout, which a test cannot cause reliably on a real Explorer
//   (`folded-foreground` needs Ctrl+N, 27.4 checks its entry). Their entries
//   are covered by the `esr-core` fold properties and `logfmt` tests.
// * A "left open" candidate (`not-folded`) cannot be made here: the one cause a
//   test can reach, turning the option off while a fold is in progress, needs a
//   Settings change at a chosen instant. A window excluded at its first signal
//   (folding off, restoration, Ctrl+Shift) is not a candidate and, by the
//   design, gets no `FOLD` entry; that is what is asserted for step 5 and for
//   the two exclusion tests.
// * Known gap: no `FOLD` entry is written for a candidate whose Location was
//   never known (the entry carries `location=`, which the engine cannot fill
//   in); such a candidate is left open without an entry. Not testable
//   without a window that never reports a Location.
// * After the restoration the Live_Session must include the window the user
//   opened (Req 4.5); that is not checked here, only that the window stays
//   separate and unchanged.
//
// DO NOT TOUCH THE KEYBOARD OR MOUSE WHILE THE LOG TEST RUNS: step 5 holds Ctrl
// and Shift down with `SendInput` (as the 27.5 tests do).
//
// Measurement limits (read before trusting a result):
//
// * Nothing here has been run on a machine yet: every assertion, bound,
//   timing and the restoration race above is unverified until a Windows 11
//   VM run. These tests were only compiled on the development machine.
// * "The restoration is in progress" is established by the order of two
//   observations (the folder's window seen, no `RESTORE` entry yet), each
//   made by polling every 20 ms; a restoration that finishes between the
//   window's first signal and the poll that sees it would be reported as a
//   setup failure, not as a pass.
// * The visibility checks rely on the ~1 ms sampler and its 40-change log
//   (see the 27.5 section).

use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};

/// Tabs of the saved window of the restoration test: about 3 s of
/// restoration (0.3 s + 10 x 0.27 s, Req 7.1), long enough for the folder to
/// be opened during it.
const RESTORE_TABS: usize = 10;
/// How long the restoration test waits for the folder's window and the
/// `RESTORE` entry.
const RESTORE_WAIT: Duration = Duration::from_secs(40);
/// How long the folder's window is watched after the restoration completed
/// (a fold would have closed it within 1.5 s, Req 14.17).
const AFTER_RESTORE_WATCH: Duration = Duration::from_millis(2_000);

/// "The user opens File Explorer": `explorer.exe` without arguments, which
/// opens the "Open File Explorer to" location in a new window when no
/// Explorer window is open (a Default_Launch; the same call as
/// `restore_timing.rs`).
fn launch_default_explorer() -> Result<(), String> {
    std::thread::spawn(|| {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpFile: w!("explorer.exe"),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // SAFETY: `sei` is fully initialised; the strings are static.
        unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| format!("ShellExecuteExW(explorer.exe): {e}"))
    })
    .join()
    .map_err(|_| "launcher thread panicked".to_string())?
}

/// Runs a test body on a harness started with a Pending Session_File (see
/// [`Harness::start_with_session`]), or reports the skip.
fn with_session_harness(
    name: &str,
    settings: Settings,
    windows: Vec<Vec<String>>,
    body: impl FnOnce(&Harness) -> Result<(), String>,
) {
    match Harness::start_with_session(name, settings, windows) {
        Started::Skipped(reason) => println!("SKIPPED {name}: {reason}"),
        Started::Ready(h) => {
            let result = body(&h);
            h.conclude(result);
        }
    }
}

/// The names of the fields of a log line, in line order.
fn field_names(p: &ParsedLine) -> Vec<&str> {
    p.fields.iter().map(|(k, _)| k.as_str()).collect()
}

fn restore_logged(h: &Harness) -> bool {
    h.log().iter().any(|p| p.event == "RESTORE")
}

// --- Restoration in progress (Req 14.3 c, d) ---------------------------------

/// Starts a restoration with `explorer.exe`, opens a folder with
/// `explorer.exe <dir>` while it runs, and checks that the folder stays a
/// separate, never hidden window, the restoration completes, and nothing is
/// logged as folded (see the section comment).
fn restoration_in_progress_case(h: &Harness) -> Result<(), String> {
    let [saved] = h.saved.as_slice() else {
        return Err(format!("setup: want one saved window, got {:?}", h.saved));
    };
    if !h.windows().is_empty() {
        return Err("setup: an Explorer window is open before the Trigger_Window".into());
    }
    let extra = h.dir("opened_during_restoration");
    let want = normalize(&extra.to_string_lossy());

    // The Trigger_Window.
    let sampler = h.sampler();
    launch_default_explorer()?;
    let trigger = wait(OPEN_TIMEOUT, || h.windows().into_iter().next())
        .ok_or_else(|| format!("explorer.exe opened no window within {OPEN_TIMEOUT:?}"))?;

    // The folder, opened by another process while the restoration runs. With
    // one saved window the Trigger_Window is that window, so the first
    // top-level window other than the trigger is the folder's. Polling only
    // lists top-level windows (cheap) so as not to slow Explorer down.
    let mut launch = h.open(OpenMethod::ExplorerExe, &extra, None)?;
    let mut extra_win: Option<(isize, bool)> = None; // (window, RESTORE already logged when first seen)
    let mut restored = false;
    let done = wait(RESTORE_WAIT, || {
        if extra_win.is_none()
            && let Some(w) = h.windows().into_iter().find(|&w| w != trigger)
        {
            extra_win = Some((w, restore_logged(h)));
        }
        restored = restored || restore_logged(h);
        (extra_win.is_some() && restored).then_some(())
    });
    settle(AFTER_RESTORE_WATCH); // a late fold or hide would show up here
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("{}: the call failed: {e}", launch.method.label()));
    }
    let Some((win, finished_before)) = extra_win else {
        return Err(format!("no window other than the Trigger_Window appeared for {want} within {RESTORE_WAIT:?}"));
    };
    if done.is_none() {
        return Err(format!("no RESTORE entry within {RESTORE_WAIT:?}; activity.log:\n{}", h.log_text()));
    }
    if finished_before {
        return Err(
            "setup: the RESTORE entry was already logged when the folder's window was first seen; the folder was \
             not opened during the restoration"
                .into(),
        );
    }

    // The restoration completed (14.3 c, d: it was not disturbed).
    let restore = h.log().into_iter().find(|p| p.event == "RESTORE").ok_or("the RESTORE entry disappeared")?;
    let num = |k: &str| restore.get_str(k).and_then(|v| v.parse::<u64>().ok());
    println!("RESTORE entry: {:?}", restore.fields.iter().map(|(k, v)| format!("{k}={}", v.to_string_lossy())).collect::<Vec<_>>());
    if num("windows") != Some(1) || num("tabs") != Some(saved.len() as u64) || num("omitted") != Some(0) {
        return Err(format!(
            "the RESTORE entry is windows={:?} tabs={:?} omitted={:?}, want windows=1 tabs={} omitted=0 (14.3 c, d)",
            num("windows"),
            num("tabs"),
            num("omitted"),
            saved.len()
        ));
    }
    let errors: Vec<String> = h
        .log()
        .iter()
        .filter(|p| p.event == "ERROR" && p.get_str("operation").is_some_and(|o| o.starts_with("restore")))
        .map(|p| format!("{:?}", field_names(p)))
        .collect();
    if !errors.is_empty() {
        return Err(format!("the restoration logged errors: {errors:?} (14.3 c, d)"));
    }

    // The windows: the folder's own window, untouched, and the restored one.
    let layout = windows_with_locations();
    println!("windows after the restoration: {layout:?} (Trigger_Window {:?}, folder's window {:?})", hwnd_of(trigger), hwnd_of(win));
    if layout.len() != 2 || !is_window(win) {
        return Err(format!("want exactly two windows (the restored one and the folder's), got {layout:?} (14.3 c, d)"));
    }
    let extra_tabs = layout.iter().find(|(t, _)| *t == win).map(|(_, l)| l.clone());
    if extra_tabs.as_deref() != Some(std::slice::from_ref(&want)) {
        return Err(format!("the folder's window has tabs {extra_tabs:?}, want exactly [{want}] (14.3 c, d)"));
    }
    let restored_tabs = layout.iter().find(|(t, _)| *t != win).map(|(_, l)| l.clone()).unwrap_or_default();
    let (mut have, mut expect) = (restored_tabs.clone(), saved.clone());
    have.sort();
    expect.sort();
    if have != expect {
        return Err(format!("the restored window has tabs {restored_tabs:?}, want the saved {saved:?} (14.3 c, d)"));
    }

    // Nothing was folded, and the window was never hidden.
    let folds: Vec<(Option<String>, Option<String>)> =
        h.fold_entries().iter().map(|p| (p.get_str("location"), p.get_str("outcome"))).collect();
    if !folds.is_empty() {
        return Err(format!("the Activity_Log has FOLD entries {folds:?}, want none for a window opened during a restoration (14.3 c, d)"));
    }
    let track = tracks.get(&win).ok_or("the sampler never saw the folder's window")?;
    let shown_at_end = track.shown.last().is_some_and(|(_, b)| b.is_none());
    let held = held_back_ms(track, end);
    println!(
        "  folder's window {:?}: shown at the end: {shown_at_end}; held at alpha 0 for {held:.0} ms; ever at alpha 0: {}",
        hwnd_of(win),
        ever_alpha0(track)
    );
    for line in describe_tracks(&tracks, end) {
        println!("      {line}");
    }
    if !shown_at_end {
        return Err("the folder's window is not shown at the end (14.3 c, d)".into());
    }
    if ever_alpha0(track) || held > 0.0 {
        return Err(format!(
            "the folder's window was at layered alpha 0 (held {held:.0} ms), but it is never a Folding_Candidate (14.3 c, d)"
        ));
    }
    Ok(())
}

/// Req 14.3 c and d (and 4.2): a folder opened by another process while a
/// restoration is running is left alone: it stays a separate window with its
/// own tab, is never hidden, and the restoration completes. Unverified on a
/// machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn a_folder_opened_during_a_restoration_is_left_alone() {
    let windows = vec![(0..RESTORE_TABS).map(|i| format!("fold_restore_t{i:02}")).collect()];
    with_session_harness("restoring", Settings::default(), windows, restoration_in_progress_case);
}

// --- Folding off (Req 12.10) -------------------------------------------------

/// Settings with `tab_folding_enabled=false`.
fn folding_off() -> Settings {
    Settings { tab_folding_enabled: false, ..Settings::default() }
}

/// Opens a new folder with `method` while another window is open and folding
/// is off, and checks that it stays a separate window (Req 12.10).
fn folding_off_case(h: &Harness, method: OpenMethod) -> Result<(), String> {
    let (dir, file) = h.dir_with_file("folding_off", "target.txt");
    let file = method.selects().then_some(file);
    let before = h.view(h.primary);
    if before.tabs.len() != 1 || !h.other_windows().is_empty() {
        return Err(format!("setup: want the Primary_Window with one tab and no other window, got {before:?}"));
    }

    let sampler = h.sampler();
    let t0 = Instant::now();
    let mut launch = h.open(method, &dir, file.as_deref())?;
    let seen = h.wait_for_tab(&dir, OPEN_TIMEOUT);
    settle(SEPARATE_WATCH); // a fold would have closed the window within 1.5 s
    let end = Instant::now();
    let tracks = sampler.stop();
    h.adopt_all();
    if let Some(Err(e)) = launch.call_result() {
        return Err(format!("{}: the call failed: {e}", method.label()));
    }
    let (win, _, at) =
        seen.ok_or_else(|| format!("{} did not open {} within {OPEN_TIMEOUT:?}", method.label(), dir.display()))?;
    println!(
        "{} with folding off: {} ms to the first poll that saw the folder, in window {:?}",
        method.label(),
        at.duration_since(t0).as_millis(),
        hwnd_of(win)
    );
    if win == h.primary {
        return Err(format!("{} with folding off opened a tab in the Primary_Window (12.10)", method.label()));
    }
    let what = format!("Folding off + {}", method.label());
    verify_separate(
        h,
        &SeparateCase { what: &what, req: "12.10", dir: &dir, selected: file.as_deref(), win, never_hidden: true },
        &tracks,
        end,
    )
}

/// Req 12.10: with `tab_folding_enabled=false`, `explorer.exe <dir>` opens a
/// window that stays a window. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn folding_off_explorer_exe_stays_a_separate_window() {
    with_harness("off_exe", folding_off(), |h| folding_off_case(h, OpenMethod::ExplorerExe));
}

/// Req 12.10 and 14.4: with `tab_folding_enabled=false`, "Show in folder"
/// (`SHOpenFolderAndSelectItems`) opens a window that stays a window, with its
/// selection. Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn folding_off_show_in_folder_stays_a_separate_window_with_its_selection() {
    with_harness("off_shopen", folding_off(), |h| folding_off_case(h, OpenMethod::ShOpenFolderAndSelectItems));
}

// --- The Activity_Log (Req 14.30) --------------------------------------------

/// One expected `FOLD` entry: the folder, the outcome and the `matching`
/// field of a `switched` entry.
type ExpectedFold = (PathBuf, &'static str, Option<&'static str>);

/// The sequence of the section comment, then the check of the log.
fn fold_log_case(h: &Harness) -> Result<(), String> {
    let mut expected: Vec<ExpectedFold> = Vec::new();

    // 1 and 2: two folds (one with a selection).
    let a = fold_via(h, OpenMethod::ExplorerExe, "log_a")?;
    a.print();
    a.check(false)?;
    expected.push((a.dir.clone(), "folded-primary", None));
    let b = fold_via(h, OpenMethod::ExplorerSelect, "log_b")?;
    b.print();
    b.check(false)?;
    expected.push((b.dir.clone(), "folded-primary", None));

    // 3: back to the Primary_Window's own tab, which is not the active one (B's is).
    let first = h.primary_dir.clone();
    let run = switch_via(h, OpenMethod::ExplorerExe, &first, None)?;
    run.print();
    run.check()?;
    expected.push((first, "switched", Some("primary")));

    // 4: A's tab, with its file selected (A's tab is not the active one now).
    let a_file = a.dir.join("target.txt"); // written by `fold_via`
    let run = switch_via(h, OpenMethod::ExplorerSelect, &a.dir, Some(&a_file))?;
    run.print();
    run.check()?;
    expected.push((a.dir.clone(), "switched", Some("primary")));

    // 5: Ctrl+Shift held: excluded at its first signal, a separate window, no entry.
    let bypass_dir = h.dir("log_bypass");
    let bypass_want = normalize(&bypass_dir.to_string_lossy());
    let keys = CtrlShiftDown::press();
    settle(Duration::from_millis(30));
    let mut launch = h.open(OpenMethod::ExplorerExe, &bypass_dir, None)?;
    let seen = h.wait_for_tab(&bypass_dir, OPEN_TIMEOUT); // keys stay down until the window exists
    settle(HOLD_AFTER_SEEN);
    drop(keys);
    settle(SEPARATE_WATCH);
    h.adopt_all();
    let _ = launch.call_result();
    let (bypass_win, _, _) = seen.ok_or_else(|| format!("Ctrl+Shift + explorer.exe did not open {bypass_want} within {OPEN_TIMEOUT:?}"))?;
    if bypass_win == h.primary {
        return Err("Ctrl+Shift + explorer.exe opened a tab in the Primary_Window (14.3 g)".into());
    }
    if h.other_windows() != [bypass_win] || locations_of(bypass_win) != [bypass_want.clone()] {
        return Err(format!(
            "Ctrl+Shift + explorer.exe: windows other than the Primary_Window are {:?}, want only {:?} showing {bypass_want} (14.3 g)",
            windows_with_locations(),
            hwnd_of(bypass_win)
        ));
    }

    // The log: one entry per candidate, in order, with its fields; none for step 5.
    let entries = wait(Duration::from_secs(3), || {
        let all = h.fold_entries();
        (all.len() >= expected.len()).then_some(all)
    })
    .unwrap_or_else(|| h.fold_entries());
    let got: Vec<(String, Option<String>, Option<String>)> = entries
        .iter()
        .map(|p| (p.get_str("location").map(|l| normalize(&l)).unwrap_or_default(), p.get_str("outcome"), p.get_str("matching")))
        .collect();
    let want: Vec<(String, Option<String>, Option<String>)> = expected
        .iter()
        .map(|(dir, outcome, matching)| {
            (normalize(&dir.to_string_lossy()), Some((*outcome).to_string()), matching.map(str::to_string))
        })
        .collect();
    println!("FOLD entries:");
    for p in &entries {
        println!("  {} FOLD {:?}", p.event, p.fields.iter().map(|(k, v)| format!("{k}={}", v.to_string_lossy())).collect::<Vec<_>>());
    }
    if got != want {
        return Err(format!(
            "the FOLD entries are {got:?}, want exactly one per candidate and none for the Ctrl+Shift window, {want:?} (14.30)"
        ));
    }
    for (p, (dir, outcome, _)) in entries.iter().zip(&expected) {
        let names = field_names(p);
        let want_names: &[&str] = if *outcome == "switched" {
            &["location", "outcome", "matching"]
        } else {
            &["location", "outcome"]
        };
        if names != want_names {
            return Err(format!(
                "the FOLD entry for {} has fields {names:?}, want {want_names:?} for outcome {outcome} (14.30)",
                dir.display()
            ));
        }
    }
    if !fold_entries_for(h, &bypass_dir).is_empty() {
        return Err("the Ctrl+Shift window has a FOLD entry; an excluded window is not a Folding_Candidate (14.3 g, 14.30)".into());
    }
    Ok(())
}

/// Req 14.30: the Activity_Log holds exactly one `FOLD` entry per
/// Folding_Candidate (two `folded-primary`, two `switched matching=primary`),
/// each with its Location, outcome and the fields of that outcome, and none
/// for a window excluded with Ctrl+Shift. Needs the keyboard to itself.
/// Unverified on a machine.
#[test]
#[ignore = "opens File Explorer windows; run alone with --ignored --nocapture --test-threads=1"]
fn the_activity_log_has_one_fold_entry_per_candidate_with_its_outcome_and_fields() {
    with_harness("fold_log", Settings::default(), fold_log_case);
}
