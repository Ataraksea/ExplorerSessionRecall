//! Integration test: Location kinds round trip through real Explorer tabs
//! (task 16.1, Requirements 8.2, 8.3, 8.6).
//!
//! ```text
//! cargo test -p esr-win --test location_kinds -- --ignored --nocapture --test-threads=1
//! ```
//!
//! For every case the test:
//!
//! 1. appends a *source* tab to a test window and navigates it to the
//!    fixture, the way a user would get there (the raw parsing name or URL is
//!    bound with `SHParseDisplayName`, without the capture code);
//! 2. captures the source tab's Location with `location::capture_from_pidl`
//!    on its current folder PIDL (the tracker's path);
//! 3. writes a Session_File document with `esr_core::format::serialize`,
//!    parses it back with `parse`, and checks that the parsed Location is
//!    field-for-field identical to the captured one;
//! 4. appends a *restored* tab and navigates it to the parsed Location with
//!    `ShellNavigator::navigate` (fresh `search-ms:` URL for searches,
//!    Req 8.6);
//! 5. checks that the Location captured from the restored tab equals the
//!    saved Location (canonical key, Req 8.3) and has the same kind;
//! 6. closes both tabs.
//!
//! If Explorer opens a Location in a *new window* instead of navigating the
//! tab (observed for searches on 22621), the window is adopted by the guard
//! and closed at the end of the case. For the source, its tab is captured
//! instead and the case is reported as failed. For the restored tab, the
//! case fails, because Req 8.3 needs the tab itself to show the Location. Such
//! windows are recognised only by fixture names unique to this test run, in
//! their tabs' parsing or editing names. Other windows are never touched.
//!
//! Cases (fixtures live under the `ExplorerGuard` temp root):
//!
//! * **ZIP subfolder**: `Archive.zip\inner\deeper`, made with PowerShell
//!   `Compress-Archive`.
//! * **Live search**: a `search-ms:` URL in the form Explorer itself produces
//!   when a query is typed into the search box (`displayname`, query crumb,
//!   `location` crumb), scoped to a folder with a non-ASCII name. Typing into
//!   the search box is not automated; the tab state is the same. The URL is
//!   bound like every `search-ms:` name (native search folder, see
//!   `shell_util::parse_bind_ctx`), so the tab shows the search itself; the
//!   capture reads the URL from the tab PIDL's editing name.
//! * **Saved search**: a `.search-ms` file (persistedQuery XML) opened in the
//!   tab; captured as `Shell(SavedSearch)` with the file path.
//! * **Portable device**: the first WPD device under This PC; skipped when
//!   none is attached.
//! * **Mapped drive (subst)**: a free drive letter mapped with `subst` to a
//!   temp folder and removed again afterwards (also on failure). `subst` is a
//!   *local* mapping: `GetDriveTypeW` reports `DRIVE_FIXED`, so this case
//!   checks that the drive letter path is kept (Req 8.7) but not
//!   `Volume::Mapped`, which needs a real network drive (not created here).
//!   Skipped when `subst` is unavailable or no drive letter is free.
//! * **UNC**: the administrative share `\\localhost\<X>$\…` of the temp root;
//!   skipped when it is not accessible without credentials. No share is
//!   created.
//! * **Long path**: a folder path above 300 UTF-16 units (created through the
//!   `\\?\` form).
//! * **Unicode path**: a folder name with NFD and NFC accents, stacked
//!   combining marks, CJK, an astral emoji and a ZWJ emoji sequence.
//!
//! Window safety: one window is opened through the navigator on a temp
//! folder and adopted by `ExplorerGuard`, which closes it (all tabs) on
//! success, failure or panic, and serialises against other Explorer tests
//! through its named mutex. No other window is touched. Run this test alone.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::explorer_guard::{ExplorerGuard, pump_once, top_level_cabinets};
use esr_core::engine::{ObservedLocation, RequestId, TabKey, WinKey};
use esr_core::format::{FORMAT_VERSION, parse, serialize};
use esr_core::model::{
    Display, DisplayRef, LocationId, LocationKind, Placement, Rect, SaveCause, SessionDoc,
    ShellKind, ShowState, Snapshot, Status, TabRec, Timestamp, Volume, WideString, WindowId,
    WindowRec,
};
use esr_core::plan::placement::FinalPlacement;
use esr_win::display_info::displays;
use esr_win::keys::{TabKeys, WindowKeys, hwnd_of, hwnd_value};
use esr_win::location::{
    capture_from_pidl, classify_kind, drive_type, is_file, pidl_is_well_formed,
    try_capture_from_parsing_name, try_capture_from_pidl,
};
use esr_win::navigator::{NavigatorSignal, ShellNavigator};
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use esr_win::strategy::Strategies;
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_ANY, RegGetValueW};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, IWebBrowser2, SHCreateItemFromParsingName,
    SHGetNameFromIDList, SIGDN, SIGDN_DESKTOPABSOLUTEEDITING, SIGDN_DESKTOPABSOLUTEPARSING,
};
use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, SetWindowPlacement};
use windows::core::{HSTRING, w};

const STEP: Duration = Duration::from_secs(10);
/// Navigation of a source or restored tab (UNC and search can be slow).
const NAV_TIMEOUT: Duration = Duration::from_secs(20);
/// A capture counts as settled once it is unchanged for this long.
const SETTLE: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn wide(s: &str) -> WideString {
    WideString(s.encode_utf16().collect())
}

fn wide_path(p: &Path) -> WideString {
    use std::os::windows::ffi::OsStrExt;
    WideString(p.as_os_str().encode_wide().collect())
}

/// Pumps messages until `f` yields or `timeout` passes.
fn wait<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
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

/// `(tab HWND, browser)` of each ShellWindows tab of `top`.
fn tabs_of(top: HWND) -> Vec<(HWND, IWebBrowser2)> {
    enumerate_all_explorer_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.top_hwnd == top)
        .flat_map(|g| g.tabs)
        .filter_map(|t| t.tab_hwnd.map(|h| (h, t.browser)))
        .collect()
}

fn browser_of(top: HWND, tab: TabKey) -> Option<IWebBrowser2> {
    tabs_of(top)
        .into_iter()
        .find(|(h, _)| hwnd_value(*h) == tab.hwnd)
        .map(|(_, b)| b)
}

/// What the tracker would record for `tab` now: `capture_from_pidl` on the
/// current folder PIDL. `Err` carries a diagnostic.
fn observe(top: HWND, tab: TabKey) -> Result<LocationId, String> {
    let browser = browser_of(top, tab).ok_or("tab not in ShellWindows")?;
    let pidl = current_folder_pidl(&browser).map_err(|e| format!("current folder: {e}"))?;
    match capture_from_pidl(&pidl) {
        ObservedLocation::Known(l) => Ok(l),
        _ => Err(format!(
            "unidentifiable: {}",
            try_capture_from_pidl(&pidl)
                .err()
                .map_or_else(String::new, |e| e.to_string())
        )),
    }
}

/// Waits until `tab` shows a Location accepted by `accept` that stays the
/// same (parsing name and kind) for [`SETTLE`].
fn wait_settled(
    top: HWND,
    tab: TabKey,
    timeout: Duration,
    accept: impl Fn(&LocationId) -> bool,
) -> Result<LocationId, String> {
    let end = Instant::now() + timeout;
    let mut last: Result<LocationId, String> = Err("nothing observed".into());
    let mut since: Option<Instant> = None;
    loop {
        pump_once();
        let now = observe(top, tab);
        match (&now, &last) {
            (Ok(n), Ok(l)) if accept(n) && n.parsing_name == l.parsing_name && n.kind == l.kind => {
                if since.is_some_and(|s| s.elapsed() >= SETTLE) {
                    return now;
                }
            }
            (Ok(n), _) if accept(n) => since = Some(Instant::now()),
            _ => since = None,
        }
        last = now;
        if Instant::now() >= end {
            return Err(format!("timed out; last observation {last:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A navigation target that only carries a raw parsing name or URL: the
/// navigator binds it with `SHParseDisplayName` as is (no search rebuild, no
/// capture code involved).
fn raw_target(name: &WideString) -> LocationId {
    LocationId {
        case_sensitive: false,
        kind: LocationKind::Shell(ShellKind::Other),
        parsing_name: name.clone(),
        display_name: WideString::default(),
        pidl: Vec::new(),
        search: None,
    }
}

/// Percent-encodes UTF-8 except the RFC 3986 unreserved set (as Explorer's
/// `search-ms:` URLs do for the scope).
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn session_doc(locations: &[LocationId], display: &Display, restored: Rect) -> SessionDoc {
    SessionDoc {
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
            offset_minutes: 60,
        },
        cause: SaveCause::LastWindowClosed,
        snapshot: Snapshot {
            windows: vec![WindowRec {
                id: WindowId(1),
                tabs: locations
                    .iter()
                    .map(|l| TabRec {
                        location: l.clone(),
                    })
                    .collect(),
                active: 0,
                placement: Placement {
                    state: ShowState::Normal,
                    restored,
                    display: DisplayRef {
                        device: display.device.clone(),
                        monitor: display.monitor,
                    },
                },
            }],
            activation: vec![WindowId(1)],
        },
    }
}

/// Serializes and parses `locations` as one window; returns the parsed tab
/// Locations after checking they are identical to the input.
fn file_round_trip(
    locations: &[LocationId],
    display: &Display,
    restored: Rect,
) -> Result<Vec<LocationId>, String> {
    let doc = session_doc(locations, display, restored);
    let text = serialize(&doc);
    let parsed = parse(text.as_bytes()).map_err(|e| format!("parse: {e:?}\n{text}"))?;
    let tabs: Vec<LocationId> = parsed
        .snapshot
        .windows
        .first()
        .ok_or("parsed document has no window")?
        .tabs
        .iter()
        .map(|t| t.location.clone())
        .collect();
    if tabs.len() != locations.len() {
        return Err(format!(
            "{} tabs parsed, {} written",
            tabs.len(),
            locations.len()
        ));
    }
    for (a, b) in locations.iter().zip(&tabs) {
        if !a.identical(b) {
            return Err(format!(
                "parsed Location differs:\n  wrote {a:?}\n  read  {b:?}"
            ));
        }
    }
    Ok(tabs)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Case {
    /// Raw parsing name or URL the source tab is navigated to.
    target: WideString,
    /// Kind the capture must report, if known in advance.
    expect_kind: Option<LocationKind>,
    /// Whether the captured parsing name must equal `target` unit for unit.
    expect_name: bool,
}

enum Prep {
    Ready(Case),
    Skip(String),
    /// The fixture could not be created.
    Broken(String),
}

fn zip_case(guard: &ExplorerGuard) -> Prep {
    let src = guard.dir("zipsrc").join("inner");
    let deeper = src.join("deeper");
    let archive = guard.root().join("Archive.zip");
    let made = std::fs::create_dir_all(&deeper)
        .and_then(|()| std::fs::write(src.join("top.txt"), b"esr"))
        .and_then(|()| std::fs::write(deeper.join("leaf.txt"), b"esr"));
    if let Err(e) = made {
        return Prep::Broken(format!("zip source: {e}"));
    }
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Compress-Archive -LiteralPath $env:ESR_ZIP_SRC -DestinationPath $env:ESR_ZIP_DST",
        ])
        .env("ESR_ZIP_SRC", &src)
        .env("ESR_ZIP_DST", &archive)
        .output();
    match out {
        Ok(o) if o.status.success() && archive.is_file() => {}
        Ok(o) => {
            return Prep::Broken(format!(
                "Compress-Archive failed: {}",
                String::from_utf8_lossy(&o.stderr)
            ));
        }
        Err(e) => return Prep::Broken(format!("powershell.exe: {e}")),
    }
    Prep::Ready(Case {
        target: wide_path(&archive.join("inner").join("deeper")),
        expect_kind: Some(LocationKind::ZipFolder),
        expect_name: true,
    })
}

fn live_search_case(guard: &ExplorerGuard) -> Prep {
    let leaf = "live scope Jos\u{e9} \u{65e5}\u{672c}";
    let scope = guard.dir(leaf);
    if let Err(e) = std::fs::write(scope.join("needle report.txt"), b"esr") {
        return Prep::Broken(format!("search scope: {e}"));
    }
    let scope_s = scope.to_string_lossy();
    // The form Explorer produces for a query typed into the search box.
    let url = format!(
        "search-ms:displayname=Search%20Results%20in%20{}&crumb=System.Generic.String%3A{}&crumb=location:{}",
        pct(leaf),
        pct("needle caf\u{e9}"),
        pct(&scope_s),
    );
    Prep::Ready(Case {
        target: wide(&url),
        expect_kind: Some(LocationKind::Search),
        expect_name: false,
    })
}

fn saved_search_case(guard: &ExplorerGuard) -> Prep {
    let scope = guard.dir("saved scope");
    let file = guard.root().join("saved-needle.search-ms");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<persistedQuery version="1.0">
  <viewInfo viewMode="details" iconSize="16" stackIconSize="0" displayName="saved needle" autoListFlags="0"/>
  <query>
    <conditions>
      <condition type="leafCondition" property="System.Generic.String" operator="wordmatch" propertyType="string" value="needle" localeName="en-US"/>
    </conditions>
    <kindList>
      <kind name="item"/>
    </kindList>
    <scope>
      <include path="{}" nonRecursive="false"/>
    </scope>
  </query>
</persistedQuery>
"#,
        scope.display()
    );
    let made = std::fs::write(scope.join("needle.txt"), b"esr")
        .and_then(|()| std::fs::write(&file, xml.as_bytes()));
    if let Err(e) = made {
        return Prep::Broken(format!("saved search: {e}"));
    }
    Prep::Ready(Case {
        target: wide_path(&file),
        // A saved search is never redirected to its folder (Req 9.2).
        expect_kind: Some(LocationKind::Shell(ShellKind::SavedSearch)),
        expect_name: true,
    })
}

/// Parsing name of the first portable (WPD) device under This PC.
fn find_portable_device() -> Result<Option<WideString>, String> {
    // SAFETY: plain Shell COM calls on this STA; returned strings are freed once.
    unsafe {
        let pc: IShellItem = SHCreateItemFromParsingName(
            w!("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
            None::<&IBindCtx>,
        )
        .map_err(|e| format!("This PC: {e}"))?;
        let items: IEnumShellItems = pc
            .BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)
            .map_err(|e| format!("enumerate This PC: {e}"))?;
        loop {
            let mut one: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0u32;
            let _ = items.Next(&mut one, Some(&mut fetched));
            if fetched == 0 {
                return Ok(None);
            }
            let Some(item) = one[0].take() else {
                return Ok(None);
            };
            let Ok(name) = item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) else {
                continue;
            };
            let units = name.as_wide().to_vec();
            CoTaskMemFree(Some(name.0.cast_const().cast()));
            if classify_kind(&units, drive_type, is_file)
                == LocationKind::Shell(ShellKind::PortableDevice)
            {
                return Ok(Some(WideString(units)));
            }
        }
    }
}

fn portable_case() -> Prep {
    match find_portable_device() {
        Ok(Some(device)) => Prep::Ready(Case {
            target: device,
            expect_kind: Some(LocationKind::Shell(ShellKind::PortableDevice)),
            expect_name: false,
        }),
        Ok(None) => Prep::Skip("no portable (WPD) device is attached".into()),
        Err(e) => Prep::Skip(format!("could not enumerate This PC: {e}")),
    }
}

/// A `subst` drive letter, removed on drop (also on panic).
struct SubstDrive {
    letter: char,
    exe: PathBuf,
}

impl SubstDrive {
    fn system32() -> PathBuf {
        std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
            .join("System32")
    }

    /// A letter that is not a drive and not a remembered network mapping.
    fn free_letter() -> Option<char> {
        // SAFETY: no arguments.
        let used = unsafe { GetLogicalDrives() };
        ('M'..='Y').rev().find(|&c| {
            let bit = 1u32 << (c as u32 - 'A' as u32);
            let mut size = 0u32;
            // SAFETY: size query only; no data buffer.
            let remembered = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    &HSTRING::from(format!(r"Network\{c}")),
                    &HSTRING::from("RemotePath"),
                    RRF_RT_ANY,
                    None,
                    None,
                    Some(&mut size),
                )
            } == ERROR_SUCCESS;
            used & bit == 0 && !remembered && !Path::new(&format!("{c}:\\")).exists()
        })
    }

    /// `Err` is a skip reason.
    fn create(target: &Path) -> Result<Self, String> {
        let exe = Self::system32().join("subst.exe");
        if !exe.is_file() {
            return Err(format!("{} is not available", exe.display()));
        }
        let letter = Self::free_letter().ok_or("no free drive letter in M..Y")?;
        let out = Command::new(&exe)
            .arg(format!("{letter}:"))
            .arg(target)
            .output()
            .map_err(|e| format!("running subst: {e}"))?;
        let drive = Self { letter, exe };
        if !out.status.success() || !Path::new(&format!("{letter}:\\")).exists() {
            return Err(format!(
                "subst {letter}: failed: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        println!("[fixture] subst {letter}: -> {}", target.display());
        Ok(drive)
    }
}

impl Drop for SubstDrive {
    fn drop(&mut self) {
        let letter = self.letter;
        let r = Command::new(&self.exe)
            .arg(format!("{letter}:"))
            .arg("/D")
            .output();
        let gone = !Path::new(&format!("{letter}:\\")).exists();
        match r {
            Ok(o) if o.status.success() && gone => println!("[fixture] subst {letter}: removed"),
            other => eprintln!(
                "[fixture] REMOVE MANUALLY with `subst {letter}: /D` (removal result {other:?}, gone {gone})"
            ),
        }
    }
}

/// Name of the folder used on the subst drive (unique per test process).
fn subst_folder() -> String {
    format!("esr-mapped-{}", std::process::id())
}

fn subst_case(guard: &ExplorerGuard, holder: &mut Option<SubstDrive>) -> Prep {
    let target = guard.dir("substroot");
    if let Err(e) = std::fs::create_dir_all(target.join(subst_folder())) {
        return Prep::Broken(format!("subst target: {e}"));
    }
    match SubstDrive::create(&target) {
        Ok(d) => {
            let path = format!("{}:\\{}", d.letter, subst_folder());
            *holder = Some(d);
            Prep::Ready(Case {
                target: wide(&path),
                // subst is a local mapping: GetDriveTypeW reports DRIVE_FIXED.
                expect_kind: Some(LocationKind::FileSystem(Volume::Fixed)),
                expect_name: true,
            })
        }
        Err(reason) => Prep::Skip(reason),
    }
}

fn unc_case(guard: &ExplorerGuard) -> Prep {
    let dir = guard.dir("unc folder");
    let s = dir.to_string_lossy().into_owned();
    let b = s.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_alphabetic() || &b[1..3] != b":\\" {
        return Prep::Skip(format!("temp root {s} is not on a drive letter"));
    }
    let unc = format!(
        r"\\localhost\{}$\{}",
        (b[0] as char).to_ascii_uppercase(),
        &s[3..]
    );
    // The check runs on a helper thread so an unresponsive server cannot hang
    // the test.
    let (tx, rx) = mpsc::channel();
    let probe = unc.clone();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read_dir(&probe).map(|_| ()));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Prep::Ready(Case {
            target: wide(&unc),
            expect_kind: Some(LocationKind::FileSystem(Volume::Unc)),
            expect_name: true,
        }),
        Ok(Err(e)) => Prep::Skip(format!(
            "{unc} is not accessible without credentials ({e}); no share is created"
        )),
        Err(_) => Prep::Skip(format!("{unc} did not respond within 5 s")),
    }
}

fn long_path_case(guard: &ExplorerGuard) -> Prep {
    let mut dir = guard.dir("long");
    while wide_path(&dir).len() <= 300 {
        dir.push("esr-long-folder-name-0123456789");
    }
    let verbatim = PathBuf::from(format!(r"\\?\{}", dir.display()));
    if let Err(e) = std::fs::create_dir_all(&verbatim) {
        return Prep::Broken(format!("long path: {e}"));
    }
    println!(
        "[fixture] long path: {} UTF-16 units",
        wide_path(&dir).len()
    );
    Prep::Ready(Case {
        target: wide_path(&dir),
        expect_kind: Some(LocationKind::FileSystem(Volume::Fixed)),
        expect_name: true,
    })
}

fn unicode_case(guard: &ExplorerGuard) -> Prep {
    // NFD é, NFC é, a + ring above + dot below, CJK, 😀, 👩‍💻 (ZWJ), mixed case.
    let name = "Uni Jose\u{301} Jos\u{e9} a\u{30A}\u{323} \u{65e5}\u{672c} \u{1F600} \u{1F469}\u{200D}\u{1F4BB} MiXeD";
    let dir = guard.root().join(name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Prep::Broken(format!("unicode folder: {e}"));
    }
    Prep::Ready(Case {
        target: wide_path(&dir),
        expect_kind: Some(LocationKind::FileSystem(Volume::Fixed)),
        expect_name: true,
    })
}

// ---------------------------------------------------------------------------
// Window and tabs
// ---------------------------------------------------------------------------

/// What a navigation led to.
enum Seen {
    /// The tab itself shows an accepted Location.
    InTab(LocationId),
    /// Explorer opened the Location in a new window instead (adopted).
    NewWindow(HWND, TabKey),
}

struct Ctx<'g> {
    guard: &'g ExplorerGuard,
    nav: ShellNavigator,
    tab_keys: TabKeys,
    signals: Rc<RefCell<Vec<NavigatorSignal>>>,
    win: Option<WinKey>,
    display: Option<Display>,
    restored: Rect,
    /// Tabs added by the current case.
    opened: Vec<TabKey>,
    /// Lower-case strings that only this test's fixture Locations contain
    /// (the temp root's leaf name, the subst folder name).
    markers: Vec<String>,
    /// Windows Explorer opened by itself for the current case (adopted).
    strays: Vec<HWND>,
}

impl Ctx<'_> {
    fn top(&self) -> HWND {
        hwnd_of(self.win.expect("window").hwnd)
    }

    /// Adopts new top-level Explorer windows (not pre-existing, not ours)
    /// that show one of this test's fixtures, judged by the parsing and
    /// editing names of their tabs (a search shows its scope only in the
    /// `search-ms:` editing name). Returns the first tab of a newly adopted
    /// one. Windows that do not show a fixture are never touched.
    fn adopt_strays(&mut self) -> Option<(HWND, TabKey)> {
        let ours = self.top();
        let groups = enumerate_all_explorer_windows().unwrap_or_default();
        let mut found = None;
        for raw in top_level_cabinets() {
            let h = HWND(raw as *mut _);
            if h == ours || self.guard.is_pre_existing(h) || self.strays.contains(&h) {
                continue;
            }
            let tabs: Vec<(HWND, Vec<String>)> = groups
                .iter()
                .filter(|g| g.top_hwnd == h)
                .flat_map(|g| g.tabs.iter())
                .filter_map(|t| {
                    let pidl = current_folder_pidl(&t.browser).ok()?;
                    let names = [SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_DESKTOPABSOLUTEEDITING]
                        .into_iter()
                        .filter_map(|s| shell_name(&pidl, s))
                        .collect();
                    Some((t.tab_hwnd?, names))
                })
                .collect();
            let mine = tabs.iter().any(|(_, names)| {
                names.iter().any(|n| {
                    let n = n.to_lowercase();
                    self.markers.iter().any(|m| n.contains(m.as_str()))
                })
            });
            if mine {
                self.guard.adopt(h);
                self.strays.push(h);
                println!("  Explorer opened a new window {h:?}: {tabs:?}");
                if found.is_none() {
                    found = Some((h, self.tab_keys.key_of(hwnd_value(tabs[0].0))));
                }
            }
        }
        found
    }

    /// Like [`wait_settled`], but also stops when Explorer opens the
    /// Location in a new window instead of the tab.
    fn wait_for(
        &mut self,
        top: HWND,
        tab: TabKey,
        timeout: Duration,
        accept: impl Fn(&LocationId) -> bool,
    ) -> Result<Seen, String> {
        let end = Instant::now() + timeout;
        let mut last: Result<LocationId, String> = Err("nothing observed".into());
        let mut since: Option<Instant> = None;
        let mut next_stray_check = Instant::now();
        loop {
            pump_once();
            let now = observe(top, tab);
            match (&now, &last) {
                (Ok(n), Ok(l))
                    if accept(n) && n.parsing_name == l.parsing_name && n.kind == l.kind =>
                {
                    if since.is_some_and(|s| s.elapsed() >= SETTLE) {
                        return now.map(Seen::InTab);
                    }
                }
                (Ok(n), _) if accept(n) => since = Some(Instant::now()),
                _ => since = None,
            }
            last = now;
            if Instant::now() >= next_stray_check {
                if let Some((h, t)) = self.adopt_strays() {
                    return Ok(Seen::NewWindow(h, t));
                }
                next_stray_check = Instant::now() + Duration::from_millis(500);
            }
            if Instant::now() >= end {
                return Err(format!("timed out; last observation {last:?}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Appends a tab and waits until it registers and shows its initial
    /// Location (returned, so a later navigation can be told apart from it).
    fn add_tab(&mut self) -> Result<(TabKey, LocationId), String> {
        let win = self.win.ok_or("no window")?;
        let top = self.top();
        let before: Vec<HWND> = tabs_of(top).into_iter().map(|(h, _)| h).collect();
        self.nav
            .create_tab(win)
            .map_err(|e| format!("create_tab: {e:?}"))?;
        let new = wait(STEP, || {
            tabs_of(top)
                .into_iter()
                .map(|(h, _)| h)
                .find(|h| !before.contains(h))
        })
        .ok_or_else(|| format!("no new tab registered; signals {:?}", self.signals.borrow()))?;
        let tab = self.tab_keys.key_of(hwnd_value(new));
        self.nav.tab_registered(win, tab);
        self.opened.push(tab);
        let initial = wait_settled(top, tab, STEP, |_| true)
            .map_err(|e| format!("new tab never showed its initial Location: {e}"))?;
        Ok((tab, initial))
    }

    /// Closes what the current case opened: its tabs (UIA close button,
    /// newest first) and any window Explorer opened by itself. Failures are
    /// only printed; the guard closes everything adopted at the end anyway.
    fn close_case_windows(&mut self) {
        pump_for_ms(500);
        let _ = self.adopt_strays();
        for h in std::mem::take(&mut self.strays) {
            if !self.guard.close(h) {
                println!("  warning: window {h:?} did not close");
            }
        }
        let top = self.top();
        while let Some(tab) = self.opened.pop() {
            let closed = self
                .nav
                .close_tab(tab)
                .map_err(|e| format!("close_tab: {e:?}"))
                .and_then(|()| {
                    wait(STEP, || {
                        (!tabs_of(top).iter().any(|(h, _)| hwnd_value(*h) == tab.hwnd))
                            .then_some(())
                    })
                    .ok_or_else(|| {
                        format!("tab did not close; signals {:?}", self.signals.borrow())
                    })
                });
            if let Err(e) = closed {
                println!("  warning: {e}");
            }
        }
    }

    /// Opens the test window on `base` (hidden, claimed, then revealed).
    fn open(&mut self, base: &Path) -> Result<(), String> {
        let display = displays()
            .into_iter()
            .find(|d| d.primary)
            .ok_or("no primary display")?;
        let w = display.work;
        self.restored = Rect {
            left: w.left + 80,
            top: w.top + 60,
            right: w.left + 80 + 1000.min(w.right - w.left - 160),
            bottom: w.top + 60 + 650.min(w.bottom - w.top - 120),
        };
        let placement = FinalPlacement {
            state: ShowState::Normal,
            restored: self.restored,
            monitor: display.monitor,
            work: display.work,
        };
        self.display = Some(display);
        let base_loc = try_capture_from_parsing_name(&wide_path(base))
            .map_err(|e| format!("capture base: {e}"))?;
        self.nav
            .open_window(RequestId(1), &base_loc, &placement)
            .map_err(|e| format!("open_window: {e:?}"))?;
        let win = wait(STEP, || self.nav.pending_assignment(RequestId(1))).ok_or_else(|| {
            format!(
                "no window was assigned; signals {:?}",
                self.signals.borrow()
            )
        })?;
        self.win = Some(win);
        let hwnd = hwnd_of(win.hwnd);
        let first = wait(STEP, || {
            let tabs = tabs_of(hwnd);
            (tabs.len() == 1).then(|| self.tab_keys.key_of(hwnd_value(tabs[0].0)))
        })
        .ok_or("the window never registered its tab")?;
        let initial = wait_settled(hwnd, first, STEP, |l| *l == base_loc)
            .map_err(|e| format!("the window never showed the base folder: {e}"))?;
        // Adopted only once it shows the test's folder (see navigator_integration).
        self.guard.adopt(hwnd);
        if self.nav.claim(win, &initial) != Some(RequestId(1)) {
            return Err("claim did not match the open request".into());
        }
        self.nav.tab_registered(win, first);
        self.nav.reveal(win).map_err(|e| format!("reveal: {e:?}"))?;
        Ok(())
    }

    /// Restore half only, for a search whose capture failed: navigates a new
    /// tab to the Location bound from the case target and reports where it
    /// showed up.
    fn restore_half(&mut self, case: &Case) -> String {
        let loc = match try_capture_from_parsing_name(&case.target) {
            Ok(l) => l,
            Err(e) => return format!("cannot bind the target: {e}"),
        };
        // Explorer hands a search to a window that already shows it, so close
        // the source's window first.
        for h in std::mem::take(&mut self.strays) {
            if !self.guard.close(h) {
                println!("  warning: window {h:?} did not close");
            }
        }
        pump_for_ms(500);
        let top = self.top();
        let r = self.add_tab().and_then(|(tab, _)| {
            self.nav
                .navigate(tab, &loc)
                .map_err(|e| format!("navigate: {e:?}"))?;
            self.wait_for(top, tab, NAV_TIMEOUT, |l| *l == loc)
        });
        match r {
            Ok(Seen::InTab(_)) => "the tab shows the search".into(),
            Ok(Seen::NewWindow(h, _)) => format!(
                "Explorer opened the search in a new window {h:?} instead of navigating the tab"
            ),
            Err(e) => e,
        }
    }

    /// Steps 1–6 of the module docs for one case. Returns the saved Location.
    fn round_trip(&mut self, case: &Case) -> Result<LocationId, String> {
        let top = self.top();
        let display = self.display.clone().ok_or("no display")?;

        // 1–2. Source tab, captured as the tracker would.
        let (source, initial) = self.add_tab()?;
        self.nav
            .navigate(source, &raw_target(&case.target))
            .map_err(|e| format!("navigate source: {e:?}"))?;
        let mut source_note = String::new();
        let saved = match self
            .wait_for(top, source, NAV_TIMEOUT, |l| *l != initial)
            .map_err(|e| format!("source tab never reached {:?}: {e}", case.target))?
        {
            Seen::InTab(l) => l,
            Seen::NewWindow(h, t) => {
                // Still a real Explorer tab showing the Location: capture it.
                source_note =
                    format!("; navigating the source tab opened a new window {h:?} instead");
                wait_settled(h, t, NAV_TIMEOUT, |_| true)
                    .map_err(|e| format!("new window {h:?}: {e}"))?
            }
        };
        println!("  saved:    {:?} {:?}", saved.kind, saved.parsing_name);
        if let Some(spec) = &saved.search {
            println!(
                "            query {:?}, scope {:?} {:?}",
                spec.query, spec.scope.kind, spec.scope.parsing_name
            );
        }
        if let Some(k) = case.expect_kind
            && saved.kind != k
        {
            let mut msg = format!(
                "captured kind {:?} with parsing name {:?}, expected {k:?}{source_note}",
                saved.kind, saved.parsing_name
            );
            if k == LocationKind::Search {
                msg += &format!(
                    "; SIGDN_DESKTOPABSOLUTEEDITING of the tab's PIDL is {:?}; restore half: {}",
                    shell_name(&saved.pidl, SIGDN_DESKTOPABSOLUTEEDITING),
                    self.restore_half(case)
                );
            }
            return Err(msg);
        }
        // Above MAX_PATH, newer builds (26300) report the path with a `\\?\`
        // prefix, which 22621 did not. It is stored as reported (location.rs),
        // and the round trip below still has to give the identical Location.
        let verbatim_target: Vec<u16> = r"\\?\"
            .encode_utf16()
            .chain(case.target.0.iter().copied())
            .collect();
        let name_ok = saved.parsing_name == case.target
            || (case.target.0.len() > 259 && saved.parsing_name.0 == verbatim_target);
        if name_ok && saved.parsing_name != case.target {
            println!("  note: the Shell reported the path with a \\\\?\\ prefix");
        }
        if case.expect_name && !name_ok {
            return Err(format!(
                "captured parsing name {:?}, expected {:?}{source_note}",
                saved.parsing_name, case.target
            ));
        }
        if saved.kind == LocationKind::Search && saved.search.is_none() {
            return Err(format!(
                "search Location without query and scope{source_note}"
            ));
        }

        // 3. Session_File round trip.
        let parsed =
            file_round_trip(std::slice::from_ref(&saved), &display, self.restored)?.remove(0);

        // 4–5. Restored tab.
        let (restored_tab, _) = self.add_tab()?;
        self.nav
            .navigate(restored_tab, &parsed)
            .map_err(|e| format!("navigate restored tab: {e:?}"))?;
        let restored = match self
            .wait_for(top, restored_tab, NAV_TIMEOUT, |l| *l == parsed)
            .map_err(|e| format!("restored tab never showed the saved Location: {e}"))?
        {
            Seen::InTab(l) => l,
            Seen::NewWindow(h, t) => {
                return Err(format!(
                    "navigating the restored tab opened a new window {h:?} (showing {:?}) \
                     instead of the tab{source_note}",
                    observe(h, t).map(|l| (l.kind, l.parsing_name))
                ));
            }
        };
        println!(
            "  restored: {:?} {:?}",
            restored.kind, restored.parsing_name
        );
        if restored.kind != saved.kind {
            return Err(format!(
                "restored kind {:?} differs from saved {:?}",
                restored.kind, saved.kind
            ));
        }
        if saved.kind != LocationKind::Search && restored.parsing_name != saved.parsing_name {
            return Err("restored parsing name differs".into());
        }
        if !source_note.is_empty() {
            return Err(format!("round trip worked, but{source_note}"));
        }

        // 6. The caller closes both tabs (also on failure).
        Ok(saved)
    }
}

fn pump_for_ms(ms: u64) {
    common::explorer_guard::pump_for(Duration::from_millis(ms));
}

/// `SHGetNameFromIDList` on a terminated PIDL.
fn shell_name(pidl: &[u8], sigdn: SIGDN) -> Option<String> {
    if !pidl_is_well_formed(pidl) {
        return None;
    }
    // SAFETY: a well-formed, terminated ITEMIDLIST.
    let name = unsafe { SHGetNameFromIDList(pidl.as_ptr().cast::<ITEMIDLIST>(), sigdn) }.ok()?;
    // SAFETY: a NUL-terminated string from the COM allocator, freed once.
    let s = unsafe { name.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(name.0.cast_const().cast())) };
    s
}

enum Outcome {
    Ran(String),
    Skipped(String),
    Failed(String),
}

#[test]
#[ignore = "requires an interactive desktop; opens and closes its own Explorer window"]
fn location_kinds_round_trip_through_explorer_tabs() {
    let _sta = StaGuard::init().expect("STA init");
    // Declared before the guard so it is dropped after it (windows first,
    // then the drive mapping), also on panic.
    let mut subst: Option<SubstDrive> = None;
    let guard = ExplorerGuard::new("loc");

    let signals: Rc<RefCell<Vec<NavigatorSignal>>> = Rc::default();
    let sink = signals.clone();
    let tab_keys = TabKeys::new();
    let mut nav = ShellNavigator::new(
        Strategies::DEFAULT,
        WindowKeys::new(),
        tab_keys.clone(),
        Rc::new(move |s: NavigatorSignal| {
            println!("  signal {s:?}");
            sink.borrow_mut().push(s);
        }),
    );
    nav.start().expect("start navigator");
    let root_leaf = guard
        .root()
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .expect("temp root leaf");
    let mut ctx = Ctx {
        guard: &guard,
        nav,
        tab_keys,
        signals,
        win: None,
        display: None,
        restored: Rect::default(),
        opened: Vec::new(),
        markers: vec![root_leaf, subst_folder().to_lowercase()],
        strays: Vec::new(),
    };

    let mut results: Vec<(String, Outcome)> = Vec::new();
    let mut saved_all: Vec<LocationId> = Vec::new();
    let base = guard.dir("base");
    match ctx.open(&base) {
        Err(e) => results.push(("open test window".into(), Outcome::Failed(e))),
        Ok(()) => {
            let preps: Vec<(&str, Prep)> = vec![
                ("ZIP subfolder", zip_case(&guard)),
                ("live search", live_search_case(&guard)),
                ("saved search", saved_search_case(&guard)),
                ("portable device", portable_case()),
                ("mapped drive (subst)", subst_case(&guard, &mut subst)),
                ("UNC", unc_case(&guard)),
                ("long path", long_path_case(&guard)),
                ("Unicode path", unicode_case(&guard)),
            ];
            for (label, prep) in preps {
                println!("== {label}");
                let outcome = match prep {
                    Prep::Skip(r) => Outcome::Skipped(r),
                    Prep::Broken(r) => Outcome::Failed(format!("fixture: {r}")),
                    Prep::Ready(case) => {
                        let r = ctx.round_trip(&case);
                        ctx.close_case_windows();
                        match r {
                            Ok(saved) => {
                                let s = format!("{:?} {:?}", saved.kind, saved.parsing_name);
                                saved_all.push(saved);
                                Outcome::Ran(s)
                            }
                            Err(e) => Outcome::Failed(e),
                        }
                    }
                };
                results.push((label.to_string(), outcome));
            }
            // All captured Locations together in one Session_File window.
            if let Some(display) = ctx.display.clone()
                && !saved_all.is_empty()
            {
                let o = match file_round_trip(&saved_all, &display, ctx.restored) {
                    Ok(_) => Outcome::Ran(format!("{} tabs in one window", saved_all.len())),
                    Err(e) => Outcome::Failed(e),
                };
                results.push(("combined Session_File".into(), o));
            }
        }
    }

    // Give the window back the placement Explorer chose, so Explorer does not
    // remember the test's size, then stop the navigator.
    if let Some(win) = ctx.win
        && let Some(mut wp) = ctx.nav.original_placement(win)
    {
        wp.showCmd = SW_SHOWNOACTIVATE.0 as u32;
        // SAFETY: a WINDOWPLACEMENT read from this window.
        let _ = unsafe { SetWindowPlacement(hwnd_of(win.hwnd), &wp) };
    }
    drop(ctx);
    let cleanup = guard.finish();
    drop(subst);

    println!("\n== Summary");
    let mut failed = false;
    for (label, outcome) in &results {
        match outcome {
            Outcome::Ran(s) => println!("  RAN      {label}: {s}"),
            Outcome::Skipped(r) => println!("  SKIPPED  {label}: {r}"),
            Outcome::Failed(e) => {
                failed = true;
                println!("  FAILED   {label}: {e}");
            }
        }
    }
    cleanup.expect("test windows left open");
    assert!(!failed, "some Location kinds did not round trip");
}
