//! `ShellWindowsSource`: `DShellWindowsEvents` and per-tab `DWebBrowserEvents2`
//! sinks, shell loss and reconnection.
//!
//! # Tabs
//!
//! Every Explorer tab is one entry of the shell's `IShellWindows` collection.
//! The source advises a `DShellWindowsEvents` sink on the collection.
//! `WindowRegistered` / `WindowRevoked` only carry a cookie that cannot be
//! mapped back to a browser, so each of them triggers a *sync*: the
//! collection is enumerated and diffed against the known tabs by COM
//! identity (the browser proxy's `IUnknown`; each known tab keeps its
//! browser, so the identity stays stable). Unknown entries cost one
//! cross-process call each (`Item`); only new entries are queried further.
//! When the collection only grew (its last known entry is still in place
//! and every later entry is unknown), only the new entries are read.
//!
//! * A **new entry** of a top-level `CabinetWClass` window gets a
//!   `DWebBrowserEvents2` sink and a [`TabKey`] (from the shared [`TabKeys`]
//!   registry; the window's [`WinKey`] comes from the shared [`WindowKeys`]).
//!   If its window was not seen before, it is a **new window** (see below).
//!   Otherwise the tab was added to an existing window: the handler's
//!   [`tab_registered`](ShellSourceHandler::tab_registered) is called and
//!   `TabCreated { win, tab }` is posted. Either way `window_changed(win)`
//!   follows. A dragged tab's `ShellTabWindowClass` HWND is recreated in the
//!   target window, so a drag shows up as a revocation plus a registration.
//! * A **missing entry** is revoked: its sink is unadvised, its tab key is
//!   retired, and `TabRevoked { win, tab }` is posted. Revocations are only
//!   derived from a complete enumeration. Any failed `Item` call makes the
//!   enumeration incomplete, including an index below `Count` that yields no
//!   entry, which happens for live tabs while another window closes. An
//!   incomplete sync is retried after [`RESYNC_RETRY_MS`] (and accepted as
//!   it is after [`MAX_INCOMPLETE_SYNCS`] attempts).
//! * Explorer sometimes replaces a live tab's automation object (seen while
//!   another window closes): the tab's entry gets a new identity with the
//!   same tab and top-level HWNDs. Such an entry is **rebound** to the
//!   existing record (new sink, same keys), and nothing is reported.
//!
//! The sinks run on whatever thread COM delivers them on and only set a flag
//! and post a message to the source's window, so all real work happens from
//! the owning thread's message loop and never inside an incoming call.
//!
//! # Locations (Req 2.2, 8.1)
//!
//! A Location is recorded only after navigation completed:
//! `NavigateComplete2` reads the tab's current folder
//! (`QueryActiveShellView â†’ IFolderView::GetFolder â†’ GetCurFolder`), captures
//! it with [`capture_from_pidl`], and posts `NavigationCompleted`. A tab whose
//! navigation never completed is [`ObservedLocation::NotNavigated`]. A tab
//! that already has a shell view when it registers (its first navigation
//! completed before the sink was attached) counts as navigated. A completed
//! navigation without a Location_Identifier is
//! [`ObservedLocation::Unidentifiable`] and is reported by
//! `window_changed` only (Req 8.8).
//!
//! # New windows: claim or trigger (Req 4.1, 6.4)
//!
//! For the first tab of a new window, the first completed navigation gives
//! the window's initial Location. The handler's
//! [`claim`](ShellSourceHandler::claim) is asked first (the runtime forwards
//! it to `ShellNavigator::claim`): a claimed window is reported as
//! `WindowOpened { request, win }`. An unclaimed window waits for
//! `DocumentComplete` (at most [`DOCUMENT_COMPLETE_WAIT_MS`]), then captures
//! the tab's selection with `IFolderView2::GetSelection` (absolute PIDLs) and
//! posts `InitialLocationKnown { win, location, selection }`. Windows whose
//! initial navigation yields no Location_Identifier are not announced.
//!
//! # Shell loss and reconnection (Req 1.7, 1.10, 2.9)
//!
//! The source is [`Connection::Attached`] while it holds the collection. It
//! is lost when a collection call fails with a disconnection error
//! (`RPC_E_DISCONNECTED`, see [`is_disconnect`]), when the shell process
//! (the owner of `GetShellWindow()`) exits, or when the runtime calls
//! [`shell_disconnected`](ShellWindowsSource::shell_disconnected). It then
//! drops every sink, posts `ShellLost`, and polls `GetShellWindow()` every
//! [`SHELL_POLL_INTERVAL_MS`] while lost. `TaskbarCreated` (received by the
//! source's hidden top-level window, or forwarded by the runtime through
//! [`on_taskbar_created`](ShellWindowsSource::on_taskbar_created)) and a
//! poll that finds a shell window both reconnect. A reconnection re-creates
//! the collection, re-attaches all sinks, rescans every window, posts
//! `ShellAvailable`, and calls `window_changed` for every open window. Tabs
//! that survived keep their keys. Nothing is revoked or announced for the
//! tabs that disappeared meanwhile; the Live_Session is left to the rescans.
//! If no shell is available at [`attach`](ShellWindowsSource::attach), the
//! source waits the same way and posts `ShellAvailable` once it connects.
//! The pure transition logic is [`Connection`].
//!
//! # Threading
//!
//! The source is bound to the thread that created it (`!Send`), normally the
//! Engine STA, which must pump messages. At most one source can be attached
//! per thread. No internal borrow is held across a COM call or a handler
//! call, so the handler may call back into the source.
//!
//! Requirements 1.7, 1.10, 2.2, 2.9, 6.4, 8.1.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use esr_core::engine::{Event, ItemId, ObservedLocation, RequestId, ShellError, TabKey, WinKey};
use esr_core::model::LocationId;
use crate::browser_worker::{OnFailure, SinkFactory};
use windows::Win32::Foundation::{
    CloseHandle, E_NOTIMPL, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Com::{
    CoTaskMemFree, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, IConnectionPoint,
    IConnectionPointContainer, IDispatch, IDispatch_Impl, ITypeInfo,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, RegisterWaitForSingleObject, UnregisterWaitEx,
    WT_EXECUTEONLYONCE,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VT_BYREF, VT_I4, VT_VARIANT,
};
use windows::Win32::UI::Shell::{
    DShellWindowsEvents, DShellWindowsEvents_Impl, DWebBrowserEvents2, DWebBrowserEvents2_Impl,
    IFolderView2, IShellWindows, IWebBrowser2, SHGetIDListFromObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetShellWindow,
    KillTimer, MSGFLT_ALLOW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetTimer,
    WM_APP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BSTR, GUID, HRESULT, IUnknown, Interface, PCWSTR, implement, w};

use crate::keys::{TabKeys, WindowKeys, hwnd_of, hwnd_value};
use crate::location::capture_from_pidl;
use crate::shell_util::{
    current_folder_pidl, is_cabinet_window, pidl_to_bytes, shell_browser, shell_windows,
    tab_hwnd, top_level_hwnd, window_process_id,
};

/// Poll period for `GetShellWindow()` while the shell is lost (design.md).
pub const SHELL_POLL_INTERVAL_MS: u32 = 1_000;
/// How long a new, unclaimed window waits for `DocumentComplete` before its
/// selection is captured anyway.
pub const DOCUMENT_COMPLETE_WAIT_MS: u32 = 500;
/// Delay before a sync that could not complete is retried.
pub const RESYNC_RETRY_MS: u32 = 100;
/// Syncs a new entry may fail to resolve (tab HWND, sink) before it is ignored.
pub const MAX_REGISTER_ATTEMPTS: u32 = 30;
/// Consecutive syncs with a failed or empty slot after which the enumeration
/// is taken as it is (so a slot that stays empty cannot block revocations).
pub const MAX_INCOMPLETE_SYNCS: u32 = 20;

// DISPIDs (exdispid.h).
pub const DISPID_WINDOWREGISTERED: i32 = 200;
pub const DISPID_WINDOWREVOKED: i32 = 201;
pub const DISPID_NAVIGATECOMPLETE2: i32 = 252;
pub const DISPID_ONQUIT: i32 = 253;
pub const DISPID_DOCUMENTCOMPLETE: i32 = 259;

/// The collection changed (posted once until the next sync).
const WM_SHELL_WINDOWS_CHANGED: u32 = WM_APP + 0x71;
/// A tab event: `wParam` = DISPID, `lParam` = tab record id.
const WM_TAB_EVENT: u32 = WM_APP + 0x72;
/// The shell process exited: `wParam` = PID.
const WM_SHELL_PROCESS_EXITED: u32 = WM_APP + 0x73;
/// A browser worker could not attach a tab's sink: `lParam` = record id.
const WM_REMOTE_ADVISE_FAILED: u32 = WM_APP + 0x74;

const TIMER_POLL: usize = 1;
const TIMER_RESYNC: usize = 2;
/// Document-wait timers use `TIMER_DOCUMENT_BASE + record id`.
const TIMER_DOCUMENT_BASE: usize = 0x1000;
/// Late-sink checks use `TIMER_LATE_SINK_BASE + record id` (above every
/// document-wait timer id in practice).
const TIMER_LATE_SINK_BASE: usize = 0x4000_0000;
/// A tab whose sink a browser worker attaches after its navigation
/// ([`crate::browser_worker::late_sink`]) may complete that navigation before
/// the sink is attached. If it still shows no completed navigation this long
/// after it registered, its Location is read once.
const LATE_SINK_CHECK_MS: u32 = 1_500;

const WINDOW_CLASS: PCWSTR = w!("ESR.ShellWindowsSource");

/// The registered `TaskbarCreated` message (0 until the first window is created).
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Receives the source's output on the source's thread.
///
/// The runtime maps this onto the `TabSource` port's `EventSink` and the
/// `ShellNavigator` (task 14.6).
pub trait ShellSourceHandler {
    /// Deliver an event to the Engine (`TabRevoked`, `TabCreated`,
    /// `NavigationCompleted`, `WindowOpened`, `InitialLocationKnown`,
    /// `ShellLost`, `ShellAvailable`).
    fn post(&self, event: Event);
    /// `win` changed (a tab registered, a navigation without a
    /// Location_Identifier, a reconnection rescan).
    fn window_changed(&self, win: WinKey);
    /// A tab registered in an already known window, right before
    /// `TabCreated` is posted (`ShellNavigator::tab_registered`).
    fn tab_registered(&self, _win: WinKey, _tab: TabKey) {}
    /// The browser of a tab just registered (`NavigatorHandle::tab_browser`).
    fn tab_browser(&self, _tab: TabKey, _browser: &IWebBrowser2) {}
    /// Attach the sink of a tab the navigator is creating on a browser
    /// worker instead of this thread (`NavigatorHandle::attach_browser`,
    /// task 16.3). `failed` runs on the worker if that fails. Returns false
    /// if the handler did not take it (the source then attaches it itself).
    fn attach_remote(
        &self,
        _win: WinKey,
        _tab: TabKey,
        _browser: &IWebBrowser2,
        _sink: SinkFactory,
        _failed: OnFailure,
    ) -> bool {
        false
    }
    /// The sink of `tab` attached through [`attach_remote`](Self::attach_remote)
    /// must go (`NavigatorHandle::detach_browser`).
    fn detach_remote(&self, _tab: TabKey) {}
    /// The initial Location of new window `win` is known: the open request
    /// that claims it, if any (`ShellNavigator::claim`).
    fn claim(&self, _win: WinKey, _initial: &LocationId) -> Option<RequestId> {
        None
    }
    /// A navigation of `tab` completed at the Location whose parsing name is
    /// `parsing_name` (the `URL` argument of `NavigateComplete2`): the
    /// Location the navigator navigated `tab` to, if it has that parsing name
    /// (`NavigatorHandle::expected_location`). The source then reports it
    /// without reading the tab's folder from Explorer, a cross-process call
    /// that waits while the tab's window thread is busy creating the next
    /// tab of a restoration.
    fn expected_location(&self, _tab: TabKey, _parsing_name: &[u16]) -> Option<LocationId> {
        None
    }
    /// Deliver what was posted so far now, if the receiver is not busy (the
    /// runtime drains its mailbox into the Engine). Called during a sync
    /// after each new tab is registered.
    fn flush(&self) {}
    /// Whether a tab requested for `win` is outstanding
    /// (`ShellNavigator::is_creating_tab`).
    fn creating_tab(&self, _win: WinKey) -> bool {
        false
    }
    /// Whether `tab` was navigated and its completion is not reported yet
    /// (`NavigatorHandle::navigation_expected`).
    fn navigation_expected(&self, _tab: TabKey) -> bool {
        false
    }
    /// Whether the Locations last reported for `win`'s tabs (by their
    /// `NavigateComplete2`) can be taken as current by
    /// [`ShellWindowsSource::tabs_of`] instead of re-reading each tab's
    /// folder: true while a restoration builds the window, whose every
    /// navigation the source observes (task 16.3; a re-read waits for each
    /// tab's busy window thread, ~12 ms per tab).
    fn cached_locations_current(&self, _win: WinKey) -> bool {
        false
    }
}

/// The tabs [`ShellWindowsSource::tabs_of`] found in one window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowTabs {
    pub pid: u32,
    /// `(tab, location)` in registration order (new tabs last). This is not
    /// the visual order; UIA gives that.
    pub tabs: Vec<(TabKey, ObservedLocation)>,
    /// Always `None`: ShellWindows cannot tell the active tab (UIA does).
    pub active: Option<TabKey>,
}

/// State of the connection to the shell's window collection, with the pure
/// transition logic of loss and reconnection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Connection {
    /// Not attached (before `attach`, after `detach`).
    #[default]
    Detached,
    /// The collection is held and its sink is attached.
    Attached,
    /// No usable shell; `GetShellWindow()` is polled. `reported` is true when
    /// the connection was lost (`ShellLost` was posted) and false while
    /// waiting for a shell that was not available at `attach`.
    Lost { reported: bool },
}

/// What the source does on a [`Connection`] transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionAction {
    /// Drop the collection, its sink, every tab sink, and the shell process wait.
    Teardown,
    /// Post `ShellLost`.
    ReportLost,
    /// Start the `GetShellWindow()` poll timer.
    StartPolling,
    StopPolling,
    /// Try to connect now.
    Reconnect,
    /// Verify that the held collection still answers.
    CheckHealth,
    /// Post `ShellAvailable`.
    ReportAvailable,
}

impl Connection {
    /// Connecting succeeded (and the rescan finished).
    pub fn connected(&mut self) -> Vec<ConnectionAction> {
        let actions = match *self {
            Connection::Lost { .. } => {
                vec![ConnectionAction::StopPolling, ConnectionAction::ReportAvailable]
            }
            Connection::Detached | Connection::Attached => Vec::new(),
        };
        *self = Connection::Attached;
        actions
    }

    /// Connecting failed (no shell window, or the collection is unusable).
    pub fn connect_failed(&mut self) -> Vec<ConnectionAction> {
        match *self {
            Connection::Detached => {
                *self = Connection::Lost { reported: false };
                vec![ConnectionAction::StartPolling]
            }
            Connection::Attached | Connection::Lost { .. } => Vec::new(),
        }
    }

    /// The held collection is disconnected (or the shell process exited).
    pub fn disconnected(&mut self) -> Vec<ConnectionAction> {
        match *self {
            Connection::Attached => {
                *self = Connection::Lost { reported: true };
                vec![
                    ConnectionAction::Teardown,
                    ConnectionAction::ReportLost,
                    ConnectionAction::StartPolling,
                ]
            }
            Connection::Detached | Connection::Lost { .. } => Vec::new(),
        }
    }

    /// `TaskbarCreated` was received.
    pub fn taskbar_created(&self) -> Vec<ConnectionAction> {
        match *self {
            Connection::Lost { .. } => vec![ConnectionAction::Reconnect],
            Connection::Attached => vec![ConnectionAction::CheckHealth],
            Connection::Detached => Vec::new(),
        }
    }

    /// The poll timer fired; `shell_window` = `GetShellWindow()` is non-null.
    pub fn poll_tick(&self, shell_window: bool) -> Vec<ConnectionAction> {
        match *self {
            Connection::Lost { .. } if shell_window => vec![ConnectionAction::Reconnect],
            _ => Vec::new(),
        }
    }
}

/// Initial-Location progress of a new window's first tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InitialPhase {
    /// Nothing to report (not a new window's first tab, or already reported).
    #[default]
    Done,
    /// Waiting for the first completed navigation.
    AwaitNavigation,
    /// Unclaimed; waiting for `DocumentComplete` to capture the selection.
    AwaitDocument(LocationId),
}

impl InitialPhase {
    /// A navigation of the tab completed at `location`. While waiting for the
    /// first navigation, a known Location is offered to `claim`: a claimed
    /// window is done (the request is returned), an unclaimed one waits for
    /// `DocumentComplete`. A navigation without a Location_Identifier ends
    /// the wait without a report.
    pub fn navigated(
        self,
        location: &ObservedLocation,
        claim: impl FnOnce(&LocationId) -> Option<RequestId>,
    ) -> (InitialPhase, Option<RequestId>) {
        match (self, location) {
            (InitialPhase::AwaitNavigation, ObservedLocation::Known(l)) => match claim(l) {
                Some(request) => (InitialPhase::Done, Some(request)),
                None => (InitialPhase::AwaitDocument(l.clone()), None),
            },
            (InitialPhase::AwaitNavigation, ObservedLocation::Unidentifiable) => {
                (InitialPhase::Done, None)
            }
            (phase, _) => (phase, None),
        }
    }

    /// `DocumentComplete` arrived or its wait elapsed: the Location to
    /// announce with `InitialLocationKnown`, if one is pending.
    pub fn document_ready(self) -> (InitialPhase, Option<LocationId>) {
        match self {
            InitialPhase::AwaitDocument(l) => (InitialPhase::Done, Some(l)),
            other => (other, None),
        }
    }
}

/// Diffs a ShellWindows enumeration (`present`, by COM identity) against the
/// `known` entries. Returns `(gone, added)`. When the enumeration was not
/// `complete`, nothing is gone.
pub fn diff_entries(known: &[usize], present: &[usize], complete: bool) -> (Vec<usize>, Vec<usize>) {
    let known_set: HashSet<usize> = known.iter().copied().collect();
    let present_set: HashSet<usize> = present.iter().copied().collect();
    let gone = if complete {
        known
            .iter()
            .copied()
            .filter(|k| !present_set.contains(k))
            .collect()
    } else {
        Vec::new()
    };
    let mut added = Vec::new();
    for p in present {
        if !known_set.contains(p) && !added.contains(p) {
            added.push(*p);
        }
    }
    (gone, added)
}

/// Whether the empty indices (`holes`) of a full enumeration with `count`
/// entries are entries `Item` never returns, given the empty indices of the
/// previous full enumeration (`previous`: its count and holes).
///
/// Explorer keeps an entry for a closed tab while a client still holds its
/// automation object, and `Item` returns nothing for it (seen: 23–118 of
/// them on a 22621 VM). New entries are appended, so while a restoration
/// adds tabs the count grows at every sync. The holes are stable when the
/// collection did not shrink, the holes below the previous count are
/// exactly the previous holes, and no hole lies beyond the previous count (a
/// new trailing hole may be a tab still registering; it counts once it has
/// been seen twice).
pub fn holes_stable(previous: Option<&(i32, Vec<usize>)>, count: i32, holes: &[usize]) -> bool {
    let Some((prev_count, prev_holes)) = previous else {
        return false;
    };
    *prev_count <= count && holes.iter().all(|&i| (i as i64) < *prev_count as i64) && holes == prev_holes.as_slice()
}

const RPC_E_DISCONNECTED: u32 = 0x8001_0108;
const CO_E_OBJNOTCONNECTED: u32 = 0x8004_01FD;
const RPC_S_SERVER_UNAVAILABLE: u32 = 0x8007_06BA;
const RPC_S_CALL_FAILED_DNE: u32 = 0x8007_06BF;
const RPC_E_SERVER_DIED_DNE: u32 = 0x8001_0012;

/// Whether `code` means the shell's collection is gone for good
/// (`RPC_E_DISCONNECTED` and equivalents). Any other failure of a collection
/// call is treated as transient and retried.
pub fn is_disconnect(code: HRESULT) -> bool {
    matches!(
        code.0 as u32,
        RPC_E_DISCONNECTED
            | CO_E_OBJNOTCONNECTED
            | RPC_S_SERVER_UNAVAILABLE
            | RPC_S_CALL_FAILED_DNE
            | RPC_E_SERVER_DIED_DNE
    )
}

/// The items selected in `browser`'s view, as absolute PIDLs
/// (`IFolderView2::GetSelection`). Empty when nothing is selected or the
/// view cannot be read.
pub fn selection_of(browser: &IWebBrowser2) -> Vec<ItemId> {
    let Ok(sb) = shell_browser(browser) else {
        return Vec::new();
    };
    // SAFETY: plain COM calls on live interfaces.
    let Ok(view) = (unsafe { sb.QueryActiveShellView() }) else {
        return Vec::new();
    };
    let Ok(folder_view) = view.cast::<IFolderView2>() else {
        return Vec::new();
    };
    // SAFETY: plain COM call; an empty selection is an error or an empty array.
    let Ok(items) = (unsafe { folder_view.GetSelection(false) }) else {
        return Vec::new();
    };
    // SAFETY: plain COM call.
    let count = unsafe { items.GetCount() }.unwrap_or(0);
    (0..count)
        .filter_map(|i| {
            // SAFETY: index below the reported count.
            let item = unsafe { items.GetItemAt(i) }.ok()?;
            // SAFETY: returns a CoTaskMemAlloc'd absolute PIDL we own.
            let pidl = unsafe { SHGetIDListFromObject(&item) }.ok()?;
            if pidl.is_null() {
                return None;
            }
            // SAFETY: valid PIDL from the shell; freed exactly once.
            let bytes = unsafe { pidl_to_bytes(pidl) };
            unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
            Some(ItemId(bytes))
        })
        .collect()
}

/// The tab's current Location, or `None` if its folder cannot be read.
fn read_location(browser: &IWebBrowser2) -> Option<ObservedLocation> {
    if let Ok(pidl) = current_folder_pidl(browser) {
        return Some(capture_from_pidl(&pidl));
    }
    // SAFETY: returns a CoTaskMemAlloc'd absolute PIDL we own.
    let pidl = unsafe { SHGetIDListFromObject(browser) }.ok()?;
    if pidl.is_null() {
        return None;
    }
    // SAFETY: valid PIDL from the shell; freed exactly once.
    let bytes = unsafe { pidl_to_bytes(pidl) };
    unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
    Some(capture_from_pidl(&bytes))
}

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

/// The `ShellWindowsSource` adapter (backs the `TabSource` port in `esr-app`).
/// Bound to the thread that created it; see the module docs.
pub struct ShellWindowsSource {
    inner: Rc<Inner>,
}

struct Inner {
    windows: WindowKeys,
    tabs: TabKeys,
    handler: Rc<dyn ShellSourceHandler>,
    /// Set by the sinks (any thread) when the collection changed; cleared
    /// when a sync starts.
    dirty: Arc<AtomicBool>,
    /// `NavigateComplete2` URL arguments per record id, in event order (set
    /// by the tab sinks, taken when the event is handled).
    urls: Arc<Mutex<HashMap<u64, VecDeque<Vec<u16>>>>>,
    syncing: Cell<bool>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    conn: Connection,
    window: Option<HWND>,
    shell: Option<ShellLink>,
    records: Vec<TabRecord>,
    /// Entries that are not Explorer tabs; kept so their identity is stable.
    ignored: Vec<(usize, IDispatch)>,
    /// Failed registration attempts per entry identity.
    attempts: HashMap<usize, u32>,
    /// Consecutive syncs whose enumeration had a failed or empty slot.
    incomplete_streak: u32,
    /// Windows whose tabs were seen (a tab in any other window starts a new window).
    seen: HashSet<WinKey>,
    /// Tab keys of tabs dropped at a shell loss, retired after reconnecting
    /// unless the tab was found again.
    lost_tabs: Vec<TabKey>,
    next_id: u64,
    polling: bool,
    resync_timer: bool,
    /// Entry identities per collection index (`None`: an index without an
    /// entry), from the last complete sync (see `appended_entries`).
    order: Vec<Option<usize>>,
    /// Count and empty indices of the last full enumeration that had empty
    /// indices (see `sync_once`).
    holes: Option<(i32, Vec<usize>)>,
}

/// The held collection.
struct ShellLink {
    windows: IShellWindows,
    advise: Option<Advise>,
    wait: Option<ShellProcessWait>,
    pid: u32,
}

/// One connection point subscription.
struct Advise {
    point: IConnectionPoint,
    cookie: u32,
}

impl Advise {
    fn new(source: &impl Interface, iid: &GUID, sink: &IUnknown) -> windows::core::Result<Self> {
        let container: IConnectionPointContainer = source.cast()?;
        // SAFETY: plain COM calls on live interfaces.
        let point = unsafe { container.FindConnectionPoint(iid)? };
        // SAFETY: as above; the cookie is released by `release`.
        let cookie = unsafe { point.Advise(sink)? };
        Ok(Self { point, cookie })
    }

    /// Unadvise; errors (a dead server) are ignored.
    fn release(self) {
        // SAFETY: plain COM call with the cookie Advise returned.
        let _ = unsafe { self.point.Unadvise(self.cookie) };
    }
}

struct TabRecord {
    id: u64,
    /// COM identity of the entry.
    unk: usize,
    browser: IWebBrowser2,
    advise: Option<Advise>,
    /// The sink was handed to a browser worker
    /// ([`ShellSourceHandler::attach_remote`]); `advise` is `None`.
    remote: bool,
    win: WinKey,
    tab: TabKey,
    location: ObservedLocation,
    initial: InitialPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncMode {
    /// Report registrations and revocations.
    Live,
    /// Attach or reconnect: record what is there, report nothing.
    Rescan,
}

enum SyncError {
    NotAttached,
    Disconnected(ShellError),
}

struct Resolved {
    browser: IWebBrowser2,
    top: HWND,
    tab_window: HWND,
}

enum Resolution {
    Tab(Resolved),
    Ignored,
    Retry,
}

thread_local! {
    static ACTIVE: RefCell<Option<Weak<Inner>>> = const { RefCell::new(None) };
}

fn active_source() -> Option<Rc<Inner>> {
    ACTIVE
        .try_with(|a| {
            a.try_borrow()
                .ok()
                .and_then(|a| a.as_ref().and_then(Weak::upgrade))
        })
        .ok()
        .flatten()
}

fn set_active(inner: Option<&Rc<Inner>>) {
    let _ = ACTIVE.try_with(|a| *a.borrow_mut() = inner.map(Rc::downgrade));
}

impl ShellWindowsSource {
    /// `windows` and `tabs` are the key registries shared with the other
    /// adapters, so keys agree across them.
    pub fn new(windows: WindowKeys, tabs: TabKeys, handler: Rc<dyn ShellSourceHandler>) -> Self {
        Self {
            inner: Rc::new(Inner {
                windows,
                tabs,
                handler,
                dirty: Arc::new(AtomicBool::new(false)),
                urls: Arc::new(Mutex::new(HashMap::new())),
                syncing: Cell::new(false),
                state: RefCell::new(State::default()),
            }),
        }
    }

    pub fn connection(&self) -> Connection {
        self.inner.state.borrow().conn
    }

    /// Connects to the shell's window collection, attaches every sink, and
    /// records the open windows (`window_changed` is called for each). The
    /// calling thread must be an STA that pumps messages.
    ///
    /// If no shell is available, the source keeps polling for one (posting
    /// `ShellAvailable` when it connects) and the error is returned. Fails
    /// if another source is attached on this thread.
    pub fn attach(&mut self) -> Result<(), ShellError> {
        match self.connection() {
            Connection::Attached => return Ok(()),
            Connection::Lost { .. } => return self.inner.connect(),
            Connection::Detached => {}
        }
        if active_source().is_some() {
            return Err(error(
                0,
                "another ShellWindowsSource is attached on this thread".into(),
            ));
        }
        let window = create_window()?;
        self.inner.state.borrow_mut().window = Some(window);
        set_active(Some(&self.inner));
        self.inner.connect()
    }

    /// Unadvises every sink and stops polling. Idempotent.
    pub fn detach(&mut self) {
        let window = {
            let mut st = self.inner.state.borrow_mut();
            st.conn = Connection::Detached;
            st.window
        };
        self.inner.set_polling(false);
        self.inner.teardown(false);
        if active_source().is_some_and(|a| Rc::ptr_eq(&a, &self.inner)) {
            set_active(None);
        }
        let mut st = self.inner.state.borrow_mut();
        st.window = None;
        st.resync_timer = false;
        st.seen.clear();
        st.lost_tabs.clear();
        drop(st);
        if let Some(window) = window {
            // SAFETY: created by this source on this thread; its timers go with it.
            let _ = unsafe { DestroyWindow(window) };
        }
    }

    /// Every open top-level Explorer window with at least one tab.
    pub fn windows(&mut self) -> Result<Vec<WinKey>, ShellError> {
        self.inner.refresh(false)?;
        Ok(self.inner.live_windows())
    }

    /// The tabs currently hosted by `win`, with their current Locations
    /// (re-read for every navigated tab, unless the handler says the reported
    /// ones are current, [`ShellSourceHandler::cached_locations_current`]).
    pub fn tabs_of(&mut self, win: WinKey) -> Result<ShellWindowTabs, ShellError> {
        let known = self.inner.state.borrow().records.iter().any(|r| r.win == win);
        self.inner.refresh(!known)?;
        let snapshot: Vec<(u64, IWebBrowser2, TabKey, ObservedLocation)> = self
            .inner
            .state
            .borrow()
            .records
            .iter()
            .filter(|r| r.win == win)
            .map(|r| (r.id, r.browser.clone(), r.tab, r.location.clone()))
            .collect();
        let hwnd = hwnd_of(win.hwnd);
        if snapshot.is_empty()
            && !(self.inner.windows.get(win.hwnd) == Some(win) && is_cabinet_window(hwnd))
        {
            return Err(error(0, format!("{win:?} is not a live Explorer window")));
        }
        let mut tabs = Vec::with_capacity(snapshot.len());
        let cached_current = self.inner.handler.cached_locations_current(win);
        for (id, browser, tab, cached) in snapshot {
            let location = match cached {
                ObservedLocation::NotNavigated => ObservedLocation::NotNavigated,
                cached if cached_current => cached,
                cached => match current_folder_pidl(&browser) {
                    Ok(pidl) => {
                        let current = capture_from_pidl(&pidl);
                        self.inner.update_location(id, &current);
                        current
                    }
                    Err(_) => cached,
                },
            };
            tabs.push((tab, location));
        }
        Ok(ShellWindowTabs {
            pid: window_process_id(hwnd),
            tabs,
            active: None,
        })
    }

    /// `TaskbarCreated` arrived at another window (the Engine's): reconnect
    /// if lost, otherwise check the connection.
    pub fn on_taskbar_created(&self) {
        self.inner.on_taskbar_created();
    }

    /// The runtime learned that the shell is gone (for example the shell's
    /// PID exited): treat the collection as disconnected.
    pub fn shell_disconnected(&self) {
        self.inner.disconnected();
    }

    /// Number of tabs with an attached sink.
    pub fn tab_count(&self) -> usize {
        self.inner.state.borrow().records.len()
    }
}

impl Drop for ShellWindowsSource {
    fn drop(&mut self) {
        self.detach();
    }
}

impl Inner {
    fn window(&self) -> Option<HWND> {
        self.state.borrow().window
    }

    fn owns_window(&self, window: HWND) -> bool {
        self.state.borrow().window == Some(window)
    }

    fn is_attached(&self) -> bool {
        self.state.borrow().conn == Connection::Attached
    }

    fn run(&self, actions: Vec<ConnectionAction>) {
        for action in actions {
            match action {
                ConnectionAction::Teardown => self.teardown(true),
                ConnectionAction::ReportLost => self.handler.post(Event::ShellLost),
                ConnectionAction::StartPolling => self.set_polling(true),
                ConnectionAction::StopPolling => self.set_polling(false),
                ConnectionAction::Reconnect => {
                    let _ = self.connect();
                }
                ConnectionAction::CheckHealth => self.check_health(),
                ConnectionAction::ReportAvailable => self.handler.post(Event::ShellAvailable),
            }
        }
    }

    /// Acquire the collection, attach its sink and every tab sink, rescan.
    fn connect(&self) -> Result<(), ShellError> {
        let Some(window) = self.window() else {
            return Err(error(0, "not attached".into()));
        };
        // SAFETY: no preconditions.
        let shell_window = unsafe { GetShellWindow() };
        if shell_window.is_invalid() {
            return self.connect_failed(error(0, "no shell window (GetShellWindow)".into()));
        }
        let collection = match shell_windows() {
            Ok(c) => c,
            Err(e) => return self.connect_failed(win_error("CoCreateInstance(ShellWindows)", &e)),
        };
        let sink: DShellWindowsEvents = ShellWindowsSink {
            window: window.0 as isize,
            dirty: self.dirty.clone(),
        }
        .into();
        let advise = match Advise::new(&collection, &DShellWindowsEvents::IID, &sink.into()) {
            Ok(a) => a,
            Err(e) => return self.connect_failed(win_error("advise DShellWindowsEvents", &e)),
        };
        let pid = window_process_id(shell_window);
        let wait = ShellProcessWait::new(window, pid);
        self.state.borrow_mut().shell = Some(ShellLink {
            windows: collection,
            advise: Some(advise),
            wait,
            pid,
        });
        self.dirty.store(false, Ordering::SeqCst);
        match self.sync(SyncMode::Rescan) {
            Ok(()) => {}
            Err(SyncError::Disconnected(e)) => {
                self.teardown(true);
                return self.connect_failed(e);
            }
            Err(SyncError::NotAttached) => {
                self.teardown(true);
                return self.connect_failed(error(0, "collection dropped while connecting".into()));
            }
        }
        let actions = self.state.borrow_mut().conn.connected();
        self.run(actions);
        self.retire_lost_tabs();
        for win in self.live_windows() {
            self.handler.window_changed(win);
        }
        Ok(())
    }

    fn connect_failed(&self, e: ShellError) -> Result<(), ShellError> {
        let actions = self.state.borrow_mut().conn.connect_failed();
        self.run(actions);
        Err(e)
    }

    fn disconnected(&self) {
        let actions = self.state.borrow_mut().conn.disconnected();
        self.run(actions);
    }

    fn on_taskbar_created(&self) {
        let actions = self.state.borrow().conn.taskbar_created();
        self.run(actions);
    }

    fn on_poll(&self) {
        // SAFETY: no preconditions.
        let present = !unsafe { GetShellWindow() }.is_invalid();
        let actions = self.state.borrow().conn.poll_tick(present);
        self.run(actions);
    }

    fn check_health(&self) {
        let collection = self.state.borrow().shell.as_ref().map(|l| l.windows.clone());
        if let Some(collection) = collection {
            // SAFETY: plain COM property read.
            if let Err(e) = unsafe { collection.Count() }
                && is_disconnect(e.code())
            {
                self.disconnected();
            }
        }
    }

    fn on_shell_process_exited(&self, pid: u32) {
        let is_shell = self
            .state
            .borrow()
            .shell
            .as_ref()
            .is_some_and(|l| l.pid == pid);
        if is_shell {
            self.disconnected();
        }
    }

    /// Drops the collection and every sink. With `lost`, the dropped tabs'
    /// keys are remembered for retirement after reconnecting.
    fn teardown(&self, lost: bool) {
        let (shell, records) = {
            let mut st = self.state.borrow_mut();
            st.ignored.clear();
            st.attempts.clear();
            st.incomplete_streak = 0;
            st.order.clear();
            st.holes = None;
            (st.shell.take(), std::mem::take(&mut st.records))
        };
        if let Some(link) = shell {
            if let Some(advise) = link.advise {
                advise.release();
            }
            if let Some(wait) = link.wait {
                wait.release();
            }
        }
        let mut dropped = Vec::with_capacity(records.len());
        self.urls.lock().unwrap_or_else(PoisonError::into_inner).clear();
        for record in records {
            if let Some(advise) = record.advise {
                advise.release();
            }
            if record.remote {
                self.handler.detach_remote(record.tab);
            }
            dropped.push(record.tab);
        }
        if lost {
            self.state.borrow_mut().lost_tabs.extend(dropped);
        }
    }

    /// Retires the keys of tabs lost with the shell that were not found again.
    fn retire_lost_tabs(&self) {
        let (lost, live): (Vec<TabKey>, HashSet<TabKey>) = {
            let mut st = self.state.borrow_mut();
            let live = st.records.iter().map(|r| r.tab).collect();
            (std::mem::take(&mut st.lost_tabs), live)
        };
        for tab in lost {
            if !live.contains(&tab) && self.tabs.get(tab.hwnd) == Some(tab) {
                self.tabs.destroyed(tab.hwnd);
            }
        }
    }

    fn set_polling(&self, on: bool) {
        let (window, polling) = {
            let st = self.state.borrow();
            (st.window, st.polling)
        };
        let Some(window) = window else {
            self.state.borrow_mut().polling = false;
            return;
        };
        if on && !polling {
            // SAFETY: our own window on this thread.
            let id = unsafe { SetTimer(Some(window), TIMER_POLL, SHELL_POLL_INTERVAL_MS, None) };
            self.state.borrow_mut().polling = id != 0;
        } else if !on && polling {
            // SAFETY: our own window's timer.
            let _ = unsafe { KillTimer(Some(window), TIMER_POLL) };
            self.state.borrow_mut().polling = false;
        }
    }

    fn schedule_resync(&self) {
        let window = {
            let st = self.state.borrow();
            if st.resync_timer {
                return;
            }
            st.window
        };
        if let Some(window) = window {
            // SAFETY: our own window on this thread.
            let id = unsafe { SetTimer(Some(window), TIMER_RESYNC, RESYNC_RETRY_MS, None) };
            self.state.borrow_mut().resync_timer = id != 0;
        }
    }

    fn on_resync_timer(&self) {
        self.state.borrow_mut().resync_timer = false;
        self.dirty.store(true, Ordering::SeqCst);
        self.on_shell_windows_changed();
    }

    fn on_shell_windows_changed(&self) {
        if !self.is_attached() || !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Err(SyncError::Disconnected(_)) = self.sync(SyncMode::Live) {
            self.disconnected();
        }
    }

    /// Syncs when the collection changed (or when `force`d). Fails when not
    /// attached or when the shell turned out to be gone.
    fn refresh(&self, force: bool) -> Result<(), ShellError> {
        if !self.is_attached() {
            return Err(error(0, "the shell is not available".into()));
        }
        let changed = self.dirty.swap(false, Ordering::SeqCst);
        if !(changed || force) {
            return Ok(());
        }
        match self.sync(SyncMode::Live) {
            Ok(()) => Ok(()),
            Err(SyncError::NotAttached) => Err(error(0, "the shell is not available".into())),
            Err(SyncError::Disconnected(e)) => {
                self.disconnected();
                Err(e)
            }
        }
    }

    fn sync(&self, mode: SyncMode) -> Result<(), SyncError> {
        if self.syncing.replace(true) {
            // Re-entered from a COM call of the running sync: run again later.
            self.dirty.store(true, Ordering::SeqCst);
            self.schedule_resync();
            return Ok(());
        }
        let start = std::time::Instant::now();
        let result = self.sync_once(mode);
        crate::diag::note(|| format!("src: sync {:.1} ms", start.elapsed().as_secs_f64() * 1000.0));
        self.syncing.set(false);
        result
    }

    fn sync_once(&self, mode: SyncMode) -> Result<(), SyncError> {
        crate::diag::note(|| "src: sync begin".into());
        let collection = match &self.state.borrow().shell {
            Some(link) => link.windows.clone(),
            None => return Err(SyncError::NotAttached),
        };
        // SAFETY: plain COM property read.
        let count = match unsafe { collection.Count() } {
            Ok(c) => c,
            Err(e) if is_disconnect(e.code()) => {
                return Err(SyncError::Disconnected(win_error("IShellWindows::Count", &e)));
            }
            Err(_) => {
                self.dirty.store(true, Ordering::SeqCst);
                self.schedule_resync();
                return Ok(());
            }
        };
        let mut enumerated = true;
        let t_append = std::time::Instant::now();
        let appended = self.appended_entries(&collection, count)?;
        let fast = appended.is_some();
        crate::diag::note(|| {
            format!(
                "src: sync count={count} fast={fast} list {:.1} ms",
                t_append.elapsed().as_secs_f64() * 1000.0
            )
        });
        let mut present: Vec<(usize, IDispatch)> = appended.unwrap_or_default();
        // Per index: the entry's identity, or `None` for an index without one.
        let mut slots: Vec<Option<usize>> = Vec::new();
        for index in (0..count).filter(|_| !fast) {
            // SAFETY: `variant_i4` owns no resources.
            match unsafe { collection.Item(&variant_i4(index)) } {
                Ok(dispatch) => match dispatch.cast::<IUnknown>() {
                    Ok(unknown) => {
                        let id = unknown.as_raw() as usize;
                        slots.push(Some(id));
                        present.push((id, dispatch));
                    }
                    Err(_) => {
                        slots.push(None);
                        enumerated = false;
                    }
                },
                Err(e) if is_disconnect(e.code()) => {
                    return Err(SyncError::Disconnected(win_error("IShellWindows::Item", &e)));
                }
                // A transient RPC failure, or an index below `Count` without
                // an entry (`S_FALSE` + null). The latter happens for live
                // tabs while another window closes, so on its own it proves
                // nothing. It also happens for entries of other kinds that
                // `Item` does not return (seen: 23 of them on a 22621 VM).
                Err(_) => {
                    slots.push(None);
                    enumerated = false;
                }
            }
        }
        // Empty indices that stay the same between two full enumerations
        // (the collection may have grown meanwhile) are entries `Item` never
        // returns: the view is complete despite them (`holes_stable`).
        let stable_holes = !fast && !enumerated && {
            let holes: Vec<usize> = (0..slots.len()).filter(|&i| slots[i].is_none()).collect();
            let mut st = self.state.borrow_mut();
            let records: HashSet<usize> = st.records.iter().map(|r| r.unk).collect();
            // A hole where the last complete view had a known tab may be that
            // tab, briefly without an entry (another window closing).
            let was_tab = holes.iter().any(|&i| st.order.get(i).copied().flatten().is_some_and(|id| records.contains(&id)));
            let stable = !was_tab && holes_stable(st.holes.as_ref(), count, &holes);
            crate::diag::note(|| {
                format!(
                    "src: holes {} (previous {:?}), was tab {was_tab}, stable {stable}",
                    holes.len(),
                    st.holes.as_ref().map(|(c, h)| (*c, h.len()))
                )
            });
            st.holes = Some((count, holes));
            stable
        };
        if !fast && enumerated {
            self.state.borrow_mut().holes = None;
        }
        let enumerated = enumerated || stable_holes;
        let streak = {
            let mut st = self.state.borrow_mut();
            st.incomplete_streak = if enumerated { 0 } else { st.incomplete_streak + 1 };
            st.incomplete_streak
        };
        // Revocations need a complete enumeration. A collection that keeps an
        // empty slot is accepted as it is after a while.
        let complete = enumerated || streak >= MAX_INCOMPLETE_SYNCS;
        crate::diag::note(|| {
            format!(
                "src: sync enumerated {} entries in {:.1} ms (complete={complete}, stable holes={stable_holes}, streak={streak})",
                present.len(),
                t_append.elapsed().as_secs_f64() * 1000.0
            )
        });
        let mut resync = !complete;
        let present_ids: Vec<usize> = {
            let mut st = self.state.borrow_mut();
            let new_ids: Vec<usize> = present.iter().map(|(id, _)| *id).collect();
            if fast {
                st.order.extend(new_ids.iter().map(|&id| Some(id)));
            } else {
                // The collection order for the next fast path (only a
                // complete view counts).
                st.order = if enumerated { slots } else { Vec::new() };
            }
            if fast { st.order.iter().flatten().copied().collect() } else { new_ids }
        };
        let (mut missing, added) = {
            let st = self.state.borrow();
            let known: Vec<usize> = st
                .records
                .iter()
                .map(|r| r.unk)
                .chain(st.ignored.iter().map(|(id, _)| *id))
                .collect();
            diff_entries(&known, &present_ids, true)
        };

        // Resolve the new entries. One whose tab window belongs to a missing
        // record is that tab under a new automation object: Explorer replaces
        // a live tab's object at times (seen when another window closes).
        let mut new_tabs: Vec<(usize, Resolved)> = Vec::new();
        for (unk, dispatch) in present.into_iter().filter(|(id, _)| added.contains(id)) {
            // A re-entrant sync may have registered it meanwhile.
            if self.state.borrow().records.iter().any(|r| r.unk == unk) {
                continue;
            }
            let t_resolve = std::time::Instant::now();
            let resolution = self.resolve(&dispatch, missing.is_empty());
            crate::diag::note(|| {
                let kind = match &resolution {
                    Resolution::Tab(_) => "tab",
                    Resolution::Ignored => "ignored",
                    Resolution::Retry => "retry",
                };
                format!("src: resolve {kind} {:.1} ms", t_resolve.elapsed().as_secs_f64() * 1000.0)
            });
            match resolution {
                Resolution::Tab(resolved) => {
                    let replaced = self.state.borrow().records.iter().find_map(|r| {
                        (missing.contains(&r.unk)
                            && r.tab.hwnd == hwnd_value(resolved.tab_window)
                            && r.win.hwnd == hwnd_value(resolved.top)
                            && self.tabs.get(r.tab.hwnd) == Some(r.tab))
                        .then_some(r.unk)
                    });
                    match replaced {
                        Some(old) => {
                            missing.retain(|m| *m != old);
                            if !self.rebind(old, unk, resolved.browser) {
                                resync = true;
                            }
                        }
                        // Nothing to revoke: register right away and let
                        // the Engine react (navigate it, request the next
                        // tab) before the next entry is resolved, which may
                        // wait for another window's busy thread.
                        None if missing.is_empty() => {
                            if self.register(unk, resolved, mode) {
                                self.state.borrow_mut().attempts.remove(&unk);
                                if mode == SyncMode::Live {
                                    self.handler.flush();
                                }
                            } else {
                                resync = true;
                            }
                        }
                        None => new_tabs.push((unk, resolved)),
                    }
                }
                Resolution::Ignored => self.state.borrow_mut().ignored.push((unk, dispatch)),
                Resolution::Retry => {
                    let mut st = self.state.borrow_mut();
                    let n = st.attempts.entry(unk).or_insert(0);
                    *n += 1;
                    if *n >= MAX_REGISTER_ATTEMPTS {
                        st.attempts.remove(&unk);
                        st.ignored.push((unk, dispatch));
                    } else {
                        resync = true;
                    }
                }
            }
        }

        // Revoke what is gone, but only after a complete enumeration.
        let revoked: Vec<TabRecord> = if complete {
            let mut st = self.state.borrow_mut();
            let (revoked, kept): (Vec<TabRecord>, Vec<TabRecord>) = std::mem::take(&mut st.records)
                .into_iter()
                .partition(|r| missing.contains(&r.unk));
            st.records = kept;
            st.ignored.retain(|(id, _)| !missing.contains(id));
            st.attempts.retain(|id, _| present_ids.contains(id));
            revoked
        } else {
            Vec::new()
        };
        for record in revoked {
            self.urls.lock().unwrap_or_else(PoisonError::into_inner).remove(&record.id);
            if let Some(advise) = record.advise {
                advise.release();
            }
            if record.remote {
                self.handler.detach_remote(record.tab);
            }
            if self.tabs.get(record.tab.hwnd) == Some(record.tab) {
                self.tabs.destroyed(record.tab.hwnd);
            }
            if mode == SyncMode::Live {
                self.handler.post(Event::TabRevoked {
                    win: record.win,
                    tab: record.tab,
                });
            }
        }

        for (unk, resolved) in new_tabs {
            if self.register(unk, resolved, mode) {
                self.state.borrow_mut().attempts.remove(&unk);
            } else {
                resync = true;
            }
        }

        if resync {
            self.dirty.store(true, Ordering::SeqCst);
            self.schedule_resync();
        } else {
            let windows = &self.windows;
            self.state
                .borrow_mut()
                .seen
                .retain(|w| windows.get(w.hwnd) == Some(*w));
        }
        Ok(())
    }

    /// The fast path of a sync that only added entries: when the collection
    /// grew, its last previously seen entry is still at its index, and every
    /// entry after it is unknown, only those entries are read (each `Item`
    /// is a cross-process call; a full enumeration of 30 tabs at every
    /// registration slowed restorations down). `None` falls back to the
    /// full enumeration: after an incomplete sync, with registrations still
    /// being retried, or whenever the order does not match.
    fn appended_entries(
        &self,
        collection: &IShellWindows,
        count: i32,
    ) -> Result<Option<Vec<(usize, IDispatch)>>, SyncError> {
        let (order, known) = {
            let st = self.state.borrow();
            if st.incomplete_streak != 0 || !st.attempts.is_empty() {
                crate::diag::note(|| {
                    format!("src: slow path: streak {} attempts {}", st.incomplete_streak, st.attempts.len())
                });
                return Ok(None);
            }
            let known: HashSet<usize> =
                st.records.iter().map(|r| r.unk).chain(st.ignored.iter().map(|(id, _)| *id)).collect();
            (st.order.clone(), known)
        };
        let len = order.len();
        // Every entry of `order` must still be held (records or ignored), so
        // its identity cannot have been reused by another object.
        let ids = || order.iter().flatten();
        if len == 0 || count.max(0) as usize <= len || ids().any(|id| !known.contains(id)) {
            crate::diag::note(|| {
                format!(
                    "src: slow path: order {len} count {count} unknown in order {} (known {})",
                    ids().filter(|id| !known.contains(id)).count(),
                    known.len()
                )
            });
            return Ok(None);
        }
        let item = |index: usize| -> Result<Option<(usize, IDispatch)>, SyncError> {
            // SAFETY: `variant_i4` owns no resources.
            match unsafe { collection.Item(&variant_i4(index as i32)) } {
                Ok(dispatch) => Ok(dispatch.cast::<IUnknown>().ok().map(|u| (u.as_raw() as usize, dispatch))),
                Err(e) if is_disconnect(e.code()) => {
                    Err(SyncError::Disconnected(win_error("IShellWindows::Item", &e)))
                }
                Err(_) => Ok(None),
            }
        };
        // The last known entry must still be at its index (the collection
        // only grew); a view of holes only proves nothing.
        let Some(last) = order.iter().rposition(Option::is_some) else {
            return Ok(None);
        };
        if item(last)?.is_none_or(|(id, _)| Some(id) != order[last]) {
            return Ok(None);
        }
        let mut new = Vec::with_capacity(count as usize - len);
        for index in len..count as usize {
            match item(index)? {
                Some((id, dispatch)) if !known.contains(&id) && !ids().any(|o| *o == id) => {
                    new.push((id, dispatch))
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(new))
    }

    /// Whether a new entry is an Explorer tab. With `quick`, the tab's window
    /// is taken from the only `ShellTabWindowClass` child of its top-level
    /// window that no record has yet (a local `EnumChildWindows`), which
    /// saves three cross-process calls into the window's thread (the
    /// `SID_STopLevelBrowser` lookup) while a restoration builds the window
    /// and that thread is busy. Any doubt falls back to the lookup.
    fn resolve(&self, dispatch: &IDispatch, quick: bool) -> Resolution {
        let Ok(browser) = dispatch.cast::<IWebBrowser2>() else {
            return Resolution::Ignored;
        };
        let Some(top) = top_level_hwnd(&browser) else {
            return Resolution::Retry;
        };
        if !is_cabinet_window(top) {
            return Resolution::Ignored;
        }
        if quick
            && self
                .windows
                .get(hwnd_value(top))
                .is_some_and(|win| self.handler.creating_tab(win))
        {
            let top_value = hwnd_value(top);
            let known: HashSet<u64> = self
                .state
                .borrow()
                .records
                .iter()
                .filter(|r| r.win.hwnd == top_value)
                .map(|r| r.tab.hwnd)
                .collect();
            let fresh: Vec<u64> =
                crate::navigator::tab_children(top).into_iter().filter(|t| !known.contains(t)).collect();
            if let [only] = fresh.as_slice() {
                return Resolution::Tab(Resolved { browser, top, tab_window: hwnd_of(*only) });
            }
        }
        let Ok(tab_window) = tab_hwnd(&browser) else {
            return Resolution::Retry;
        };
        Resolution::Tab(Resolved {
            browser,
            top,
            tab_window,
        })
    }

    /// Advises a new tab sink for record `id` on `browser`.
    fn advise_tab(&self, id: u64, browser: &IWebBrowser2) -> Option<Advise> {
        let window = self.window()?;
        let sink: DWebBrowserEvents2 = TabSink {
            window: window.0 as isize,
            id,
            dirty: self.dirty.clone(),
            urls: self.urls.clone(),
        }
        .into();
        Advise::new(browser, &DWebBrowserEvents2::IID, &sink.into()).ok()
    }

    /// The sink of record `id` for a browser worker to create (on its own
    /// thread), and the callback that reports a failure to attach it back
    /// to this thread (`WM_REMOTE_ADVISE_FAILED`).
    fn remote_sink(&self, id: u64) -> Option<(SinkFactory, OnFailure)> {
        let window = self.window()?.0 as isize;
        let (dirty, urls) = (self.dirty.clone(), self.urls.clone());
        let sink: SinkFactory = Box::new(move || {
            let sink: DWebBrowserEvents2 = TabSink { window, id, dirty, urls }.into();
            sink.into()
        });
        let failed: OnFailure = Box::new(move |_| {
            // SAFETY: posting to the source's window; a destroyed window just fails.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(window as *mut c_void)),
                    WM_REMOTE_ADVISE_FAILED,
                    WPARAM(0),
                    LPARAM(id as isize),
                )
            };
        });
        Some((sink, failed))
    }

    /// A browser worker could not attach the sink of record `id`: attach it
    /// here. If that fails too, the tab's navigations go unobserved (the
    /// Engine's operation timeouts report them).
    fn on_remote_advise_failed(&self, id: u64) {
        let Some((browser, _, tab, _)) = self.record(id) else { return };
        let still_remote = self.state.borrow().records.iter().any(|r| r.id == id && r.remote);
        if !still_remote {
            return;
        }
        self.handler.detach_remote(tab);
        let advise = self.advise_tab(id, &browser);
        crate::diag::note(|| format!("src: worker advise of {:#x} failed; local advise ok={}", tab.hwnd, advise.is_some()));
        let mut st = self.state.borrow_mut();
        match st.records.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.remote = false;
                r.advise = advise;
            }
            None => {
                drop(st);
                if let Some(a) = advise {
                    a.release();
                }
            }
        }
    }

    /// Moves the record with identity `old` to the tab's new automation
    /// object. Returns false (record unchanged) if the sink cannot be attached.
    fn rebind(&self, old: usize, unk: usize, browser: IWebBrowser2) -> bool {
        let Some(id) = self
            .state
            .borrow()
            .records
            .iter()
            .find(|r| r.unk == old)
            .map(|r| r.id)
        else {
            return false;
        };
        let Some(advise) = self.advise_tab(id, &browser) else {
            return false;
        };
        let (previous, remote_tab) = {
            let mut st = self.state.borrow_mut();
            let Some(r) = st.records.iter_mut().find(|r| r.id == id) else {
                drop(st);
                advise.release();
                return false;
            };
            r.unk = unk;
            r.browser = browser;
            let remote_tab = std::mem::take(&mut r.remote).then_some(r.tab);
            (r.advise.replace(advise), remote_tab)
        };
        if let Some(previous) = previous {
            previous.release();
        }
        // The worker's sink and browser belong to the replaced object.
        if let Some(tab) = remote_tab {
            self.handler.detach_remote(tab);
        }
        true
    }

    /// Attaches a sink to a new tab and records it. Returns false if the
    /// sink cannot be attached (retried at the next sync).
    fn register(&self, unk: usize, resolved: Resolved, mode: SyncMode) -> bool {
        let Resolved {
            browser,
            top,
            tab_window,
        } = resolved;
        let id = {
            let mut st = self.state.borrow_mut();
            st.next_id += 1;
            st.next_id
        };
        let win = self.windows.key_of(hwnd_value(top));
        let tab = self.tabs.key_of(hwnd_value(tab_window));
        // A tab the navigator is creating is about to be navigated (its
        // NavigateComplete2 reports where it arrives). Reading its folder now
        // would wait for Explorer to finish setting up the tab's start
        // location, on the restoration's critical path.
        let creating = mode == SyncMode::Live
            && self.state.borrow().seen.contains(&win)
            && self.handler.creating_tab(win);
        let t_advise = std::time::Instant::now();
        // The same goes for attaching its sink: a browser worker does it (and
        // then issues the tab's navigation), so this thread does not wait for
        // the tab's busy window thread.
        let remote = creating
            && self.remote_sink(id).is_some_and(|(sink, failed)| {
                self.handler.attach_remote(win, tab, &browser, sink, failed)
            });
        let advise = if remote {
            None
        } else {
            match self.advise_tab(id, &browser) {
                Some(a) => Some(a),
                None => return false,
            }
        };
        let skip_read = creating;
        let t_read = std::time::Instant::now();
        let location = if skip_read {
            ObservedLocation::NotNavigated
        } else {
            read_location(&browser).unwrap_or(ObservedLocation::NotNavigated)
        };
        crate::diag::note(|| {
            format!(
                "src: register tab {:#x}: advise {:.1} ms{}, read location {:.1} ms{}",
                hwnd_value(tab_window),
                (t_read - t_advise).as_secs_f64() * 1000.0,
                if remote { " (worker)" } else { "" },
                t_read.elapsed().as_secs_f64() * 1000.0,
                if skip_read { " (skipped)" } else { "" }
            )
        });
        let navigated = location != ObservedLocation::NotNavigated;
        let new_window = {
            let mut st = self.state.borrow_mut();
            let new_window = mode == SyncMode::Live && !st.seen.contains(&win);
            st.seen.insert(win);
            st.records.push(TabRecord {
                id,
                unk,
                browser: browser.clone(),
                advise,
                remote,
                win,
                tab,
                location: location.clone(),
                initial: if new_window {
                    InitialPhase::AwaitNavigation
                } else {
                    InitialPhase::Done
                },
            });
            new_window
        };
        if mode == SyncMode::Live {
            if !new_window {
                self.handler.tab_browser(tab, &browser);
                self.handler.tab_registered(win, tab);
                self.handler.post(Event::TabCreated { win, tab });
            }
            self.handler.window_changed(win);
            if navigated {
                // The first navigation completed before the sink was
                // attached (the Location just read is reported, not read again).
                self.navigated_with(id, location);
            }
            if remote
                && crate::browser_worker::late_sink()
                && let Some(window) = self.window()
            {
                // SAFETY: our own window on this thread; a failed timer only
                // loses the safety net.
                let _ = unsafe { SetTimer(Some(window), TIMER_LATE_SINK_BASE + id as usize, LATE_SINK_CHECK_MS, None) };
            }
        }
        true
    }

    /// The late-sink safety net of record `id` ([`LATE_SINK_CHECK_MS`]): a
    /// tab the navigator navigated whose completion has not been seen may
    /// have completed before its sink was attached; its Location is read.
    fn on_late_sink_check(&self, id: u64) {
        let Some((_, _, tab, location)) = self.record(id) else { return };
        if location != ObservedLocation::NotNavigated || !self.handler.navigation_expected(tab) {
            return;
        }
        crate::diag::note(|| format!("src: late-sink check of {:#x}: no completion seen; reading it", tab.hwnd));
        self.navigated(id);
    }

    fn live_windows(&self) -> Vec<WinKey> {
        let st = self.state.borrow();
        let mut out: Vec<WinKey> = Vec::new();
        for r in &st.records {
            if !out.contains(&r.win) && self.windows.get(r.win.hwnd) == Some(r.win) {
                out.push(r.win);
            }
        }
        out
    }

    fn record(&self, id: u64) -> Option<(IWebBrowser2, WinKey, TabKey, ObservedLocation)> {
        self.state
            .borrow()
            .records
            .iter()
            .find(|r| r.id == id)
            .map(|r| (r.browser.clone(), r.win, r.tab, r.location.clone()))
    }

    fn update_location(&self, id: u64, location: &ObservedLocation) {
        if let Some(r) = self.state.borrow_mut().records.iter_mut().find(|r| r.id == id) {
            r.location = location.clone();
        }
    }

    fn take_phase(&self, id: u64) -> Option<InitialPhase> {
        self.state
            .borrow_mut()
            .records
            .iter_mut()
            .find(|r| r.id == id)
            .map(|r| std::mem::take(&mut r.initial))
    }

    fn set_phase(&self, id: u64, phase: InitialPhase) {
        if let Some(r) = self.state.borrow_mut().records.iter_mut().find(|r| r.id == id) {
            r.initial = phase;
        }
    }

    fn on_tab_event(&self, dispid: i32, id: u64) {
        if !self.is_attached() {
            return;
        }
        let start = std::time::Instant::now();
        match dispid {
            DISPID_NAVIGATECOMPLETE2 => self.navigated(id),
            DISPID_DOCUMENTCOMPLETE => self.document_ready(id),
            _ => {}
        }
        crate::diag::note(|| format!("src: tab event {dispid} id {id} {:.1} ms", start.elapsed().as_secs_f64() * 1000.0));
    }

    /// A navigation of tab `id` completed. The Location is the one the
    /// navigator navigated the tab to when the event's `URL` is its parsing
    /// name; otherwise it is read from the tab.
    fn navigated(&self, id: u64) {
        let url = self
            .urls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&id)
            .and_then(VecDeque::pop_front);
        let Some((browser, _, tab, _)) = self.record(id) else {
            return;
        };
        if let Some(l) = url
            .filter(|u| !u.is_empty())
            .and_then(|u| self.handler.expected_location(tab, &u))
        {
            crate::diag::note(|| format!("src: navigated {:#x} (expected location)", tab.hwnd));
            return self.navigated_with(id, ObservedLocation::Known(l));
        }
        let location = read_location(&browser).unwrap_or(ObservedLocation::Unidentifiable);
        self.navigated_with(id, location);
    }

    /// A navigation of tab `id` completed at `location` (already read).
    fn navigated_with(&self, id: u64, location: ObservedLocation) {
        let Some((_, win, tab, _)) = self.record(id) else {
            return;
        };
        self.update_location(id, &location);
        match &location {
            ObservedLocation::Known(l) => self.handler.post(Event::NavigationCompleted {
                tab,
                location: l.clone(),
            }),
            _ => self.handler.window_changed(win),
        }
        let Some(phase) = self.take_phase(id) else {
            return;
        };
        let handler = self.handler.clone();
        let (phase, claimed) = phase.navigated(&location, |l| handler.claim(win, l));
        if let Some(request) = claimed {
            self.handler.post(Event::WindowOpened { request, win, tab: Some(tab) });
        }
        let waiting = matches!(phase, InitialPhase::AwaitDocument(_));
        self.set_phase(id, phase);
        if waiting && let Some(window) = self.window() {
            // SAFETY: our own window on this thread.
            let timer = unsafe {
                SetTimer(
                    Some(window),
                    TIMER_DOCUMENT_BASE + id as usize,
                    DOCUMENT_COMPLETE_WAIT_MS,
                    None,
                )
            };
            if timer == 0 {
                self.document_ready(id);
            }
        }
    }

    /// `DocumentComplete` of tab `id`, or its wait elapsed.
    fn document_ready(&self, id: u64) {
        let Some((browser, win, tab, location)) = self.record(id) else {
            return;
        };
        if location == ObservedLocation::NotNavigated && !self.handler.navigation_expected(tab) {
            // NavigateComplete2 was missed; this completes the navigation too.
            // (Not for a tab the navigator navigates: its NavigateComplete2
            // follows; this is its start location's document.)
            self.navigated(id);
        }
        let Some(phase) = self.take_phase(id) else {
            return;
        };
        let (phase, announce) = phase.document_ready();
        self.set_phase(id, phase);
        if let Some(location) = announce {
            if let Some(window) = self.window() {
                // SAFETY: our own window's timer (absent timers are ignored).
                let _ = unsafe { KillTimer(Some(window), TIMER_DOCUMENT_BASE + id as usize) };
            }
            let selection = selection_of(&browser);
            self.handler.post(Event::InitialLocationKnown {
                win,
                tab: Some(tab),
                location,
                selection,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Shell process wait
// ---------------------------------------------------------------------------

/// `RegisterWaitForSingleObject` on the shell process.
struct ShellProcessWait {
    process: HANDLE,
    wait: HANDLE,
    /// Read by the thread-pool callback; freed only after the wait is
    /// unregistered with completion.
    _context: Box<WaitContext>,
}

struct WaitContext {
    window: isize,
    pid: u32,
}

impl ShellProcessWait {
    fn new(window: HWND, pid: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        // SAFETY: plain handle-returning call; the handle is owned by the wait.
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.ok()?;
        let context = Box::new(WaitContext {
            window: window.0 as isize,
            pid,
        });
        let mut wait = HANDLE::default();
        let ptr: *const WaitContext = &*context;
        // SAFETY: `ptr` stays valid until `release` unregisters the wait and
        // waits for a running callback; the callback only reads it.
        let registered = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                process,
                Some(shell_process_signalled),
                Some(ptr.cast::<c_void>()),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        if registered.is_err() {
            // SAFETY: opened above, closed once.
            let _ = unsafe { CloseHandle(process) };
            return None;
        }
        Some(Self {
            process,
            wait,
            _context: context,
        })
    }

    fn release(self) {
        // SAFETY: a registered wait; INVALID_HANDLE_VALUE blocks until any
        // running callback (which only posts a message) has returned.
        let _ = unsafe { UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE)) };
        // SAFETY: owned by this wait and closed once.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

/// Wait callback (thread-pool thread): hand the PID to the source's thread.
unsafe extern "system" fn shell_process_signalled(context: *mut c_void, _timed_out: bool) {
    // SAFETY: `context` is the wait's `WaitContext`, alive until unregistered.
    let context = unsafe { &*(context as *const WaitContext) };
    // SAFETY: posting to another thread's window; a destroyed window just fails.
    let _ = unsafe {
        PostMessageW(
            Some(HWND(context.window as *mut c_void)),
            WM_SHELL_PROCESS_EXITED,
            WPARAM(context.pid as usize),
            LPARAM(0),
        )
    };
}

// ---------------------------------------------------------------------------
// COM sinks
// ---------------------------------------------------------------------------

/// `DShellWindowsEvents`: marks the collection dirty and wakes the source.
#[implement(DShellWindowsEvents)]
struct ShellWindowsSink {
    window: isize,
    dirty: Arc<AtomicBool>,
}

impl DShellWindowsEvents_Impl for ShellWindowsSink_Impl {}

impl IDispatch_Impl for ShellWindowsSink_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }

    fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }

    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        _names: *const PCWSTR,
        _count: u32,
        _lcid: u32,
        _dispids: *mut i32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(
        &self,
        dispid: i32,
        _riid: *const GUID,
        _lcid: u32,
        _flags: DISPATCH_FLAGS,
        _params: *const DISPPARAMS,
        _result: *mut VARIANT,
        _excepinfo: *mut EXCEPINFO,
        _argerr: *mut u32,
    ) -> windows::core::Result<()> {
        if matches!(dispid, DISPID_WINDOWREGISTERED | DISPID_WINDOWREVOKED) {
            crate::diag::note(|| format!("src: ShellWindows event {dispid}"));
            mark_dirty(self.window, &self.dirty);
        }
        Ok(())
    }
}

/// `DWebBrowserEvents2` of one tab: forwards completed navigations and
/// documents to the source's thread.
#[implement(DWebBrowserEvents2)]
struct TabSink {
    window: isize,
    id: u64,
    dirty: Arc<AtomicBool>,
    urls: Arc<Mutex<HashMap<u64, VecDeque<Vec<u16>>>>>,
}

/// The `URL` argument of `NavigateComplete2(pDisp, URL)`: the parsing name
/// of the Location navigated to (a `BSTR`, normally behind
/// `VT_BYREF | VT_VARIANT`). `None` if it is not a string.
///
/// # Safety
///
/// `params` is null or the `DISPPARAMS` of the current `Invoke` call.
unsafe fn url_argument(params: *const DISPPARAMS) -> Option<Vec<u16>> {
    // SAFETY: per the contract; arguments are in reverse order, so URL is
    // `rgvarg[0]`.
    unsafe {
        let p = params.as_ref()?;
        if p.cArgs < 1 || p.rgvarg.is_null() {
            return None;
        }
        let mut v: &VARIANT = &*p.rgvarg;
        if v.Anonymous.Anonymous.vt == VARENUM(VT_BYREF.0 | VT_VARIANT.0) {
            v = v.Anonymous.Anonymous.Anonymous.pvarVal.as_ref()?;
        }
        if v.Anonymous.Anonymous.vt != VT_BSTR {
            return None;
        }
        let s: &BSTR = &v.Anonymous.Anonymous.Anonymous.bstrVal;
        let units: &[u16] = s;
        Some(units.to_vec())
    }
}

impl DWebBrowserEvents2_Impl for TabSink_Impl {}

impl IDispatch_Impl for TabSink_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }

    fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }

    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        _names: *const PCWSTR,
        _count: u32,
        _lcid: u32,
        _dispids: *mut i32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(
        &self,
        dispid: i32,
        _riid: *const GUID,
        _lcid: u32,
        _flags: DISPATCH_FLAGS,
        _params: *const DISPPARAMS,
        _result: *mut VARIANT,
        _excepinfo: *mut EXCEPINFO,
        _argerr: *mut u32,
    ) -> windows::core::Result<()> {
        crate::diag::count(dispid);
        match dispid {
            DISPID_NAVIGATECOMPLETE2 | DISPID_DOCUMENTCOMPLETE => {
                if dispid == DISPID_NAVIGATECOMPLETE2 {
                    // SAFETY: the DISPPARAMS COM passed for this call.
                    let url = unsafe { url_argument(_params) }.unwrap_or_default();
                    self.urls
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .entry(self.id)
                        .or_default()
                        .push_back(url);
                }
                // SAFETY: posting to the source's window; a destroyed window just fails.
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(self.window as *mut c_void)),
                        WM_TAB_EVENT,
                        WPARAM(dispid as usize),
                        LPARAM(self.id as isize),
                    )
                };
            }
            DISPID_ONQUIT => mark_dirty(self.window, &self.dirty),
            _ => {}
        }
        Ok(())
    }
}

/// Sets the dirty flag and, if it was clear, posts one wake-up message.
fn mark_dirty(window: isize, dirty: &AtomicBool) {
    if !dirty.swap(true, Ordering::SeqCst) {
        // SAFETY: posting to the source's window; a destroyed window just fails.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(window as *mut c_void)),
                WM_SHELL_WINDOWS_CHANGED,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

/// A hidden top-level window: unlike a message-only window it receives the
/// `TaskbarCreated` broadcast.
fn create_window() -> Result<HWND, ShellError> {
    // SAFETY: standard class registration (a repeat registration fails
    // harmlessly) and creation of a hidden window owned by this thread.
    unsafe {
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_CREATED.store(taskbar_created, Ordering::SeqCst);
        let module = GetModuleHandleW(None).map_err(|e| win_error("GetModuleHandleW", &e))?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            WINDOW_CLASS,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )
        .map_err(|e| win_error("CreateWindowExW(ShellWindowsSource)", &e))?;
        if taskbar_created != 0 {
            let _ = ChangeWindowMessageFilterEx(window, taskbar_created, MSGFLT_ALLOW, None);
        }
        Ok(window)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let source = || active_source().filter(|i| i.owns_window(hwnd));
    match msg {
        WM_SHELL_WINDOWS_CHANGED => {
            if let Some(inner) = source() {
                inner.on_shell_windows_changed();
            }
            LRESULT(0)
        }
        WM_TAB_EVENT => {
            if let Some(inner) = source() {
                inner.on_tab_event(wparam.0 as i32, lparam.0 as u64);
            }
            LRESULT(0)
        }
        WM_SHELL_PROCESS_EXITED => {
            if let Some(inner) = source() {
                inner.on_shell_process_exited(wparam.0 as u32);
            }
            LRESULT(0)
        }
        WM_REMOTE_ADVISE_FAILED => {
            if let Some(inner) = source() {
                inner.on_remote_advise_failed(lparam.0 as u64);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = wparam.0;
            if id != TIMER_POLL {
                // SAFETY: our own window's one-shot timer.
                let _ = unsafe { KillTimer(Some(hwnd), id) };
            }
            if let Some(inner) = source() {
                match id {
                    TIMER_POLL => inner.on_poll(),
                    TIMER_RESYNC => inner.on_resync_timer(),
                    id if id >= TIMER_LATE_SINK_BASE => {
                        inner.on_late_sink_check((id - TIMER_LATE_SINK_BASE) as u64)
                    }
                    id if id >= TIMER_DOCUMENT_BASE => {
                        inner.document_ready((id - TIMER_DOCUMENT_BASE) as u64)
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::SeqCst) => {
            if let Some(inner) = source() {
                inner.on_taskbar_created();
            }
            LRESULT(0)
        }
        // SAFETY: default processing for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// A `VT_I4` VARIANT. It owns no resources, so it needs no `VariantClear`.
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

fn error(code: i32, message: String) -> ShellError {
    ShellError {
        code,
        message: format!("shell windows: {message}"),
    }
}

fn win_error(step: &str, e: &windows::core::Error) -> ShellError {
    error(e.code().0, format!("{step}: {}", e.message()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_core::model::{LocationKind, Volume, WideString};
    use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW};

    use ConnectionAction::*;

    fn loc(name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: WideString(name.encode_utf16().collect()),
            display_name: WideString(name.encode_utf16().collect()),
            pidl: vec![0, 0],
            search: None,
        }
    }

    #[test]
    fn connection_loss_reports_once_and_reconnects_by_poll_or_taskbar() {
        let mut c = Connection::default();
        assert_eq!(c, Connection::Detached);
        assert_eq!(c.connected(), vec![]);
        assert_eq!(c, Connection::Attached);
        // Polling only matters while lost.
        assert_eq!(c.poll_tick(true), vec![]);

        assert_eq!(c.disconnected(), vec![Teardown, ReportLost, StartPolling]);
        assert_eq!(c, Connection::Lost { reported: true });
        // A second disconnection signal (process wait + RPC error) is not reported again.
        assert_eq!(c.disconnected(), vec![]);
        assert_eq!(c.poll_tick(false), vec![]);
        assert_eq!(c.poll_tick(true), vec![Reconnect]);
        assert_eq!(c.taskbar_created(), vec![Reconnect]);
        // A failed reconnection keeps polling.
        assert_eq!(c.connect_failed(), vec![]);
        assert_eq!(c, Connection::Lost { reported: true });
        assert_eq!(c.connected(), vec![StopPolling, ReportAvailable]);
        assert_eq!(c, Connection::Attached);
    }

    #[test]
    fn connection_waits_for_a_shell_missing_at_attach() {
        let mut c = Connection::Detached;
        assert_eq!(c.connect_failed(), vec![StartPolling]);
        assert_eq!(c, Connection::Lost { reported: false });
        // Nothing was lost, so nothing more is torn down or reported.
        assert_eq!(c.disconnected(), vec![]);
        assert_eq!(c.poll_tick(true), vec![Reconnect]);
        assert_eq!(c.connected(), vec![StopPolling, ReportAvailable]);
    }

    #[test]
    fn connection_signals_while_attached_or_detached() {
        let c = Connection::Attached;
        assert_eq!(c.taskbar_created(), vec![CheckHealth]);
        let mut d = Connection::Detached;
        assert_eq!(d.taskbar_created(), vec![]);
        assert_eq!(d.poll_tick(true), vec![]);
        assert_eq!(d.disconnected(), vec![]);
        assert_eq!(d, Connection::Detached);
    }

    #[test]
    fn disconnect_codes() {
        for c in [
            0x8001_0108_u32,
            0x8004_01FD,
            0x8007_06BA,
            0x8007_06BF,
            0x8001_0012,
        ] {
            assert!(is_disconnect(HRESULT(c as i32)), "{c:#x}");
        }
        // Busy, rejected, or failed calls are retried, not a lost shell.
        for c in [0u32, 0x8000_4005, 0x8007_0057, 0x8001_0001, 0x8001_010A, 0x8007_06BE] {
            assert!(!is_disconnect(HRESULT(c as i32)), "{c:#x}");
        }
    }

    #[test]
    fn diff_finds_gone_and_added_entries() {
        let (gone, added) = diff_entries(&[1, 2, 3], &[3, 4, 1, 4], true);
        assert_eq!(gone, vec![2]);
        assert_eq!(added, vec![4]);
        // An incomplete enumeration never revokes.
        let (gone, added) = diff_entries(&[1, 2, 3], &[5], false);
        assert!(gone.is_empty());
        assert_eq!(added, vec![5]);
        let (gone, added) = diff_entries(&[], &[], true);
        assert!(gone.is_empty() && added.is_empty());
    }

    #[test]
    fn holes_are_stable_once_seen_twice_while_the_collection_grows() {
        // No previous view: never stable.
        assert!(!holes_stable(None, 5, &[0, 1]));
        // Same count, same holes (the former rule).
        assert!(holes_stable(Some(&(5, vec![0, 1])), 5, &[0, 1]));
        // Grown (tabs appended), the old holes unchanged.
        assert!(holes_stable(Some(&(5, vec![0, 1])), 9, &[0, 1]));
        // A new hole beyond the previous count may be a registering tab.
        assert!(!holes_stable(Some(&(5, vec![0, 1])), 9, &[0, 1, 6]));
        // ... and counts once it is seen again.
        assert!(holes_stable(Some(&(9, vec![0, 1, 6])), 10, &[0, 1, 6]));
        // A hole filled or a new hole in the prefix: not stable.
        assert!(!holes_stable(Some(&(5, vec![0, 1])), 5, &[0]));
        assert!(!holes_stable(Some(&(5, vec![0, 1])), 6, &[0, 1, 3]));
        // The collection shrank.
        assert!(!holes_stable(Some(&(5, vec![0, 1])), 4, &[0, 1]));
    }

    #[test]
    fn initial_phase_claimed_window_is_done() {
        let a = loc("A");
        let mut asked = None;
        let (phase, claimed) = InitialPhase::AwaitNavigation
            .navigated(&ObservedLocation::Known(a.clone()), |l| {
                asked = Some(l.clone());
                Some(RequestId(7))
            });
        assert_eq!(phase, InitialPhase::Done);
        assert_eq!(claimed, Some(RequestId(7)));
        assert_eq!(asked, Some(a));
        assert_eq!(phase.document_ready(), (InitialPhase::Done, None));
    }

    #[test]
    fn initial_phase_unclaimed_window_announces_at_document_complete() {
        let a = loc("A");
        let (phase, claimed) =
            InitialPhase::AwaitNavigation.navigated(&ObservedLocation::Known(a.clone()), |_| None);
        assert_eq!(claimed, None);
        assert_eq!(phase, InitialPhase::AwaitDocument(a.clone()));
        // Later navigations before DocumentComplete keep the initial Location.
        let (phase, claimed) = phase.navigated(&ObservedLocation::Known(loc("B")), |_| {
            panic!("claim asked twice")
        });
        assert_eq!(claimed, None);
        let (phase, announce) = phase.document_ready();
        assert_eq!(phase, InitialPhase::Done);
        assert_eq!(announce, Some(a));
    }

    #[test]
    fn initial_phase_without_identifier_or_navigation() {
        let (phase, _) = InitialPhase::AwaitNavigation
            .navigated(&ObservedLocation::Unidentifiable, |_| panic!("no claim"));
        assert_eq!(phase, InitialPhase::Done);
        let (phase, _) = InitialPhase::AwaitNavigation
            .navigated(&ObservedLocation::NotNavigated, |_| panic!("no claim"));
        assert_eq!(phase, InitialPhase::AwaitNavigation);
        assert_eq!(
            InitialPhase::AwaitNavigation.document_ready(),
            (InitialPhase::AwaitNavigation, None)
        );
        let (phase, claimed) =
            InitialPhase::Done.navigated(&ObservedLocation::Known(loc("A")), |_| panic!("no claim"));
        assert_eq!((phase, claimed), (InitialPhase::Done, None));
    }

    /// Drains the calling thread's queue and returns the `(msg, wParam,
    /// lParam)` of messages for `window`.
    fn take_messages(window: HWND) -> Vec<(u32, usize, isize)> {
        let mut out = Vec::new();
        let mut msg = MSG::default();
        // SAFETY: standard PeekMessage loop on this thread.
        while unsafe { PeekMessageW(&mut msg, Some(window), 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message >= WM_APP {
                out.push((msg.message, msg.wParam.0, msg.lParam.0));
            }
        }
        out
    }

    fn invoke(dispatch: &IDispatch, dispid: i32) {
        let params = DISPPARAMS::default();
        // SAFETY: an in-process call with empty parameters.
        unsafe {
            dispatch
                .Invoke(
                    dispid,
                    &GUID::zeroed(),
                    0,
                    DISPATCH_FLAGS(1),
                    &params,
                    None,
                    None,
                    None,
                )
                .expect("Invoke");
        }
    }

    #[test]
    fn url_argument_reads_a_bstr_by_reference_or_by_value() {
        let path: Vec<u16> = r"C:\Temp\x".encode_utf16().collect();
        let mut inner = VARIANT::from(BSTR::from_wide(&path));
        // [pDisp, URL] in reverse order: URL first.
        let mut args = [
            VARIANT {
                Anonymous: VARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                        vt: VARENUM(VT_BYREF.0 | VT_VARIANT.0),
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: VARIANT_0_0_0 { pvarVal: &mut inner },
                    }),
                },
            },
            VARIANT::default(),
        ];
        let params = DISPPARAMS { rgvarg: args.as_mut_ptr(), cArgs: 2, ..Default::default() };
        // SAFETY: valid DISPPARAMS for the duration of the call.
        assert_eq!(unsafe { url_argument(&params) }, Some(path.clone()));
        let mut by_value = [VARIANT::from(BSTR::from_wide(&path))];
        let params = DISPPARAMS { rgvarg: by_value.as_mut_ptr(), cArgs: 1, ..Default::default() };
        // SAFETY: as above.
        assert_eq!(unsafe { url_argument(&params) }, Some(path));
        let mut number = [VARIANT::from(5i32)];
        let params = DISPPARAMS { rgvarg: number.as_mut_ptr(), cArgs: 1, ..Default::default() };
        // SAFETY: as above.
        assert_eq!(unsafe { url_argument(&params) }, None);
        // SAFETY: null and empty parameters are tolerated.
        assert_eq!(unsafe { url_argument(std::ptr::null()) }, None);
        assert_eq!(unsafe { url_argument(&DISPPARAMS::default()) }, None);
        // The by-reference VARIANT does not own `inner`.
        std::mem::forget(args);
    }

    #[test]
    fn sinks_answer_their_dispinterfaces_and_post_to_the_source_window() {
        let window = create_window().expect("source window");
        let dirty = Arc::new(AtomicBool::new(false));

        let shell: DShellWindowsEvents = ShellWindowsSink {
            window: window.0 as isize,
            dirty: dirty.clone(),
        }
        .into();
        let shell: IDispatch = shell.cast().expect("IDispatch");
        let _: DShellWindowsEvents = shell.cast().expect("DShellWindowsEvents");
        // Registrations coalesce into one wake-up until the flag is cleared.
        invoke(&shell, DISPID_WINDOWREGISTERED);
        invoke(&shell, DISPID_WINDOWREVOKED);
        invoke(&shell, 1234);
        assert!(dirty.load(Ordering::SeqCst));
        assert_eq!(take_messages(window), vec![(WM_SHELL_WINDOWS_CHANGED, 0, 0)]);

        dirty.store(false, Ordering::SeqCst);
        let urls: Arc<Mutex<HashMap<u64, VecDeque<Vec<u16>>>>> = Arc::default();
        let tab: DWebBrowserEvents2 = TabSink {
            window: window.0 as isize,
            id: 42,
            dirty: dirty.clone(),
            urls: urls.clone(),
        }
        .into();
        let tab: IDispatch = tab.cast().expect("IDispatch");
        let _: DWebBrowserEvents2 = tab.cast().expect("DWebBrowserEvents2");
        invoke(&tab, DISPID_NAVIGATECOMPLETE2);
        invoke(&tab, DISPID_DOCUMENTCOMPLETE);
        invoke(&tab, 105); // DISPID_STATUSTEXTCHANGE: ignored
        invoke(&tab, DISPID_ONQUIT);
        assert_eq!(
            take_messages(window),
            vec![
                (WM_TAB_EVENT, DISPID_NAVIGATECOMPLETE2 as usize, 42),
                (WM_TAB_EVENT, DISPID_DOCUMENTCOMPLETE as usize, 42),
                (WM_SHELL_WINDOWS_CHANGED, 0, 0),
            ]
        );
        // One (empty: no URL argument) entry per NavigateComplete2.
        assert_eq!(urls.lock().unwrap().get(&42).map(|q| q.len()), Some(1));
        // SAFETY: created above on this thread.
        unsafe { DestroyWindow(window) }.expect("DestroyWindow");
    }

    #[derive(Default)]
    struct Recorder {
        events: RefCell<Vec<Event>>,
    }

    impl ShellSourceHandler for Recorder {
        fn post(&self, event: Event) {
            self.events.borrow_mut().push(event);
        }
        fn window_changed(&self, _win: WinKey) {}
    }

    #[test]
    fn a_detached_source_reports_nothing_and_fails_queries() {
        let rec = Rc::new(Recorder::default());
        let mut source = ShellWindowsSource::new(WindowKeys::new(), TabKeys::new(), rec.clone());
        assert_eq!(source.connection(), Connection::Detached);
        assert!(source.windows().is_err());
        assert!(source.tabs_of(WinKey { hwnd: 1, seq: 1 }).is_err());
        source.shell_disconnected();
        source.on_taskbar_created();
        source.detach();
        assert_eq!(source.connection(), Connection::Detached);
        assert!(rec.events.borrow().is_empty());
    }
}
