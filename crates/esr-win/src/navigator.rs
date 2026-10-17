//! `ShellNavigator`: window open/hide/reveal, tab create/navigate/close,
//! selection, placement, z-order, and focus.
//!
//! Requirements 5.5, 5.6, 5.7, 5.10, 6.2, 6.4, 7.2, 7.7, 7.9. Strategy
//! selections come from [`crate::strategy`] (spikes R1–R9).
//!
//! # Opening hidden windows (R1, Req 7.2, 7.7)
//!
//! [`open_window`](ShellNavigator::open_window) runs
//! `ShellExecuteExW("opennewwindow", SEE_MASK_IDLIST, SW_HIDE)` on a
//! short-lived launcher STA thread, so the navigator's thread keeps pumping
//! messages and windows open in parallel
//! ([`OPEN_WINDOWS_IN_PARALLEL`](crate::strategy::OPEN_WINDOWS_IN_PARALLEL)).
//! Explorer honours `SW_HIDE`: the window is created invisible and stays so
//! while its tabs are created, navigated and activated
//! ([`HideStrategy::LaunchHidden`]). Nothing has to reach the window before
//! its first frame. (The former [`HideStrategy::LayeredAlpha`], which made
//! the window transparent at `EVENT_OBJECT_CREATE`, lost that race under
//! load: the event fires only when `CreateWindowEx` returns, and Explorer
//! can show the window before that, task 16.3.)
//!
//! While an open request is pending, the navigator holds an out-of-context
//! `EVENT_OBJECT_CREATE` hook. In that callback, a new top-level
//! `CabinetWClass` window is assigned to the oldest unassigned request
//! (FIFO) and recorded as hidden; with `LayeredAlpha` it is also made
//! transparent (`WS_EX_LAYERED`, alpha 0) and moved to its final restored
//! bounds.
//!
//! Window creation order need not match request order, and a window opened
//! by the user can appear while requests are pending. So the assignment is
//! only provisional. Once the new window's initial Location is known, the
//! shell window source calls [`claim`](ShellNavigator::claim). `claim`
//! matches the window FIFO by (pending request, initial Location), swaps
//! placements between hidden windows when needed, and releases a hidden
//! window that matches no request. A released window gets its original
//! placement back (if the navigator moved it) and is made visible. Requests
//! that are not claimed within [`OPEN_REQUEST_EXPIRY_MS`] are dropped, and
//! their windows are released; a window that turns up later for a dropped
//! request is released when it is claimed ([`STRAY_WINDOW_MS`]).
//!
//! The final placement is applied at [`reveal`](ShellNavigator::reveal), in
//! one `SetWindowPlacement` (state included, `SW_SHOWMINNOACTIVE` +
//! `WPF_RESTORETOMAXIMIZED` for minimized windows), which makes a window
//! launched hidden visible. Its first frame is therefore at the final
//! placement. Placements applied while the window is hidden use `SW_HIDE`
//! ([`PlacementPhase::Invisible`]). With `LayeredAlpha` the window stays
//! normal at its final restored bounds until `reveal` re-applies the final
//! placement while it is still transparent, then sets alpha 255 and removes
//! `WS_EX_LAYERED`.
//!
//! # Tabs (R2/R4/R9, Req 6.2, 7.9)
//!
//! * [`create_tab`](ShellNavigator::create_tab) posts `WM_COMMAND`
//!   [`NEW_TAB_COMMAND`] to the top-level window
//!   ([`TabCreateStrategy::WmCommand`]). With
//!   [`TabCreationPipelining::Sequential`], one command at a time may be
//!   outstanding per window. A command is settled when a new
//!   `ShellTabWindowClass` child appears or
//!   [`tab_registered`](ShellNavigator::tab_registered) is called. If no tab
//!   appears within [`NEW_TAB_REPOST_AFTER_MS`], the command is posted once
//!   more. If that also produces no tab, the UIA `AddButton` is invoked
//!   ([`TabCreateStrategy::FALLBACK`]). If none of these produce a tab,
//!   [`NavigatorSignal::TabCreateFailed`] is reported.
//! * [`navigate`](ShellNavigator::navigate) calls `IWebBrowser2::Navigate2`
//!   with the PIDL as a `VT_ARRAY | VT_UI1` SAFEARRAY and [`NAV_NO_HISTORY`],
//!   passing empty `VARIANT`s for the other arguments. The SAFEARRAY is
//!   deliberately leaked: `SafeArrayDestroy` after `Navigate2` corrupted the
//!   heap in R2. A search is navigated to the native search-folder PIDL of a
//!   freshly built `search-ms:` URL (see [`location_pidl`]); the plain
//!   Internet-URL PIDL of that URL would open a new window instead.
//! * [`close_tab`](ShellNavigator::close_tab) invokes the tab's UIA
//!   "Close Tab" button ([`TabCloseStrategy::UiaCloseButton`]). UIA tab items
//!   are not linked to HWNDs, so the navigator tracks each window's tab
//!   HWNDs in visual order. New tabs are always appended rightmost (R2, R9).
//!   The UIA item at the tab's position is closed, once the strip has exactly
//!   as many items as there are tabs. The strip lags behind tab creation, so
//!   this is re-read with backoff for up to [`UIA_CLOSE_CATCH_UP_MS`]. The
//!   items come from the strip's `ItemContainerPattern`, which also lists the
//!   items an overflowing strip virtualises; the item is realized before its
//!   close button is looked up ([`crate::uia_strip`]).
//!   [`TabCloseStrategy::Quit`] calls `IWebBrowser2::Quit` from this
//!   (pumping) STA instead.
//!
//! UIA calls never run on the navigator's thread. A private MTA worker
//! thread runs them, and their failures arrive as [`NavigatorSignal`]s.
//!
//! With [`crate::browser_worker`]s turned on (on by default), the sink of a
//! tab the navigator creates is attached, the tab is navigated, and the
//! window's next new-tab command is posted by the window's browser worker
//! (with late sinks, an opt-in that is off by default, the sink is attached
//! right after the navigation, overlapping the next command); a failed
//! `Navigate2` arrives as
//! [`NavigatorSignal::NavigateFailed`].
//!
//! # Selection, z-order, focus (Req 5.7, 5.10, 6.3, 6.4)
//!
//! * [`select_items`](ShellNavigator::select_items) calls
//!   `IShellView::SelectItem`. The first item gets `SVSI_SELECT |
//!   SVSI_DESELECTOTHERS | SVSI_ENSUREVISIBLE | SVSI_FOCUSED`, and the
//!   others get `SVSI_SELECT`.
//! * [`set_z_order`](ShellNavigator::set_z_order) calls
//!   `SetWindowPos(HWND_TOP, SWP_NOACTIVATE)` from the last window to the
//!   first. `order[0]` ends up topmost, and each window lies directly under
//!   its predecessor.
//! * [`focus`](ShellNavigator::focus) sends a zero-effect `SendInput`, as
//!   PowerToys does, and then calls `SetForegroundWindow`. If the window
//!   still is not in the foreground, it falls back to z-order only
//!   ([`FocusOutcome::ZOrderOnly`]).
//!
//! # Windows the navigator did not open (Tab folding, Req 14.6, 14.7, 14.13, 14.21)
//!
//! A Fold_Target or Matching_Tab belongs to the user, not to a restoration.
//! `select_items`, `close_tab`, `focus`, `apply_placement` and `reveal` take
//! only keys and look the live window up themselves, so they work on any
//! Explorer window or tab (a window that was not hidden by the navigator is
//! left as it is by `reveal`). `apply_placement` with the pre-minimize
//! placement of a minimized target restores it to normal at its restored
//! bounds, or maximized. The late tab of a stopped fold is closed after its
//! window appeared and before it registered, so no strip item can be matched
//! by name; only the rightmost item may then be closed
//! ([`unnamed_close_allowed`]).
//!
//! # Threading
//!
//! The navigator belongs to the thread that created it (`!Send`), normally
//! the Engine STA. That thread must pump messages: the WinEvent callback,
//! the repost timer, and the asynchronous results all arrive through it. At
//! most one navigator can be started per thread. No internal borrow is held
//! across a COM or cross-process Win32 call. Re-entrant callbacks are
//! therefore safe, and the [`NavigatorHandler`] may call back into the
//! navigator.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use esr_core::engine::{ItemId, RequestId, ShellError, TabKey, WinKey};
use esr_core::model::{LocationId, LocationKind, ShowState, WideString};
use esr_core::plan::placement::FinalPlacement;
use esr_core::search::build_search_ms;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RedrawWindow,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_ARRAY, VT_UI1,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    SetWinEventHook, TreeScope_Children, TreeScope_Descendants, UIA_AutomationIdPropertyId,
    UIA_ButtonControlTypeId, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
    UIA_InvokePatternId, UIA_TabItemControlTypeId, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_MOUSE, SendInput};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ILFindLastID, IShellWindows, IWebBrowser2, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHParseDisplayName, SVSI_DESELECTOTHERS,
    SVSI_ENSUREVISIBLE, SVSI_FOCUSED, SVSI_SELECT, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, EVENT_OBJECT_CREATE,
    EnumChildWindows, GA_ROOT, GWL_EXSTYLE, GetAncestor, GetForegroundWindow,
    GetLayeredWindowAttributes, GetWindowLongPtrW, GetWindowPlacement, HWND_MESSAGE, HWND_TOP,
    IsWindow, KillTimer, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, OBJID_WINDOW, PostMessageW,
    RegisterClassW, SHOW_WINDOW_CMD, SW_HIDE, SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetLayeredWindowAttributes, SetTimer,
    SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, WINDOW_EX_STYLE, WINDOW_STYLE,
    WINDOWPLACEMENT, WINDOWPLACEMENT_FLAGS, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP,
    SC_CLOSE, WM_COMMAND, WM_SYSCOMMAND, WM_TIMER, WNDCLASSW, WPF_ASYNCWINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED, WS_EX_LAYERED,
};
use windows::core::{BOOL, Interface, PCWSTR, w};

use crate::keys::{TabKeys, WindowKeys, hwnd_of, hwnd_value};
use crate::location::pidl_is_well_formed;
use crate::shell_util::{
    SHELL_TAB_WINDOW_CLASS, StaGuard, is_cabinet_window, is_internet_url_pidl, is_search_ms_name,
    parse_bind_ctx, pidl_to_bytes, shell_browser,
    shell_windows, tab_hwnd, window_class_name,
};
use crate::strategy::{
    HideStrategy, NAV_NO_HISTORY, NEW_TAB_COMMAND, NEW_TAB_REPOST_AFTER_MS, Strategies,
    TabCloseStrategy, TabCreateStrategy, TabCreationPipelining, UIA_ADD_TAB_AUTOMATION_ID,
    UIA_EXCLUDED_ITEM_CLASS, UIA_TAB_ITEM_CLASS, UIA_TRANSIENT_ERROR,
};
use crate::uia_strip;

/// An open request whose window has not been claimed after this long is
/// dropped, and a window hidden for it is released. This is longer than the
/// core's 5 s open timeout, so the core decides first.
pub const OPEN_REQUEST_EXPIRY_MS: u64 = 10_000;

/// How long a dropped `LaunchHidden` request is remembered so that its
/// window, should it still turn up, is shown rather than left invisible.
pub const STRAY_WINDOW_MS: u64 = 60_000;

/// How long a UIA `AddButton` invoke may take to produce a tab (the button
/// takes about 430 ms, R2).
pub const UIA_ADD_TAB_WAIT_MS: u64 = 2_500;

/// How long the UIA worker waits for a closed tab's window to disappear.
pub const UIA_CLOSE_WAIT_MS: u64 = 3_000;

/// How long a UIA close waits for the tab strip to catch up with the
/// window's tab count (it lags behind tab creation). With
/// [`UIA_CLOSE_WAIT_MS`] this stays within the core's 5 s close timeout.
pub const UIA_CLOSE_CATCH_UP_MS: u64 = 1_500;

/// Number of `WM_COMMAND` posts before the UIA fallback (the first post and
/// one re-post after [`NEW_TAB_REPOST_AFTER_MS`]).
pub const NEW_TAB_MAX_POSTS: u32 = 2;

/// Period of the navigator's housekeeping timer (re-posts, request expiry).
const TICK_MS: u32 = 100;

/// Attempts at `ElementFromHandle` while it fails with [`UIA_TRANSIENT_ERROR`].
const UIA_ELEMENT_ATTEMPTS: u32 = 10;

/// Posted to the navigator's message window when asynchronous results are queued.
const WM_NAV_ASYNC: u32 = WM_APP + 0x61;

const TIMER_ID: usize = 1;

const MESSAGE_WINDOW_CLASS: PCWSTR = w!("ESR.ShellNavigator");

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Asynchronous outcomes the navigator reports on its own thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavigatorSignal {
    /// `ShellExecuteExW` failed for an open request. The request was dropped.
    OpenFailed {
        request: RequestId,
        error: ShellError,
    },
    /// A new-tab command produced no tab in time and was retried.
    TabCreateRetried {
        win: WinKey,
        /// 2 for the re-post, 3 for the UIA fallback.
        attempt: u32,
        strategy: TabCreateStrategy,
    },
    /// Every new-tab attempt failed or produced no tab.
    TabCreateFailed { win: WinKey, error: ShellError },
    /// Closing a tab through UIA failed.
    CloseFailed { tab: TabKey, error: ShellError },
    /// A navigation issued by a browser worker failed.
    NavigateFailed { tab: TabKey, error: ShellError },
    /// The asynchronous z-order change of [`ShellNavigator::set_z_order`]
    /// failed for a window (the first failure is reported).
    ZOrderFailed { error: ShellError },
    /// The new `ShellTabWindowClass` child of an outstanding new-tab command
    /// of `win` appeared (3–53 ms after the command in F2; the tab registers
    /// in ShellWindows later). Reported once per command, at the moment the
    /// command is settled, whoever issued it; the caller maps `win` to the
    /// `CreateTab` op and posts `Event::TabStarted { op, tab }` (Req 14.1,
    /// 14.13). Only sequential tab creation tracks commands.
    TabStarted { win: WinKey, tab: TabKey },
}

/// Receives [`NavigatorSignal`]s on the navigator's thread.
pub trait NavigatorHandler {
    fn on_signal(&self, signal: NavigatorSignal);
}

impl<F: Fn(NavigatorSignal)> NavigatorHandler for F {
    fn on_signal(&self, signal: NavigatorSignal) {
        self(signal)
    }
}

/// How [`ShellNavigator::focus`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FocusOutcome {
    /// The window is the foreground window.
    Foreground,
    /// The foreground could not be taken; the window was only raised.
    ZOrderOnly,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// When a placement is applied to a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlacementPhase {
    /// A transparent window under construction ([`HideStrategy::LayeredAlpha`]):
    /// normal show state at the final restored bounds, never activating.
    Hidden,
    /// A window launched hidden ([`HideStrategy::LaunchHidden`]): the final
    /// restored bounds with `SW_HIDE`, so it stays invisible.
    Invisible,
    /// The final placement, show state included.
    Final,
}

/// The `WINDOWPLACEMENT` for `placement` in `phase`: `rcNormalPosition` in
/// workspace coordinates of the target display. `Final` uses
/// `SW_SHOWNOACTIVATE` (normal), `SW_SHOWMAXIMIZED`, or `SW_SHOWMINNOACTIVE`
/// with `WPF_RESTORETOMAXIMIZED` when the pre-minimize state was maximized.
/// `None` if the bounds do not fit in workspace coordinates.
pub fn window_placement_for(
    placement: &FinalPlacement,
    phase: PlacementPhase,
) -> Option<WINDOWPLACEMENT> {
    let r = placement.restored_workspace()?;
    let (show, flags) = match (phase, placement.state) {
        (PlacementPhase::Invisible, _) => (SW_HIDE, WINDOWPLACEMENT_FLAGS(0)),
        (PlacementPhase::Hidden, _) | (PlacementPhase::Final, ShowState::Normal) => {
            (SW_SHOWNOACTIVATE, WINDOWPLACEMENT_FLAGS(0))
        }
        (PlacementPhase::Final, ShowState::Maximized) => {
            (SW_SHOWMAXIMIZED, WINDOWPLACEMENT_FLAGS(0))
        }
        (PlacementPhase::Final, ShowState::Minimized { restore_maximized }) => (
            SW_SHOWMINNOACTIVE,
            if restore_maximized {
                WPF_RESTORETOMAXIMIZED
            } else {
                WINDOWPLACEMENT_FLAGS(0)
            },
        ),
    };
    Some(WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        flags,
        showCmd: show.0 as u32,
        rcNormalPosition: RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        },
        ..Default::default()
    })
}

/// `IShellView::SelectItem` flags for the `index`-th item of a selection:
/// the first replaces the selection, is focused and scrolled into view; the
/// others are added.
pub fn select_flags(index: usize) -> u32 {
    if index == 0 {
        (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0 | SVSI_ENSUREVISIBLE.0 | SVSI_FOCUSED.0) as u32
    } else {
        SVSI_SELECT.0 as u32
    }
}

/// The order of `SetWindowPos(HWND_TOP)` calls that leaves `order[0]`
/// topmost and each window directly under its predecessor.
pub fn z_order_calls<T: Copy>(order: &[T]) -> Vec<T> {
    order.iter().rev().copied().collect()
}

/// One attempt at creating a tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CreateAttempt {
    /// The `n`-th `WM_COMMAND` [`NEW_TAB_COMMAND`] post (1-based).
    WmCommand(u32),
    /// Invoke the UIA `AddButton`.
    UiaAddButton,
}

impl CreateAttempt {
    /// The first attempt for `strategy`.
    pub fn first(strategy: TabCreateStrategy) -> Self {
        match strategy {
            TabCreateStrategy::WmCommand => CreateAttempt::WmCommand(1),
            TabCreateStrategy::UiaAddButton => CreateAttempt::UiaAddButton,
        }
    }

    /// The attempt after `self` produced no tab in time, or `None` when
    /// every attempt is used up: re-post once, then the
    /// [`TabCreateStrategy::FALLBACK`].
    pub fn next(self) -> Option<Self> {
        match self {
            CreateAttempt::WmCommand(n) if n < NEW_TAB_MAX_POSTS => {
                Some(CreateAttempt::WmCommand(n + 1))
            }
            CreateAttempt::WmCommand(_) => Some(CreateAttempt::first(TabCreateStrategy::FALLBACK))
                .filter(|a| !matches!(a, CreateAttempt::WmCommand(_))),
            CreateAttempt::UiaAddButton => None,
        }
    }

    /// How long to wait for a tab after this attempt.
    pub fn wait(self) -> Duration {
        match self {
            CreateAttempt::WmCommand(_) => Duration::from_millis(NEW_TAB_REPOST_AFTER_MS),
            CreateAttempt::UiaAddButton => Duration::from_millis(UIA_ADD_TAB_WAIT_MS),
        }
    }

    /// 1-based attempt number (for [`NavigatorSignal::TabCreateRetried`]).
    pub fn number(self) -> u32 {
        match self {
            CreateAttempt::WmCommand(n) => n,
            CreateAttempt::UiaAddButton => NEW_TAB_MAX_POSTS + 1,
        }
    }

    pub fn strategy(self) -> TabCreateStrategy {
        match self {
            CreateAttempt::WmCommand(_) => TabCreateStrategy::WmCommand,
            CreateAttempt::UiaAddButton => TabCreateStrategy::UiaAddButton,
        }
    }
}

/// The visual order of a window's tabs now: the tracked `order` without the
/// tabs that are gone, followed by live tabs that were not tracked (new tabs
/// are appended rightmost). Returns `(index of target, tab count)`, or
/// `None` if `target` is not live.
pub fn tab_position(order: &[u64], live: &[u64], target: u64) -> Option<(usize, usize)> {
    let mut now: Vec<u64> = order.iter().copied().filter(|t| live.contains(t)).collect();
    for t in live {
        if !now.contains(t) {
            now.push(*t);
        }
    }
    let index = now.iter().position(|&t| t == target)?;
    Some((index, now.len()))
}

/// Merges the live tabs of a window into its tracked visual order: gone tabs
/// are dropped, new ones appended. Several new tabs at once (rare with
/// sequential creation) are appended in reverse enumeration order, since a
/// newly created child window is enumerated first.
pub fn merge_tab_order(order: &mut Vec<u64>, live: &[u64]) {
    order.retain(|t| live.contains(t));
    for t in live.iter().rev() {
        if !order.contains(t) {
            order.push(*t);
        }
    }
}

/// The tab window a new-tab command produced: the first of `live` (a window's
/// tab HWNDs in `EnumChildWindows` order) that is not in `baseline`. When
/// several appeared at once, the earliest created one is returned: a newly
/// created child is enumerated first, so it is the last of the new ones
/// (the order [`merge_tab_order`] appends them in).
pub fn new_tab_among(baseline: &[u64], live: &[u64]) -> Option<u64> {
    live.iter().rev().copied().find(|t| !baseline.contains(t))
}

/// The `WM_SYSCOMMAND` `wParam` that closes a window with all of its tabs,
/// the caption's close button. (`WM_CLOSE`, sent tab by tab, crashed Explorer
/// in R9.)
pub const CLOSE_WINDOW_SYSCOMMAND: usize = SC_CLOSE as usize;

/// An `OpenWindow` request that has not been claimed yet.
#[derive(Clone, Debug)]
pub struct PendingOpen {
    pub request: RequestId,
    pub location: LocationId,
    pub placement: FinalPlacement,
    pub requested_at: Instant,
    /// The window provisionally assigned at `EVENT_OBJECT_CREATE`.
    pub window: Option<u64>,
}

/// Result of [`OpenQueue::claim`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claim {
    /// The claiming request and its placement. `placement_changed` is true
    /// when the window was hidden for another request (or none), so the
    /// placement must be applied again.
    pub claimed: Option<(RequestId, FinalPlacement, bool)>,
    /// Other hidden windows that now belong to a different request; their
    /// placement must be applied again.
    pub reassigned: Vec<(u64, FinalPlacement)>,
    /// No request matches the window's initial Location.
    pub unmatched: bool,
}

/// Pending open requests, assigned FIFO to new windows and claimed FIFO by
/// (pending request, initial Location).
#[derive(Debug, Default)]
pub struct OpenQueue {
    items: Vec<PendingOpen>,
}

impl OpenQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn push(&mut self, open: PendingOpen) {
        self.items.push(open);
    }

    pub fn remove(&mut self, request: RequestId) -> Option<PendingOpen> {
        let i = self.items.iter().position(|p| p.request == request)?;
        Some(self.items.remove(i))
    }

    /// The window provisionally assigned to `request`.
    pub fn assigned(&self, request: RequestId) -> Option<u64> {
        self.items
            .iter()
            .find(|p| p.request == request)
            .and_then(|p| p.window)
    }

    /// A new top-level window `hwnd` was created: assign it to the oldest
    /// request without a live window. Returns that request and its placement.
    pub fn on_created(
        &mut self,
        hwnd: u64,
        alive: impl Fn(u64) -> bool,
    ) -> Option<(RequestId, FinalPlacement)> {
        if self.items.iter().any(|p| p.window == Some(hwnd)) {
            return None;
        }
        for p in &mut self.items {
            if p.window.is_some_and(|w| !alive(w)) {
                // Explorer sometimes replaces a fresh window; the request is free again.
                p.window = None;
            }
        }
        let p = self.items.iter_mut().find(|p| p.window.is_none())?;
        p.window = Some(hwnd);
        Some((p.request, p.placement))
    }

    /// The initial Location of new window `hwnd` is known: claim the oldest
    /// request with that Location, preferring the one `hwnd` is assigned to,
    /// then requests without a live window, then requests whose (unclaimed)
    /// window is another one (the two windows swap requests).
    pub fn claim(
        &mut self,
        hwnd: u64,
        location: &LocationId,
        alive: impl Fn(u64) -> bool,
    ) -> Claim {
        let mine = self.items.iter().position(|p| p.window == Some(hwnd));
        let candidates: Vec<usize> = (0..self.items.len())
            .filter(|&i| self.items[i].location == *location)
            .collect();
        let mut out = Claim::default();
        let chosen = if let Some(i) = mine.filter(|i| candidates.contains(i)) {
            Some((i, false))
        } else if let Some(&j) = candidates
            .iter()
            .find(|&&j| self.items[j].window.is_none_or(|w| !alive(w)))
        {
            if let Some(i) = mine {
                self.items[i].window = None;
            }
            Some((j, true))
        } else if let Some(&j) = candidates.first() {
            let other = self.items[j]
                .window
                .expect("candidates without a window were handled");
            if let Some(i) = mine {
                self.items[i].window = Some(other);
                out.reassigned.push((other, self.items[i].placement));
            }
            Some((j, true))
        } else {
            if let Some(i) = mine {
                self.items[i].window = None;
            }
            out.unmatched = true;
            None
        };
        if let Some((j, changed)) = chosen {
            let p = self.items.remove(j);
            out.claimed = Some((p.request, p.placement, changed));
        }
        out
    }

    /// Removes and returns the requests older than `max_age`.
    pub fn expire(&mut self, now: Instant, max_age: Duration) -> Vec<PendingOpen> {
        let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|p| now.saturating_duration_since(p.requested_at) >= max_age);
        self.items = keep;
        old
    }
}

// ---------------------------------------------------------------------------
// ShellNavigator
// ---------------------------------------------------------------------------

/// The `ShellNavigator` adapter (backs the `Navigator` port in `esr-app`).
/// Bound to the thread that created it; see the module docs.
pub struct ShellNavigator {
    inner: Rc<Inner>,
}

struct Inner {
    strategies: Strategies,
    windows: WindowKeys,
    tabs: TabKeys,
    handler: Rc<dyn NavigatorHandler>,
    state: RefCell<State>,
    /// `EVENT_OBJECT_CREATE`s that arrived while `state` was borrowed.
    deferred: RefCell<Vec<u64>>,
}

#[derive(Default)]
struct State {
    started: bool,
    message_window: Option<HWND>,
    hook: Option<HWINEVENTHOOK>,
    timer: bool,
    asyncs: Option<Arc<AsyncQueue>>,
    uia: Option<Sender<UiaJob>>,
    opens: OpenQueue,
    /// Locations of `LaunchHidden` requests that were dropped before their
    /// window was seen. A window that turns up for one later is shown
    /// (released) instead of staying invisible.
    strays: Vec<(LocationId, Instant)>,
    /// Per top-level window HWND value.
    managed: HashMap<u64, Managed>,
    /// Outstanding new-tab command per top-level window HWND value.
    creates: HashMap<u64, PendingCreate>,
    /// Windows whose new-tab command was settled by the new tab's window
    /// appearing ([`Inner::tick`]) before the tab registered in ShellWindows.
    /// The tab is still the navigator's own until it registers, so
    /// [`NavigatorHandle::creating_tab`] stays true and the shell window
    /// source keeps skipping its cross-process lookups (task 16.3).
    registering: HashMap<u64, WinKey>,
    /// Tab HWND value → browser.
    browsers: HashMap<u64, IWebBrowser2>,
    /// Browsers handed over by the ShellWindows source at registration
    /// ([`NavigatorHandle::tab_browser`]); keyed by tab key, so no lookup
    /// in ShellWindows or HWND check is needed to navigate a new tab.
    source_browsers: HashMap<TabKey, IWebBrowser2>,
    /// The Location each tab was last navigated to, until its completion is
    /// reported ([`NavigatorHandle::expected_location`]).
    expected: HashMap<TabKey, LocationId>,
    shell_windows: Option<IShellWindows>,
    /// MTA threads for new tabs' sinks and navigations (task 16.3).
    workers: Option<crate::browser_worker::BrowserWorkers>,
}

/// What the navigator knows about one top-level window.
struct Managed {
    key: WinKey,
    /// Set while the window is hidden by the navigator.
    hidden: Option<Hidden>,
    /// The placement applied at reveal (windows the navigator opened, or
    /// placements applied while hidden).
    target: Option<FinalPlacement>,
    /// The window's placement when the navigator first touched it at
    /// `EVENT_OBJECT_CREATE`.
    original: Option<WINDOWPLACEMENT>,
    /// Claimed by an open request.
    claimed: bool,
    /// Tab HWND values in visual order.
    order: Vec<u64>,
}

impl Managed {
    fn new(key: WinKey) -> Self {
        Self {
            key,
            hidden: None,
            target: None,
            original: None,
            claimed: false,
            order: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct Hidden {
    /// Made transparent ([`HideStrategy::LayeredAlpha`]): alpha 255 is
    /// restored at reveal.
    transparent: bool,
    /// `WS_EX_LAYERED` was added by the navigator (and is removed at reveal).
    added_layered: bool,
    /// The navigator moved the window while hiding it (its original
    /// placement is restored when it is released).
    placed: bool,
}

impl Hidden {
    /// A window opened with `SW_HIDE` that the navigator has not touched.
    const LAUNCHED: Hidden = Hidden { transparent: false, added_layered: false, placed: false };

    /// The phase in which a placement is applied while the window is hidden.
    fn phase(self) -> PlacementPhase {
        if self.transparent {
            PlacementPhase::Hidden
        } else {
            PlacementPhase::Invisible
        }
    }
}

struct PendingCreate {
    key: WinKey,
    /// Live tab HWND values when the current attempt was made.
    baseline: Vec<u64>,
    attempt: CreateAttempt,
    deadline: Instant,
}

/// Results produced on other threads, drained on the navigator's thread.
struct AsyncQueue {
    /// HWND value of the navigator's message window.
    window: u64,
    queue: Mutex<VecDeque<AsyncResult>>,
}

enum AsyncResult {
    OpenFailed {
        request: RequestId,
        error: ShellError,
    },
    TabAdd {
        top: u64,
        result: Result<(), ShellError>,
    },
    TabClose {
        /// HWND value of the tab's top-level window.
        top: u64,
        tab: TabKey,
        /// The tab was closed before it registered in ShellWindows.
        unregistered: bool,
        result: Result<(), ShellError>,
    },
    NavigateFailed {
        tab: TabKey,
        error: ShellError,
    },
    ZOrderFailed {
        error: ShellError,
    },
}

impl AsyncQueue {
    fn push(&self, result: AsyncResult) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(result);
        // SAFETY: posting to a window of another thread is allowed; a destroyed
        // window just makes the call fail.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd_of(self.window)),
                WM_NAV_ASYNC,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }

    fn drain(&self) -> Vec<AsyncResult> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Weak<Inner>>> = const { RefCell::new(None) };
}

fn active_navigator() -> Option<Rc<Inner>> {
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

impl ShellNavigator {
    /// `windows` and `tabs` are the key registries shared with the other
    /// adapters, so keys agree across them.
    pub fn new(
        strategies: Strategies,
        windows: WindowKeys,
        tabs: TabKeys,
        handler: Rc<dyn NavigatorHandler>,
    ) -> Self {
        Self {
            inner: Rc::new(Inner {
                strategies,
                windows,
                tabs,
                handler,
                state: RefCell::new(State::default()),
                deferred: RefCell::new(Vec::new()),
            }),
        }
    }

    pub fn strategies(&self) -> Strategies {
        self.inner.strategies
    }

    pub fn is_started(&self) -> bool {
        self.inner.state.borrow().started
    }

    /// Creates the message window and the UIA worker. The calling thread must
    /// be an STA that pumps messages. Idempotent. Fails if another navigator
    /// is started on this thread.
    pub fn start(&mut self) -> Result<(), ShellError> {
        if self.is_started() {
            return Ok(());
        }
        if active_navigator().is_some() {
            return Err(error(
                0,
                "another ShellNavigator is started on this thread".into(),
            ));
        }
        let window = create_message_window()?;
        let asyncs = Arc::new(AsyncQueue {
            window: hwnd_value(window),
            queue: Mutex::new(VecDeque::new()),
        });
        let uia = match spawn_uia_worker(asyncs.clone()) {
            Ok(tx) => tx,
            Err(e) => {
                // SAFETY: created above on this thread.
                let _ = unsafe { DestroyWindow(window) };
                return Err(e);
            }
        };
        {
            let mut st = self.inner.state.borrow_mut();
            st.started = true;
            st.message_window = Some(window);
            st.asyncs = Some(asyncs);
            st.uia = Some(uia);
            // With workers off (`browser_worker::set_enabled(false)`; on by
            // default), sinks and navigations stay on this thread.
            st.workers = if crate::browser_worker::enabled() {
                crate::browser_worker::BrowserWorkers::start().ok()
            } else {
                None
            };
        }
        set_active(Some(&self.inner));
        Ok(())
    }

    /// Reveals every window still hidden, removes the hook and timer, and
    /// stops the UIA worker. Pending requests are dropped. Idempotent.
    pub fn stop(&mut self) {
        if !self.is_started() {
            return;
        }
        self.reveal_all();
        let (hook, window) = {
            let mut st = self.inner.state.borrow_mut();
            st.started = false;
            st.timer = false;
            st.opens = OpenQueue::new();
            st.creates.clear();
            st.browsers.clear();
            st.shell_windows = None;
            // Dropping the senders ends the workers after their current job.
            st.uia = None;
            st.workers = None;
            st.asyncs = None;
            (st.hook.take(), st.message_window.take())
        };
        if active_navigator().is_some_and(|a| Rc::ptr_eq(&a, &self.inner)) {
            set_active(None);
        }
        if let Some(hook) = hook {
            // SAFETY: installed by this navigator on this thread.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
        if let Some(window) = window {
            // SAFETY: created by this navigator on this thread; its timer goes with it.
            let _ = unsafe { DestroyWindow(window) };
        }
    }

    // -- windows ------------------------------------------------------------

    /// Opens a hidden window on `location`, placed at `placement` before its
    /// first frame. The window is assigned at `EVENT_OBJECT_CREATE` and
    /// claimed by [`claim`](Self::claim).
    pub fn open_window(
        &mut self,
        request: RequestId,
        location: &LocationId,
        placement: &FinalPlacement,
    ) -> Result<(), ShellError> {
        let asyncs = {
            let st = self.inner.state.borrow();
            match (&st.asyncs, st.started) {
                (Some(a), true) => a.clone(),
                _ => return Err(error(0, "open_window: navigator not started".into())),
            }
        };
        let pidl = location_pidl(location)?;
        self.inner.ensure_hook()?;
        self.inner.state.borrow_mut().opens.push(PendingOpen {
            request,
            location: location.clone(),
            placement: *placement,
            requested_at: Instant::now(),
            window: None,
        });
        self.inner.ensure_timer();
        let show = if self.inner.strategies.hide == HideStrategy::LaunchHidden {
            SW_HIDE
        } else {
            SW_SHOWNOACTIVATE
        };
        let spawned = std::thread::Builder::new()
            .name("esr-open-window".into())
            .spawn(move || {
                crate::diag::note(|| format!("nav: ShellExecute start req {}", request.0));
                if let Err(error) = shell_execute_open_new_window(&pidl, show) {
                    asyncs.push(AsyncResult::OpenFailed { request, error });
                }
                crate::diag::note(|| format!("nav: ShellExecute end req {}", request.0));
            });
        if let Err(e) = spawned {
            self.inner.state.borrow_mut().opens.remove(request);
            self.inner.update_hook_and_timer();
            return Err(error(0, format!("open_window: launcher thread: {e}")));
        }
        Ok(())
    }

    /// The window provisionally assigned to an unclaimed `request`.
    pub fn pending_assignment(&self, request: RequestId) -> Option<WinKey> {
        let hwnd = self.inner.state.borrow().opens.assigned(request)?;
        self.inner.windows.get(hwnd)
    }

    /// Number of unclaimed open requests.
    pub fn pending_opens(&self) -> usize {
        self.inner.state.borrow().opens.len()
    }

    /// The initial Location of new window `win` is known. Returns the open
    /// request that claims it, or `None` for a window the navigator did not
    /// request (a hidden one is then released, visible at its original
    /// placement).
    pub fn claim(&mut self, win: WinKey, initial: &LocationId) -> Option<RequestId> {
        self.inner.claim(win, initial)
    }

    /// A non-owning handle for callbacks that must reach the navigator
    /// while its owner may be in the middle of a call on it (for example
    /// the shell window source's `claim`, which can run from a message
    /// pumped during a COM call the owner made). The handle stops working
    /// once the navigator is dropped.
    pub fn handle(&self) -> NavigatorHandle {
        NavigatorHandle {
            inner: Rc::downgrade(&self.inner),
        }
    }

    /// Applies `placement` to `win` in one `SetWindowPlacement`. For a window
    /// the navigator is still hiding, the show state is completed at
    /// [`reveal`](Self::reveal).
    pub fn apply_placement(
        &mut self,
        win: WinKey,
        placement: &FinalPlacement,
    ) -> Result<(), ShellError> {
        let hwnd = self.inner.live_window(win)?;
        let hidden = {
            let mut st = self.inner.state.borrow_mut();
            let m = st.managed_entry(win);
            if let Some(h) = &mut m.hidden {
                m.target = Some(*placement);
                h.placed = true;
            }
            m.hidden
        };
        let phase = match hidden {
            Some(h) => h.phase(),
            None => PlacementPhase::Final,
        };
        set_placement(hwnd, placement, phase)
    }

    /// Makes a window hidden by the navigator visible at its final
    /// placement. A window that is not hidden is left as it is.
    pub fn reveal(&mut self, win: WinKey) -> Result<(), ShellError> {
        let hwnd = self.inner.live_window(win)?;
        self.inner.reveal_hwnd(hwnd)
    }

    /// Reveals every window the navigator still hides (for example after a
    /// failed restoration, which leaves the opened windows open). A hidden
    /// window that no request claimed is released at its original placement.
    pub fn reveal_all(&mut self) {
        let hidden: Vec<(u64, bool)> = {
            let st = self.inner.state.borrow();
            st.managed
                .iter()
                .filter(|(_, m)| m.hidden.is_some())
                .map(|(h, m)| (*h, m.claimed))
                .collect()
        };
        for (h, claimed) in hidden {
            if !is_alive(h) {
                self.inner.state.borrow_mut().managed.remove(&h);
            } else if claimed {
                let _ = self.inner.reveal_hwnd(hwnd_of(h));
            } else {
                self.inner.release(h);
            }
        }
    }

    /// Whether the navigator currently hides `win`.
    pub fn is_hidden(&self, win: WinKey) -> bool {
        self.inner
            .state
            .borrow()
            .managed
            .get(&win.hwnd)
            .is_some_and(|m| m.key == win && m.hidden.is_some())
    }

    /// The placement `win` had when the navigator took it over at
    /// `EVENT_OBJECT_CREATE`.
    pub fn original_placement(&self, win: WinKey) -> Option<WINDOWPLACEMENT> {
        let st = self.inner.state.borrow();
        st.managed
            .get(&win.hwnd)
            .filter(|m| m.key == win)
            .and_then(|m| m.original)
    }

    /// Forget a destroyed window.
    pub fn window_destroyed(&mut self, win: WinKey) {
        self.inner.window_destroyed(win);
    }

    /// Stacks the windows so that `order[0]` is topmost and each lies
    /// directly under its predecessor. The `SetWindowPos` calls run on a
    /// short-lived thread, one after the other, so the caller does not wait
    /// for the windows' threads (each call waits until the window's thread
    /// has handled it, 0.2–0.4 s in all right after a reveal; task 16.3).
    /// A window that cannot be found fails the call at once (nothing is
    /// changed); a failing `SetWindowPos` is reported as
    /// [`NavigatorSignal::ZOrderFailed`] (every window is attempted).
    pub fn set_z_order(&mut self, order: &[WinKey]) -> Result<(), ShellError> {
        let calls = z_order_calls(order)
            .into_iter()
            .map(|win| self.inner.live_window(win).map(hwnd_value))
            .collect::<Result<Vec<u64>, ShellError>>()?;
        let asyncs = self.inner.state.borrow().asyncs.clone();
        let spawned = std::thread::Builder::new().name("esr-zorder".into()).spawn(move || {
            let mut first_error = None;
            for hwnd in calls {
                if let (Err(e), None) = (raise(hwnd_of(hwnd)), &first_error) {
                    first_error = Some(e);
                }
            }
            if let (Some(error), Some(asyncs)) = (first_error, asyncs) {
                asyncs.push(AsyncResult::ZOrderFailed { error });
            }
        });
        spawned.map(|_| ()).map_err(|e| error(0, format!("z-order thread: {e}")))
    }

    /// Gives `win` the foreground: zero-effect `SendInput`, then
    /// `SetForegroundWindow`; falls back to raising it.
    pub fn focus(&mut self, win: WinKey) -> Result<FocusOutcome, ShellError> {
        let hwnd = self.inner.live_window(win)?;
        let input = [INPUT {
            r#type: INPUT_MOUSE,
            ..Default::default()
        }];
        // SAFETY: one zero-initialized mouse INPUT (no movement, no buttons):
        // it has no effect except counting as input for the foreground lock.
        unsafe { SendInput(&input, size_of::<INPUT>() as i32) };
        // SAFETY: plain call on a validated window.
        let _ = unsafe { SetForegroundWindow(hwnd) };
        // SAFETY: plain read.
        if unsafe { GetForegroundWindow() } == hwnd {
            return Ok(FocusOutcome::Foreground);
        }
        raise(hwnd)?;
        Ok(FocusOutcome::ZOrderOnly)
    }

    // -- tabs ---------------------------------------------------------------

    /// Appends one tab to `win` (see the module docs for re-posts and the
    /// UIA fallback). With sequential pipelining, fails while a previous
    /// new-tab command for `win` has not produced a tab.
    pub fn create_tab(&mut self, win: WinKey) -> Result<(), ShellError> {
        let hwnd = self.inner.live_window(win)?;
        let live = tab_children(hwnd);
        let sequential = self.inner.strategies.tab_pipelining == TabCreationPipelining::Sequential;
        {
            let mut st = self.inner.state.borrow_mut();
            merge_tab_order(&mut st.managed_entry(win).order, &live);
            if sequential && let Some(p) = st.creates.get(&win.hwnd) {
                if p.key == win && live.iter().all(|t| p.baseline.contains(t)) {
                    return Err(error(
                        0,
                        format!("create_tab: a new-tab command for {win:?} is still outstanding"),
                    ));
                }
                st.creates.remove(&win.hwnd);
            }
        }
        let attempt = CreateAttempt::first(self.inner.strategies.tab_create);
        self.inner.attempt_create(hwnd, win, attempt)?;
        if sequential {
            self.inner.state.borrow_mut().creates.insert(
                win.hwnd,
                PendingCreate {
                    key: win,
                    baseline: live,
                    attempt,
                    deadline: Instant::now() + attempt.wait(),
                },
            );
            self.inner.ensure_timer();
        }
        Ok(())
    }

    /// A new tab of `win` registered in ShellWindows: settles the outstanding
    /// new-tab command and appends the tab to the tracked order.
    pub fn tab_registered(&mut self, win: WinKey, tab: TabKey) {
        self.inner.tab_registered(win, tab);
    }

    /// Whether a new-tab command for `win` is outstanding.
    pub fn is_creating_tab(&self, win: WinKey) -> bool {
        self.inner
            .state
            .borrow()
            .creates
            .get(&win.hwnd)
            .is_some_and(|c| c.key == win)
    }

    /// Tab HWND values of `win` in tracked visual order.
    pub fn tab_order(&self, win: WinKey) -> Vec<u64> {
        let live = tab_children(hwnd_of(win.hwnd));
        let mut st = self.inner.state.borrow_mut();
        let order = &mut st.managed_entry(win).order;
        merge_tab_order(order, &live);
        order.clone()
    }

    /// Navigates `tab` to `location` without adding history. Does not wait
    /// for the navigation to complete (`NavigationCompleted` comes from the
    /// shell window source). A tab attached to a browser worker
    /// ([`NavigatorHandle::attach_browser`]) is navigated by the worker; its
    /// failure is reported as [`NavigatorSignal::NavigateFailed`].
    pub fn navigate(&mut self, tab: TabKey, location: &LocationId) -> Result<(), ShellError> {
        let (tab_hwnd, top) = self.inner.live_tab(tab)?;
        let pidl = location_pidl(location)?;
        let known = {
            let mut st = self.inner.state.borrow_mut();
            st.expected.retain(|t, _| is_alive(t.hwnd));
            st.expected.insert(tab, location.clone());
            st.source_browsers.get(&tab).cloned()
        };
        {
            let mut st = self.inner.state.borrow_mut();
            let asyncs = st.asyncs.clone();
            if let (Some(workers), Some(asyncs)) = (st.workers.as_mut(), asyncs) {
                // A tab the workers do not hold yet (the trigger's initial
                // tab, registered before the restoration) is handed to its
                // window's worker without a sink (the source attached its
                // own): its navigation then does not hold up this thread
                // (task 16.3: ~90 ms while Explorer starts building tabs).
                if !workers.has(tab)
                    && let Some(browser) = &known
                {
                    let _ = workers.attach(hwnd_value(top), tab, browser, None, Box::new(|_| {}));
                }
                if workers.has(tab) {
                    let failed: crate::browser_worker::OnFailure =
                        Box::new(move |error| asyncs.push(AsyncResult::NavigateFailed { tab, error }));
                    if workers.navigate(tab, pidl.clone(), failed) {
                        return Ok(());
                    }
                }
            }
        }
        let browser = match known {
            Some(b) => b,
            None => self.inner.browser_of(tab_hwnd)?,
        };
        navigate_pidl(&browser, &pidl)
    }

    /// Closes the whole window `win` with all of its tabs: posts
    /// `WM_SYSCOMMAND SC_CLOSE`, as the caption's close button does (a folded
    /// candidate, Req 14.1). Does not wait; the window's destruction arrives
    /// through the window event source. Every call posts again, so the core's
    /// retry after `FOLD_CLOSE_RETRY_MS` needs no state here. `Err` if `win`
    /// is not a live Explorer window or the post fails; a window that does
    /// not answer (a hung thread) still accepts the post.
    pub fn close_window(&mut self, win: WinKey) -> Result<(), ShellError> {
        let hwnd = self.inner.live_window(win)?;
        // SAFETY: posting a plain message to a validated Explorer window.
        unsafe { PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(CLOSE_WINDOW_SYSCOMMAND), LPARAM(0)) }
            .map_err(|e| win_error("PostMessageW(WM_SYSCOMMAND SC_CLOSE)", &e))
    }

    /// Closes one tab (UIA close button, or `Quit` per strategy). The UIA
    /// close is asynchronous; a failure is reported as
    /// [`NavigatorSignal::CloseFailed`].
    pub fn close_tab(&mut self, tab: TabKey) -> Result<(), ShellError> {
        let (tab_hwnd, top) = self.inner.live_tab(tab)?;
        match self.inner.strategies.tab_close {
            TabCloseStrategy::Quit => {
                let browser = self.inner.browser_of(tab_hwnd)?;
                // SAFETY: plain COM call from this pumping STA.
                unsafe { browser.Quit() }.map_err(|e| win_error("IWebBrowser2::Quit", &e))?;
                self.inner
                    .state
                    .borrow_mut()
                    .browsers
                    .remove(&hwnd_value(tab_hwnd));
                Ok(())
            }
            TabCloseStrategy::UiaCloseButton => {
                let top_v = hwnd_value(top);
                let win = self.inner.windows.key_of(top_v);
                let live = tab_children(top);
                let (order, sender) = {
                    let mut st = self.inner.state.borrow_mut();
                    let order = &mut st.managed_entry(win).order;
                    merge_tab_order(order, &live);
                    let order = order.clone();
                    (order, st.uia.clone())
                };
                let sender =
                    sender.ok_or_else(|| error(0, "close_tab: navigator not started".into()))?;
                let names = self.inner.tab_names(tab_hwnd);
                // The late tab of a stopped fold (Req 14.13) is closed after
                // its window appeared but before it registered in
                // ShellWindows, so it has no folder to compare the strip item
                // with. It is the newest tab, hence the rightmost: that is the
                // only identity left to check ([`unnamed_close_allowed`]).
                let rightmost_only =
                    names.is_empty() && self.inner.state.borrow().registering.get(&top_v) == Some(&win);
                sender
                    .send(UiaJob::CloseTab {
                        top: top_v,
                        order,
                        tab,
                        names,
                        rightmost_only,
                    })
                    .map_err(|_| error(0, "close_tab: UIA worker stopped".into()))
            }
        }
    }

    /// Selects exactly `items` (absolute ITEMIDLISTs) in `tab`'s view,
    /// focusing the first. An empty list clears the selection.
    pub fn select_items(&mut self, tab: TabKey, items: &[ItemId]) -> Result<(), ShellError> {
        let (tab_hwnd, _) = self.inner.live_tab(tab)?;
        let browser = self.inner.browser_of(tab_hwnd)?;
        let sb = shell_browser(&browser).map_err(|e| win_error("SID_STopLevelBrowser", &e))?;
        // SAFETY: plain COM call on a live interface.
        let view = unsafe { sb.QueryActiveShellView() }
            .map_err(|e| win_error("QueryActiveShellView", &e))?;
        if items.is_empty() {
            // SAFETY: a null PIDL with SVSI_DESELECTOTHERS deselects everything.
            return unsafe { view.SelectItem(std::ptr::null(), SVSI_DESELECTOTHERS.0 as u32) }
                .map_err(|e| win_error("IShellView::SelectItem(deselect)", &e));
        }
        for (i, item) in items.iter().enumerate() {
            if item.0.len() <= 2 || !pidl_is_well_formed(&item.0) {
                return Err(error(
                    0,
                    format!("select_items: item {i} is not an item ID list"),
                ));
            }
            // SAFETY: `item` is a terminated, well-formed ITEMIDLIST; the last
            // ID points into it and stays valid for the call.
            let r = unsafe {
                let child = ILFindLastID(item.0.as_ptr().cast::<ITEMIDLIST>());
                view.SelectItem(child, select_flags(i))
            };
            r.map_err(|e| win_error(&format!("IShellView::SelectItem(item {i})"), &e))?;
        }
        Ok(())
    }
}

impl Drop for ShellNavigator {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A non-owning handle on a [`ShellNavigator`] (see
/// [`ShellNavigator::handle`]). Bound to the navigator's thread like the
/// navigator itself. Every call is a no-op (or `None`) once the navigator
/// is dropped.
#[derive(Clone)]
pub struct NavigatorHandle {
    inner: Weak<Inner>,
}

impl NavigatorHandle {
    /// [`ShellNavigator::claim`].
    pub fn claim(&self, win: WinKey, initial: &LocationId) -> Option<RequestId> {
        self.inner.upgrade()?.claim(win, initial)
    }

    /// [`ShellNavigator::tab_registered`].
    pub fn tab_registered(&self, win: WinKey, tab: TabKey) {
        if let Some(inner) = self.inner.upgrade() {
            inner.tab_registered(win, tab);
        }
    }

    /// The browser of a newly registered `tab`, from the ShellWindows source
    /// (saves the lookup when the tab is navigated, task 16.3).
    pub fn tab_browser(&self, tab: TabKey, browser: &IWebBrowser2) {
        if let Some(inner) = self.inner.upgrade() {
            let mut st = inner.state.borrow_mut();
            st.source_browsers.retain(|t, _| is_alive(t.hwnd));
            st.source_browsers.insert(tab, browser.clone());
        }
    }

    /// [`ShellNavigator::window_destroyed`].
    pub fn window_destroyed(&self, win: WinKey) {
        if let Some(inner) = self.inner.upgrade() {
            inner.window_destroyed(win);
        }
    }

    /// Hands a newly registered `tab` of `win` to a browser worker: `sink`
    /// (if any) is attached to `browser` there, and the tab's navigations
    /// are issued there ([`crate::browser_worker`]). `failed` runs on the
    /// worker if the sink cannot be attached. Returns false (nothing
    /// changed) when there are no workers ([`crate::browser_worker::enabled`]
    /// was off when the navigator started) or `browser` cannot be
    /// marshaled; the caller then attaches the sink itself.
    pub fn attach_browser(
        &self,
        win: WinKey,
        tab: TabKey,
        browser: &IWebBrowser2,
        sink: Option<crate::browser_worker::SinkFactory>,
        failed: crate::browser_worker::OnFailure,
    ) -> bool {
        let Some(inner) = self.inner.upgrade() else { return false };
        let Ok(mut st) = inner.state.try_borrow_mut() else { return false };
        match &mut st.workers {
            Some(w) => w.attach(win.hwnd, tab, browser, sink, failed).is_ok(),
            None => false,
        }
    }

    /// The sink of `tab` attached by a browser worker must go (the tab is
    /// gone, or its automation object was replaced).
    pub fn detach_browser(&self, tab: TabKey) {
        if let Some(inner) = self.inner.upgrade()
            && let Ok(mut st) = inner.state.try_borrow_mut()
            && let Some(w) = &mut st.workers
        {
            w.detach(tab);
        }
    }

    /// A navigation of `tab` completed at the Location with parsing name
    /// `parsing_name`: the Location the navigator last navigated `tab` to,
    /// if it has exactly that parsing name (it is then forgotten).
    pub fn expected_location(&self, tab: TabKey, parsing_name: &[u16]) -> Option<LocationId> {
        let inner = self.inner.upgrade()?;
        let mut st = inner.state.try_borrow_mut().ok()?;
        match st.expected.get(&tab) {
            Some(l) if l.parsing_name.units() == parsing_name => st.expected.remove(&tab),
            _ => None,
        }
    }

    /// Whether `tab` was navigated by the navigator and the completion has
    /// not been matched by [`expected_location`](Self::expected_location).
    pub fn navigation_expected(&self, tab: TabKey) -> bool {
        self.inner
            .upgrade()
            .and_then(|i| i.state.try_borrow().ok().map(|st| st.expected.contains_key(&tab)))
            .unwrap_or(false)
    }

    /// [`ShellNavigator::is_creating_tab`], or the command was settled by the
    /// new tab's window appearing and the tab has not registered yet.
    pub fn creating_tab(&self, win: WinKey) -> bool {
        self.inner
            .upgrade()
            .and_then(|i| {
                i.state.try_borrow().ok().map(|st| {
                    st.creates.get(&win.hwnd).is_some_and(|c| c.key == win)
                        || st.registering.get(&win.hwnd) == Some(&win)
                })
            })
            .unwrap_or(false)
    }
}

impl Inner {
    fn claim(&self, win: WinKey, initial: &LocationId) -> Option<RequestId> {
        let hwnd = win.hwnd;
        let launch_hidden = self.strategies.hide == HideStrategy::LaunchHidden;
        let claim = {
            let mut st = self.state.borrow_mut();
            let now = Instant::now();
            st.strays
                .retain(|(_, at)| now.saturating_duration_since(*at) < Duration::from_millis(STRAY_WINDOW_MS));
            if st.opens.is_empty() && st.strays.is_empty() {
                return None;
            }
            let claim = st.opens.claim(hwnd, initial, is_alive);
            // A window launched hidden for a request that was dropped before
            // the window was seen: show it rather than leave it invisible.
            let stray = claim.claimed.is_none()
                && launch_hidden
                && !st.managed.get(&hwnd).is_some_and(|m| m.key == win && m.claimed)
                && !crate::shell_util::is_window_visible(hwnd_of(hwnd))
                && match st.strays.iter().position(|(l, _)| l == initial) {
                    Some(i) => {
                        st.strays.remove(i);
                        true
                    }
                    None => false,
                };
            if stray {
                let m = st.managed_entry(win);
                m.hidden.get_or_insert(Hidden::LAUNCHED);
                m.claimed = false;
            }
            Claim { unmatched: claim.unmatched || stray, ..claim }
        };
        let mut reapply: Vec<(u64, FinalPlacement)> = claim.reassigned.clone();
        let mut claimed = None;
        if let Some((request, placement, changed)) = claim.claimed {
            {
                let mut st = self.state.borrow_mut();
                let m = st.managed_entry(win);
                m.claimed = true;
                m.target = Some(placement);
                if m.order.is_empty() {
                    m.order = tab_children(hwnd_of(hwnd));
                }
                // Launched hidden, but its EVENT_OBJECT_CREATE has not been
                // handled yet (it fires late, when CreateWindowEx returns).
                if launch_hidden && m.hidden.is_none() && !crate::shell_util::is_window_visible(hwnd_of(hwnd)) {
                    m.hidden = Some(Hidden::LAUNCHED);
                }
            }
            if changed {
                reapply.push((hwnd, placement));
            }
            claimed = Some(request);
        } else if claim.unmatched {
            self.release(hwnd);
        }
        for (other, placement) in reapply {
            self.reapply_hidden(other, &placement);
        }
        self.update_hook_and_timer();
        claimed
    }

    fn tab_registered(&self, win: WinKey, tab: TabKey) {
        let live = tab_children(hwnd_of(win.hwnd));
        let unsettled = {
            let mut st = self.state.borrow_mut();
            let unsettled = st.creates.get(&win.hwnd).is_some_and(|c| c.key == win);
            if unsettled {
                st.creates.remove(&win.hwnd);
            }
            st.registering.remove(&win.hwnd);
            let order = &mut st.managed_entry(win).order;
            merge_tab_order(order, &live);
            if !order.contains(&tab.hwnd) && is_alive(tab.hwnd) {
                order.push(tab.hwnd);
            }
            unsettled
        };
        // The tab registered before the housekeeping timer saw its window:
        // its start is reported now, right before the caller's `TabCreated`.
        if unsettled {
            self.emit(vec![NavigatorSignal::TabStarted { win, tab }]);
        }
    }

    fn window_destroyed(&self, win: WinKey) {
        let mut st = self.state.borrow_mut();
        if st.managed.get(&win.hwnd).is_some_and(|m| m.key == win) {
            st.managed.remove(&win.hwnd);
        }
        if st.creates.get(&win.hwnd).is_some_and(|c| c.key == win) {
            st.creates.remove(&win.hwnd);
        }
        st.registering.remove(&win.hwnd);
        if let Some(w) = &mut st.workers {
            w.window_destroyed(win.hwnd);
        }
    }
}

impl State {
    /// The entry for `win`, replacing an entry of an earlier window that had
    /// the same HWND value.
    fn managed_entry(&mut self, win: WinKey) -> &mut Managed {
        let m = self
            .managed
            .entry(win.hwnd)
            .or_insert_with(|| Managed::new(win));
        if m.key != win {
            *m = Managed::new(win);
        }
        m
    }
}

impl Inner {
    fn emit(&self, signals: Vec<NavigatorSignal>) {
        for s in signals {
            self.handler.on_signal(s);
        }
    }

    /// `win` as a live top-level Explorer window.
    fn live_window(&self, win: WinKey) -> Result<HWND, ShellError> {
        if self.windows.get(win.hwnd).is_some_and(|k| k != win) {
            return Err(error(0, format!("{win:?} is no longer live")));
        }
        let hwnd = hwnd_of(win.hwnd);
        if !is_cabinet_window(hwnd) {
            return Err(error(0, format!("{win:?} is not a live Explorer window")));
        }
        Ok(hwnd)
    }

    /// `tab` as a live `ShellTabWindowClass` window and its top-level window.
    fn live_tab(&self, tab: TabKey) -> Result<(HWND, HWND), ShellError> {
        if self.tabs.get(tab.hwnd).is_some_and(|k| k != tab) {
            return Err(error(0, format!("{tab:?} is no longer live")));
        }
        let hwnd = hwnd_of(tab.hwnd);
        if !is_alive(tab.hwnd) || window_class_name(hwnd) != SHELL_TAB_WINDOW_CLASS {
            return Err(error(0, format!("{tab:?} is not a live Explorer tab")));
        }
        // SAFETY: GetAncestor tolerates stale HWNDs.
        let top = unsafe { GetAncestor(hwnd, GA_ROOT) };
        if !is_cabinet_window(top) {
            return Err(error(0, format!("{tab:?} has no Explorer window")));
        }
        Ok((hwnd, top))
    }

    fn ensure_hook(&self) -> Result<(), ShellError> {
        if self.state.borrow().hook.is_some() {
            return Ok(());
        }
        // SAFETY: out-of-context hook with a 'static callback, removed on this thread.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_CREATE,
                None,
                Some(create_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            let e = windows::core::Error::from_thread();
            return Err(win_error("SetWinEventHook(EVENT_OBJECT_CREATE)", &e));
        }
        self.state.borrow_mut().hook = Some(hook);
        Ok(())
    }

    fn ensure_timer(&self) {
        let window = {
            let st = self.state.borrow();
            if st.timer {
                return;
            }
            st.message_window
        };
        let Some(window) = window else { return };
        // SAFETY: a timer on this thread's own message window.
        if unsafe { SetTimer(Some(window), TIMER_ID, TICK_MS, None) } != 0 {
            self.state.borrow_mut().timer = true;
        }
    }

    /// Removes the hook when no open request is pending, and the timer when
    /// nothing needs housekeeping.
    fn update_hook_and_timer(&self) {
        let (hook, kill) = {
            let mut st = self.state.borrow_mut();
            let hook = if st.opens.is_empty() {
                st.hook.take()
            } else {
                None
            };
            let idle = st.opens.is_empty() && st.creates.is_empty();
            let kill = (idle && st.timer).then_some(st.message_window).flatten();
            if kill.is_some() {
                st.timer = false;
            }
            (hook, kill)
        };
        if let Some(hook) = hook {
            // SAFETY: installed by this navigator on this thread.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
        if let Some(window) = kill {
            // SAFETY: the timer of this thread's own message window.
            let _ = unsafe { KillTimer(Some(window), TIMER_ID) };
        }
    }

    /// `EVENT_OBJECT_CREATE` for `hwnd` (already filtered to `OBJID_WINDOW`).
    fn on_created(&self, hwnd: HWND) {
        if !is_cabinet_window(hwnd) {
            return;
        }
        let value = hwnd_value(hwnd);
        let assignment = match self.state.try_borrow_mut() {
            Ok(mut st) => {
                if st.opens.is_empty() {
                    return;
                }
                // Already claimed: EVENT_OBJECT_CREATE fires only when
                // CreateWindowEx returns, which can be after the window's
                // first navigation completed and claimed it. Assigning it
                // again would give it another request's placement.
                let key = self.windows.get(value);
                if st.managed.get(&value).is_some_and(|m| m.claimed && Some(m.key) == key) {
                    return;
                }
                st.opens.on_created(value, is_alive)
            }
            Err(_) => {
                // Re-entrant delivery: handle it from the message loop.
                self.deferred.borrow_mut().push(value);
                if let Some(a) = self.state.try_borrow().ok().and_then(|s| s.asyncs.clone()) {
                    a.push_wakeup();
                }
                return;
            }
        };
        let Some((_, placement)) = assignment else {
            return;
        };
        crate::diag::note(|| format!("nav: CREATE {value:#x} visible={}", crate::shell_util::is_window_visible(hwnd)));
        let win = self.windows.key_of(value);
        let original = get_placement(hwnd);
        let hidden = match self.strategies.hide {
            // Nothing to do: the window stays invisible until it is
            // revealed, and its placement is applied then.
            HideStrategy::LaunchHidden => {
                (!crate::shell_util::is_window_visible(hwnd)).then_some(Hidden::LAUNCHED)
            }
            HideStrategy::LayeredAlpha => {
                let added_layered = make_transparent(hwnd);
                let _ = set_placement(hwnd, &placement, PlacementPhase::Hidden);
                Some(Hidden { transparent: true, added_layered, placed: true })
            }
            HideStrategy::PlaceOnlyNoHide => {
                let _ = set_placement(hwnd, &placement, PlacementPhase::Final);
                None
            }
        };
        crate::diag::note(|| format!("nav: CREATE {value:#x} handled"));
        let mut st = self.state.borrow_mut();
        let m = st.managed_entry(win);
        m.hidden = hidden;
        m.target = Some(placement);
        m.original = original;
        m.claimed = false;
        m.order = Vec::new();
    }

    /// Processes `EVENT_OBJECT_CREATE`s deferred by re-entrancy.
    fn run_deferred(&self) {
        let deferred: Vec<u64> = std::mem::take(&mut *self.deferred.borrow_mut());
        for h in deferred {
            if is_alive(h) {
                self.on_created(hwnd_of(h));
            }
        }
    }

    /// Re-applies `placement` to a window the navigator is hiding.
    fn reapply_hidden(&self, hwnd: u64, placement: &FinalPlacement) {
        if !is_alive(hwnd) {
            return;
        }
        let win = self.windows.key_of(hwnd);
        let hidden = {
            let mut st = self.state.borrow_mut();
            let m = st.managed_entry(win);
            m.target = Some(*placement);
            if let Some(h) = &mut m.hidden {
                h.placed = true;
            }
            m.hidden
        };
        let phase = match hidden {
            Some(h) => h.phase(),
            None => PlacementPhase::Final,
        };
        let _ = set_placement(hwnd_of(hwnd), placement, phase);
    }

    /// A window hidden for a request it does not belong to: give it back its
    /// original placement (if the navigator moved it) and make it visible.
    fn release(&self, hwnd: u64) {
        let entry = {
            let mut st = self.state.borrow_mut();
            match st.managed.get(&hwnd) {
                Some(m) if !m.claimed => st.managed.remove(&hwnd),
                _ => None,
            }
        };
        let Some(m) = entry else { return };
        if !is_alive(hwnd) {
            return;
        }
        let h = hwnd_of(hwnd);
        let Some(hidden) = m.hidden else { return };
        crate::diag::note(|| format!("nav: release {hwnd:#x}"));
        let original = if hidden.placed { m.original } else { None };
        match original {
            Some(wp) => restore_placement(h, &wp),
            // Launched hidden and never moved: show it where it is.
            None if !crate::shell_util::is_window_visible(h) => {
                if let Some(wp) = get_placement(h) {
                    restore_placement(h, &wp);
                }
            }
            None => {}
        }
        if hidden.transparent {
            make_opaque(h, hidden.added_layered);
        }
    }

    fn reveal_hwnd(&self, hwnd: HWND) -> Result<(), ShellError> {
        let value = hwnd_value(hwnd);
        let (hidden, target, claimed) = {
            let mut st = self.state.borrow_mut();
            match st.managed.get_mut(&value) {
                Some(m) => (m.hidden.take(), m.target.take(), m.claimed),
                None => (None, None, false),
            }
        };
        crate::diag::note(|| format!("nav: reveal {value:#x} hidden={}", hidden.is_some()));
        let Some(hidden) = hidden else {
            // A window launched hidden that Explorer showed on its own
            // (never seen): it still gets its final placement.
            if claimed && self.strategies.hide == HideStrategy::LaunchHidden
                && let Some(t) = target
            {
                return set_placement(hwnd, &t, PlacementPhase::Final);
            }
            return Ok(());
        };
        // One SetWindowPlacement: a window launched hidden becomes visible
        // here, at its final placement; a transparent one is placed first
        // and then made opaque.
        let placed = match target {
            Some(t) => set_placement_with(hwnd, &t, PlacementPhase::Final, !hidden.transparent),
            None if !hidden.transparent => {
                // No placement known (not claimed): just show it.
                if let Some(wp) = get_placement(hwnd) {
                    restore_placement(hwnd, &wp);
                }
                Ok(())
            }
            None => Ok(()),
        };
        if hidden.transparent {
            make_opaque(hwnd, hidden.added_layered);
        }
        placed
    }

    /// The names the tab's UIA strip item can have: the parsing and the
    /// display name of the folder the tab shows now (newer builds, measured on
    /// 26300, name the item by its parsing name, older ones by its display
    /// name). Empty when the tab's folder cannot be read.
    fn tab_names(&self, tab: HWND) -> Vec<String> {
        let Ok(browser) = self.browser_of(tab) else { return Vec::new() };
        let Ok(pidl) = crate::shell_util::current_folder_pidl(&browser) else { return Vec::new() };
        match crate::location::try_capture_from_pidl(&pidl) {
            Ok(l) => vec![l.parsing_name.to_string_lossy(), l.display_name.to_string_lossy()],
            Err(_) => Vec::new(),
        }
    }

    /// The browser of a tab: cached, else found by scanning ShellWindows from
    /// the newest entry (new tabs are appended).
    fn browser_of(&self, tab: HWND) -> Result<IWebBrowser2, ShellError> {
        let value = hwnd_value(tab);
        let (cached, sw) = {
            let st = self.state.borrow();
            (st.browsers.get(&value).cloned(), st.shell_windows.clone())
        };
        if let Some(b) = cached {
            if tab_hwnd(&b).is_ok_and(|h| h == tab) {
                return Ok(b);
            }
            self.state.borrow_mut().browsers.remove(&value);
        }
        let sw = match sw {
            Some(sw) => sw,
            None => {
                let sw = shell_windows().map_err(|e| win_error("ShellWindows", &e))?;
                self.state.borrow_mut().shell_windows = Some(sw.clone());
                sw
            }
        };
        // SAFETY: plain COM property read.
        let count = unsafe { sw.Count() }.map_err(|e| win_error("IShellWindows::Count", &e))?;
        for index in (0..count).rev() {
            // SAFETY: a VT_I4 VARIANT that owns no resources.
            let Ok(dispatch) = (unsafe { sw.Item(&VARIANT::from(index)) }) else {
                continue;
            };
            let Ok(browser) = dispatch.cast::<IWebBrowser2>() else {
                continue;
            };
            let Ok(h) = tab_hwnd(&browser) else {
                continue;
            };
            let found = h == tab;
            self.state
                .borrow_mut()
                .browsers
                .insert(hwnd_value(h), browser.clone());
            if found {
                return Ok(browser);
            }
        }
        Err(error(0, format!("no ShellWindows entry for tab {tab:?}")))
    }

    fn attempt_create(
        &self,
        top: HWND,
        win: WinKey,
        attempt: CreateAttempt,
    ) -> Result<(), ShellError> {
        match attempt {
            CreateAttempt::WmCommand(_) => {
                // With browser workers, the command is posted by the window's
                // worker, behind the previous tab's sink and navigation: posted
                // from here while that Navigate2 was still in flight, it was
                // followed by a heap corruption in Explorer once (task 16.3).
                {
                    let mut st = self.state.borrow_mut();
                    if let Some(w) = &mut st.workers
                        && w.post_command(win.hwnd, NEW_TAB_COMMAND)
                    {
                        return Ok(());
                    }
                }
                // SAFETY: posting a plain message to a validated Explorer window.
                unsafe { PostMessageW(Some(top), WM_COMMAND, WPARAM(NEW_TAB_COMMAND), LPARAM(0)) }
                    .map_err(|e| win_error("PostMessageW(WM_COMMAND 0xA21B)", &e))
            }
            CreateAttempt::UiaAddButton => {
                let sender = self.state.borrow().uia.clone();
                sender
                    .ok_or_else(|| error(0, "create_tab: navigator not started".into()))?
                    .send(UiaJob::AddTab { top: win.hwnd })
                    .map_err(|_| error(0, "create_tab: UIA worker stopped".into()))
            }
        }
    }

    /// The new-tab command of `key` (top-level `top`) produced a tab window
    /// (`live` are the window's tab HWNDs now): the command is settled, but
    /// the tab still counts as being created until it registers
    /// ([`NavigatorHandle::creating_tab`]).
    fn settle_create(&self, top: u64, key: WinKey, live: &[u64]) {
        let mut st = self.state.borrow_mut();
        st.creates.remove(&top);
        st.registering.insert(top, key);
        merge_tab_order(&mut st.managed_entry(key).order, live);
    }

    /// Housekeeping: settle, re-post, or fail outstanding new-tab commands,
    /// and expire old open requests.
    fn tick(&self) {
        let now = Instant::now();
        let creates: Vec<(u64, WinKey, Vec<u64>, CreateAttempt, Instant)> = {
            let st = self.state.borrow();
            st.creates
                .iter()
                .map(|(h, c)| (*h, c.key, c.baseline.clone(), c.attempt, c.deadline))
                .collect()
        };
        let mut signals = Vec::new();
        for (top, key, baseline, attempt, deadline) in creates {
            if !is_cabinet_window(hwnd_of(top)) {
                self.state.borrow_mut().creates.remove(&top);
                continue;
            }
            let live = tab_children(hwnd_of(top));
            if let Some(started) = new_tab_among(&baseline, &live) {
                self.settle_create(top, key, &live);
                signals.push(NavigatorSignal::TabStarted {
                    win: key,
                    tab: self.tabs.key_of(started),
                });
                continue;
            }
            if now < deadline {
                continue;
            }
            match attempt.next() {
                Some(next) => {
                    let result = self.attempt_create(hwnd_of(top), key, next);
                    let mut st = self.state.borrow_mut();
                    match result {
                        Ok(()) => {
                            if let Some(c) = st.creates.get_mut(&top) {
                                c.attempt = next;
                                c.baseline = live;
                                c.deadline = now + next.wait();
                            }
                            signals.push(NavigatorSignal::TabCreateRetried {
                                win: key,
                                attempt: next.number(),
                                strategy: next.strategy(),
                            });
                        }
                        Err(e) => {
                            st.creates.remove(&top);
                            signals.push(NavigatorSignal::TabCreateFailed { win: key, error: e });
                        }
                    }
                }
                None => {
                    self.state.borrow_mut().creates.remove(&top);
                    signals.push(NavigatorSignal::TabCreateFailed {
                        win: key,
                        error: error(
                            0,
                            format!(
                                "no tab appeared in {key:?} after {} attempts",
                                attempt.number()
                            ),
                        ),
                    });
                }
            }
        }
        let expired = self
            .state
            .borrow_mut()
            .opens
            .expire(now, Duration::from_millis(OPEN_REQUEST_EXPIRY_MS));
        for p in expired {
            match p.window {
                Some(w) => self.release(w),
                None => self.remember_stray(p.location),
            }
        }
        self.update_hook_and_timer();
        self.emit(signals);
    }

    /// A `LaunchHidden` request was dropped before its window was seen.
    fn remember_stray(&self, location: LocationId) {
        if self.strategies.hide == HideStrategy::LaunchHidden {
            self.state.borrow_mut().strays.push((location, Instant::now()));
        }
    }

    /// Results from the launcher threads and the UIA worker.
    fn drain_async(&self) {
        self.run_deferred();
        let asyncs = self.state.borrow().asyncs.clone();
        let Some(asyncs) = asyncs else { return };
        let mut signals = Vec::new();
        for r in asyncs.drain() {
            match r {
                AsyncResult::OpenFailed { request, error } => {
                    let removed = self.state.borrow_mut().opens.remove(request);
                    match removed {
                        Some(PendingOpen { window: Some(w), .. }) => self.release(w),
                        Some(p) => self.remember_stray(p.location),
                        None => {}
                    }
                    signals.push(NavigatorSignal::OpenFailed { request, error });
                }
                AsyncResult::TabAdd { top, result } => {
                    let Err(e) = result else { continue };
                    let mut st = self.state.borrow_mut();
                    let failed = st
                        .creates
                        .get(&top)
                        .filter(|c| c.attempt == CreateAttempt::UiaAddButton)
                        .map(|c| c.key);
                    if let Some(win) = failed {
                        st.creates.remove(&top);
                        signals.push(NavigatorSignal::TabCreateFailed { win, error: e });
                    } else if !st.creates.contains_key(&top)
                        && self.strategies.tab_pipelining == TabCreationPipelining::Pipelined
                    {
                        signals.push(NavigatorSignal::TabCreateFailed {
                            win: self.windows.key_of(top),
                            error: e,
                        });
                    }
                }
                AsyncResult::TabClose { top, tab, unregistered, result } => {
                    if let Err(e) = result {
                        signals.push(NavigatorSignal::CloseFailed { tab, error: e });
                    } else {
                        let mut st = self.state.borrow_mut();
                        st.browsers.remove(&tab.hwnd);
                        // A tab closed before it registered (the late tab of a
                        // stopped fold) never will: it no longer counts as
                        // being created.
                        if unregistered {
                            st.registering.remove(&top);
                        }
                    }
                }
                AsyncResult::NavigateFailed { tab, error } => {
                    signals.push(NavigatorSignal::NavigateFailed { tab, error });
                }
                AsyncResult::ZOrderFailed { error } => {
                    signals.push(NavigatorSignal::ZOrderFailed { error });
                }
            }
        }
        self.update_hook_and_timer();
        self.emit(signals);
    }
}

impl AsyncQueue {
    /// Wakes the navigator's thread without a result (deferred work).
    fn push_wakeup(&self) {
        // SAFETY: see `push`.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd_of(self.window)),
                WM_NAV_ASYNC,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

/// WinEvent callback (out-of-context, on the navigator's thread).
unsafe extern "system" fn create_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    time: u32,
) {
    if event != EVENT_OBJECT_CREATE
        || id_object != OBJID_WINDOW.0
        || id_child != CHILDID_SELF as i32
        || hwnd.is_invalid()
    {
        return;
    }
    if crate::diag::tracing() && is_cabinet_window(hwnd) {
        // SAFETY: no preconditions.
        let lag = unsafe { windows::Win32::System::SystemInformation::GetTickCount() }.wrapping_sub(time);
        crate::diag::note(|| format!("nav: CREATE event {:#x} delivered {lag} ms after it fired", hwnd_value(hwnd)));
    }
    if let Some(inner) = active_navigator() {
        inner.on_created(hwnd);
    }
}

unsafe extern "system" fn message_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ours = || {
        active_navigator().filter(|i| {
            i.state
                .try_borrow()
                .is_ok_and(|s| s.message_window == Some(hwnd))
        })
    };
    match msg {
        WM_TIMER if wparam.0 == TIMER_ID => {
            if let Some(inner) = ours() {
                inner.tick();
            }
            LRESULT(0)
        }
        WM_NAV_ASYNC => {
            if let Some(inner) = ours() {
                inner.drain_async();
            }
            LRESULT(0)
        }
        // SAFETY: default processing for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ---------------------------------------------------------------------------
// Win32 / Shell helpers
// ---------------------------------------------------------------------------

fn is_alive(hwnd: u64) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd_of(hwnd))) }.as_bool()
}

/// HWND values of the `ShellTabWindowClass` windows inside `top`, in
/// `EnumChildWindows` order.
pub fn tab_children(top: HWND) -> Vec<u64> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<u64>` passed below.
        let list = unsafe { &mut *(data.0 as *mut Vec<u64>) };
        if window_class_name(hwnd) == SHELL_TAB_WINDOW_CLASS {
            list.push(hwnd_value(hwnd));
        }
        true.into()
    }
    let mut list: Vec<u64> = Vec::new();
    // SAFETY: the callback only runs during this call; `list` outlives it.
    unsafe {
        let _ = EnumChildWindows(Some(top), Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

fn get_placement(hwnd: HWND) -> Option<WINDOWPLACEMENT> {
    let _dpi = ThreadDpiScope::per_monitor_v2();
    let mut wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: valid, correctly sized out structure.
    unsafe { GetWindowPlacement(hwnd, &mut wp) }.ok()?;
    Some(wp)
}

/// Puts a window back at a placement read by [`get_placement`], without
/// activating it (a released window).
fn restore_placement(hwnd: HWND, original: &WINDOWPLACEMENT) {
    let mut wp = *original;
    wp.showCmd = SW_SHOWNOACTIVATE.0 as u32;
    let _dpi = ThreadDpiScope::per_monitor_v2();
    // SAFETY: a fully initialized WINDOWPLACEMENT read from this window.
    let _ = unsafe { SetWindowPlacement(hwnd, &wp) };
}

fn set_placement(
    hwnd: HWND,
    placement: &FinalPlacement,
    phase: PlacementPhase,
) -> Result<(), ShellError> {
    set_placement_with(hwnd, placement, phase, true)
}

/// [`set_placement`]; `asynchronous: false` returns only once the placement
/// is applied (needed before a transparent window is made opaque).
fn set_placement_with(
    hwnd: HWND,
    placement: &FinalPlacement,
    phase: PlacementPhase,
    asynchronous: bool,
) -> Result<(), ShellError> {
    let mut wp = window_placement_for(placement, phase)
        .ok_or_else(|| error(0, format!("placement out of range: {placement:?}")))?;
    // Carried out by Explorer's window thread itself, in one go. Done from
    // here, a change of state (maximized to normal) was visible in two steps
    // for ~90 ms: the state changed, then the window moved once Explorer's
    // busy thread answered the cross-thread WM_WINDOWPOSCHANGING (Req 7.7).
    if asynchronous {
        wp.flags |= WPF_ASYNCWINDOWPLACEMENT;
    }
    let _dpi = ThreadDpiScope::per_monitor_v2();
    // SAFETY: a fully initialized WINDOWPLACEMENT.
    unsafe { SetWindowPlacement(hwnd, &wp) }.map_err(|e| win_error("SetWindowPlacement", &e))
}

/// Adds `WS_EX_LAYERED` with alpha 0. Returns whether the style was added
/// (rather than already present).
fn make_transparent(hwnd: HWND) -> bool {
    // SAFETY: style and layered-attribute calls on a live window of the same
    // integrity level.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let added = ex & WS_EX_LAYERED.0 as isize == 0;
        if added {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        }
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        added
    }
}

/// Alpha 255, and `WS_EX_LAYERED` removed if the navigator added it.
fn make_opaque(hwnd: HWND, remove_layered: bool) {
    // SAFETY: as in `make_transparent`; RedrawWindow repaints after the style change.
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        if remove_layered {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN,
            );
        }
    }
}

/// The alpha of a layered window, or `None` if it is not layered with `LWA_ALPHA`.
pub fn layered_alpha(hwnd: HWND) -> Option<u8> {
    // SAFETY: plain reads; stale HWNDs fail.
    unsafe {
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize == 0 {
            return None;
        }
        let mut alpha = 255u8;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
        GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)).ok()?;
        (flags.0 & LWA_ALPHA.0 != 0).then_some(alpha)
    }
}

fn raise(hwnd: HWND) -> Result<(), ShellError> {
    // SAFETY: z-order change only.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    }
    .map_err(|e| win_error("SetWindowPos(HWND_TOP)", &e))
}

/// The PIDL to open or navigate to for `location`: a fresh `search-ms:` URL
/// for a search (Req 8.6), otherwise the parsing name, bound with
/// `SHParseDisplayName`; the stored PIDL bytes if binding fails.
///
/// `search-ms:` URLs are bound with [`parse_bind_ctx`] to the native search
/// folder, which `Navigate2` shows in the tab. Should the Shell still return
/// an Internet-URL item (which Explorer would open in a new, visible window
/// instead of the tab, Req 7.3), the stored PIDL is used if it is not one,
/// and otherwise the navigation fails.
pub fn location_pidl(location: &LocationId) -> Result<Vec<u8>, ShellError> {
    // A deferred tab's placeholder file is written on first use (Req 5.13).
    if let Err(e) = crate::placeholder::ensure(location) {
        return Err(error(e.raw_os_error().unwrap_or(0), format!("placeholder file: {e}")));
    }
    let name: WideString = match (&location.kind, &location.search) {
        (LocationKind::Search, Some(spec)) => {
            build_search_ms(&spec.query, &spec.scope.parsing_name).ok()
        }
        _ => None,
    }
    .unwrap_or_else(|| location.parsing_name.clone());
    let stored_ok = pidl_is_well_formed(&location.pidl) && !is_internet_url_pidl(&location.pidl);
    match parse_display_name(name.units()) {
        Ok(pidl) if is_search_ms_name(name.units()) && is_internet_url_pidl(&pidl) => {
            if stored_ok {
                Ok(location.pidl.clone())
            } else {
                Err(error(
                    0,
                    "the search URL binds only to an Internet-URL item, which Explorer \
                     opens in a new window"
                        .into(),
                ))
            }
        }
        Ok(pidl) => Ok(pidl),
        Err(_) if stored_ok => Ok(location.pidl.clone()),
        Err(e) => Err(e),
    }
}

fn parse_display_name(name: &[u16]) -> Result<Vec<u8>, ShellError> {
    if name.is_empty() || name.contains(&0) {
        return Err(error(0, "unbindable parsing name".into()));
    }
    let mut z = name.to_vec();
    z.push(0);
    let ctx = parse_bind_ctx(name);
    let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: `z` is NUL-terminated; `out` is a valid out pointer.
    let r = unsafe { SHParseDisplayName(PCWSTR(z.as_ptr()), ctx.as_ref(), &mut out, 0, None) };
    if out.is_null() {
        let e = r
            .err()
            .map_or((0, "no PIDL".into()), |e| (e.code().0, e.message()));
        return Err(error(e.0, format!("SHParseDisplayName: {}", e.1)));
    }
    // SAFETY: `out` is a valid PIDL allocated with the COM allocator, freed once.
    let bytes = unsafe { pidl_to_bytes(out) };
    // SAFETY: as above.
    unsafe { CoTaskMemFree(Some(out.cast_const().cast::<c_void>())) };
    r.map_err(|e| win_error("SHParseDisplayName", &e))?;
    Ok(bytes)
}

/// `Navigate2(PIDL as VT_ARRAY|VT_UI1, navNoHistory)`; does not wait.
pub(crate) fn navigate_pidl(browser: &IWebBrowser2, pidl: &[u8]) -> Result<(), ShellError> {
    let flags = VARIANT::from(NAV_NO_HISTORY);
    // Optional [in] VARIANT* parameters must be empty VARIANTs, never None.
    let empty = VARIANT::default();
    // SAFETY: the SAFEARRAY is created and filled here and passed to Navigate2.
    unsafe {
        let psa = SafeArrayCreateVector(VT_UI1, 0, pidl.len() as u32);
        if psa.is_null() {
            return Err(error(0, "SafeArrayCreateVector failed".into()));
        }
        let mut data: *mut c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).map_err(|e| win_error("SafeArrayAccessData", &e))?;
        std::ptr::copy_nonoverlapping(pidl.as_ptr(), data.cast::<u8>(), pidl.len());
        SafeArrayUnaccessData(psa).map_err(|e| win_error("SafeArrayUnaccessData", &e))?;
        // `ManuallyDrop` contents: the VARIANT is never cleared, so the array
        // is never destroyed (R2: SafeArrayDestroy after Navigate2 corrupted
        // the heap). The array is leaked deliberately.
        let url = VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt: VARENUM(VT_ARRAY.0 | VT_UI1.0),
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { parray: psa },
                }),
            },
        };
        let url = ManuallyDrop::new(url);
        browser
            .Navigate2(
                &*url,
                Some(&flags),
                Some(&empty),
                Some(&empty),
                Some(&empty),
            )
            .map_err(|e| win_error("IWebBrowser2::Navigate2", &e))
    }
}

/// `ShellExecuteExW("opennewwindow", SEE_MASK_IDLIST, show)` on the calling
/// (launcher) thread, which enters an STA for the call.
fn shell_execute_open_new_window(pidl: &[u8], show: SHOW_WINDOW_CMD) -> Result<(), ShellError> {
    let _sta = StaGuard::init().map_err(|e| win_error("CoInitializeEx(STA)", &e))?;
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("opennewwindow"),
        lpIDList: pidl.as_ptr().cast_mut().cast::<c_void>(),
        nShow: show.0,
        ..Default::default()
    };
    // SAFETY: `sei` is initialized; the PIDL buffer outlives the call and
    // ShellExecuteExW only reads it.
    unsafe { ShellExecuteExW(&mut sei) }
        .map_err(|e| win_error("ShellExecuteExW(opennewwindow)", &e))
}

fn create_message_window() -> Result<HWND, ShellError> {
    // SAFETY: standard class registration (a repeat registration fails
    // harmlessly) and creation of a message-only window owned by this thread.
    unsafe {
        let module = GetModuleHandleW(None).map_err(|e| win_error("GetModuleHandleW", &e))?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(message_window_proc),
            hInstance: module.into(),
            lpszClassName: MESSAGE_WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MESSAGE_WINDOW_CLASS,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )
        .map_err(|e| win_error("CreateWindowExW(message window)", &e))
    }
}

fn error(code: i32, message: String) -> ShellError {
    ShellError {
        code,
        message: format!("navigator: {message}"),
    }
}

fn win_error(step: &str, e: &windows::core::Error) -> ShellError {
    error(e.code().0, format!("{step}: {}", e.message()))
}

/// Switches the calling thread to per-monitor-aware v2 (physical pixels) and
/// restores the previous context on drop.
struct ThreadDpiScope {
    previous: DPI_AWARENESS_CONTEXT,
}

impl ThreadDpiScope {
    fn per_monitor_v2() -> Self {
        // SAFETY: valid predefined context; null on failure (nothing to restore).
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { previous }
    }
}

impl Drop for ThreadDpiScope {
    fn drop(&mut self) {
        if !self.previous.0.is_null() {
            // SAFETY: restores the context returned by the matching call.
            unsafe { SetThreadDpiAwarenessContext(self.previous) };
        }
    }
}

// ---------------------------------------------------------------------------
// UIA worker (MTA)
// ---------------------------------------------------------------------------

enum UiaJob {
    /// Invoke the tab strip's `AddButton` of top-level window `top`.
    AddTab { top: u64 },
    /// Invoke the close button of `tab`, located by its position in `order`
    /// (tracked visual order of `top`'s tabs).
    CloseTab {
        top: u64,
        order: Vec<u64>,
        tab: TabKey,
        /// Names the tab's strip item can have (see `tab_names`); empty when
        /// unknown, which turns the identity check off.
        names: Vec<String>,
        /// The tab has not registered yet and has no name: only the
        /// rightmost item may be closed ([`unnamed_close_allowed`]).
        rightmost_only: bool,
    },
}

fn spawn_uia_worker(asyncs: Arc<AsyncQueue>) -> Result<Sender<UiaJob>, ShellError> {
    let (tx, rx) = channel::<UiaJob>();
    std::thread::Builder::new()
        .name("esr-navigator-uia".into())
        .spawn(move || uia_worker(rx, asyncs))
        .map_err(|e| error(0, format!("UIA worker thread: {e}")))?;
    Ok(tx)
}

fn uia_worker(rx: Receiver<UiaJob>, asyncs: Arc<AsyncQueue>) {
    // SAFETY: balanced with CoUninitialize below on this same thread.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    {
        let mut uia: Option<Uia> = None;
        while let Ok(job) = rx.recv() {
            if uia.is_none() {
                uia = Uia::new().ok();
            }
            let result = match &uia {
                Some(u) => match &job {
                    UiaJob::AddTab { top } => u.invoke_add(*top),
                    UiaJob::CloseTab { top, order, tab, names, rightmost_only } => {
                        u.close_tab(*top, order, tab.hwnd, names, *rightmost_only)
                    }
                },
                None => Err(error(0, "UI Automation is unavailable".into())),
            };
            asyncs.push(match job {
                UiaJob::AddTab { top } => AsyncResult::TabAdd { top, result },
                UiaJob::CloseTab { top, tab, rightmost_only, .. } => {
                    AsyncResult::TabClose { top, tab, unregistered: rightmost_only, result }
                }
            });
        }
    }
    if init.is_ok() {
        // SAFETY: balanced with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

struct Uia {
    automation: IUIAutomation,
    /// Tab strip element per top-level window (a descendant search is slow).
    strips: RefCell<HashMap<u64, IUIAutomationElement>>,
}

impl Uia {
    fn new() -> Result<Self, ShellError> {
        // SAFETY: valid CLSID; in-process server.
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| win_error("CoCreateInstance(CUIAutomation)", &e))?;
        Ok(Self {
            automation,
            strips: RefCell::new(HashMap::new()),
        })
    }

    /// `ElementFromHandle`, retried while it fails with [`UIA_TRANSIENT_ERROR`].
    fn element(&self, top: u64) -> Result<IUIAutomationElement, ShellError> {
        let mut attempt = 0;
        loop {
            // SAFETY: plain UIA call; stale HWNDs just fail.
            match unsafe { self.automation.ElementFromHandle(hwnd_of(top)) } {
                Ok(e) => return Ok(e),
                Err(e)
                    if e.code().0 as u32 == UIA_TRANSIENT_ERROR
                        && attempt + 1 < UIA_ELEMENT_ATTEMPTS =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(win_error("ElementFromHandle", &e)),
            }
        }
    }

    fn invoke_add(&self, top: u64) -> Result<(), ShellError> {
        let root = self.element(top)?;
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let cond = self
                .automation
                .CreatePropertyCondition(
                    UIA_AutomationIdPropertyId,
                    &VARIANT::from(UIA_ADD_TAB_AUTOMATION_ID),
                )
                .map_err(|e| win_error("CreatePropertyCondition", &e))?;
            let button = root
                .FindFirst(TreeScope_Descendants, &cond)
                .map_err(|e| win_error("FindFirst(AddButton)", &e))?;
            invoke(&button)
        }
    }

    /// Tab strip items (`ListViewItem` TabItems, excluding
    /// `NavigationViewItem`) of `top`, left to right.
    fn tab_items(&self, top: u64) -> Result<Vec<IUIAutomationElement>, ShellError> {
        let cached = self.strips.borrow().get(&top).cloned();
        if let Some(strip) = cached {
            match self.strip_items(&strip) {
                Ok(items) if !items.is_empty() => return Ok(items),
                _ => {
                    self.strips.borrow_mut().remove(&top);
                }
            }
        }
        let strip = unsafe {
            let tab_item = self
                .automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_TabItemControlTypeId.0),
                )
                .map_err(|e| win_error("CreatePropertyCondition", &e))?;
            let class = self
                .automation
                .CreatePropertyCondition(
                    UIA_ClassNamePropertyId,
                    &VARIANT::from(UIA_TAB_ITEM_CLASS),
                )
                .map_err(|e| win_error("CreatePropertyCondition", &e))?;
            let cond = self
                .automation
                .CreateAndCondition(&tab_item, &class)
                .map_err(|e| win_error("CreateAndCondition", &e))?;
            let walker = self
                .automation
                .RawViewWalker()
                .map_err(|e| win_error("RawViewWalker", &e))?;
            // Under the XAML island first: a search from the top-level
            // window walks every tab's file view (0.8-2.4 s).
            uia_strip::find_strip(&self.automation, &walker, &cond, hwnd_of(top))
                .map_err(|e| win_error("FindFirst(tab item)", &e))?
        };
        let items = self.strip_items(&strip)?;
        self.strips.borrow_mut().insert(top, strip);
        Ok(items)
    }

    fn strip_items(
        &self,
        strip: &IUIAutomationElement,
    ) -> Result<Vec<IUIAutomationElement>, ShellError> {
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let cond = self
                .automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_TabItemControlTypeId.0),
                )
                .map_err(|e| win_error("CreatePropertyCondition", &e))?;
            let all = strip
                .FindAll(TreeScope_Children, &cond)
                .map_err(|e| win_error("FindAll(tab items)", &e))?;
            let mut items = Vec::new();
            for i in 0..all.Length().map_err(|e| win_error("Length", &e))? {
                let el = all.GetElement(i).map_err(|e| win_error("GetElement", &e))?;
                let class = el
                    .CurrentClassName()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if is_tab_item_class(&class) {
                    items.push(el);
                }
            }
            Ok(items)
        }
    }

    /// Every tab item of `top` in visual order, realized or not
    /// ([`uia_strip::all_items`]); the exposed items without an
    /// `ItemContainerPattern`.
    fn all_tab_items(&self, top: u64) -> Result<Vec<IUIAutomationElement>, ShellError> {
        let exposed = self.tab_items(top)?; // finds and caches the strip
        let strip = self.strips.borrow().get(&top).cloned();
        match strip.and_then(|s| uia_strip::all_items(&s)) {
            Some(all) if !all.is_empty() => Ok(all),
            _ => Ok(exposed),
        }
    }

    /// The close button of `target`'s tab item, once the strip has as many
    /// items as `top` has tabs. `Err((retry, error))`: `retry` when the strip
    /// may still catch up.
    fn close_button(
        &self,
        top: u64,
        order: &[u64],
        target: u64,
        names: &[String],
        rightmost_only: bool,
    ) -> Result<IUIAutomationElement, (bool, ShellError)> {
        let live = tab_children(hwnd_of(top));
        let (index, count) = tab_position(order, &live, target).ok_or_else(|| {
            (false, error(0, format!("close: tab {target:#x} is not in its window")))
        })?;
        if rightmost_only && !unnamed_close_allowed(index, count) {
            return Err((
                false,
                error(
                    0,
                    format!("close: tab {target:#x} has no name and is not the rightmost of {count} tab(s)"),
                ),
            ));
        }
        let items = self.all_tab_items(top).map_err(|e| (true, e))?;
        if items.len() != count {
            return Err((
                true,
                error(
                    0,
                    format!(
                        "close: the tab strip has {} item(s) for {count} tab(s)",
                        items.len()
                    ),
                ),
            ));
        }
        // Never close a tab on position alone: the strip's item order can lag
        // behind the tracked order (found on build 26300, where the item at
        // the target's position was another tab's and that tab was closed).
        // The item must carry the tab's name, else it is the only item that
        // does, else the close is retried and finally fails (the restoration
        // then counts as failed and is retried) rather than closing a tab the
        // user has.
        let index = if names.is_empty() {
            index
        } else {
            uia_strip::realize(&items[index]);
            let item_name = |it: &IUIAutomationElement| -> String {
                // SAFETY: plain UIA property read on a live element.
                unsafe { it.CurrentName() }.map(|b| b.to_string()).unwrap_or_default()
            };
            let mut item_names = vec![String::new(); items.len()];
            item_names[index] = item_name(&items[index]);
            let positional = pick_close_item(&item_names, index, names);
            match positional {
                Some(i) => i,
                None => {
                    for (i, it) in items.iter().enumerate() {
                        if i != index {
                            item_names[i] = item_name(it);
                        }
                    }
                    pick_close_item(&item_names, index, names).ok_or_else(|| {
                        (
                            true,
                            error(
                                0,
                                format!(
                                    "close: the strip item at position {index} is named {:?}, not {:?}",
                                    item_names[index], names
                                ),
                            ),
                        )
                    })?
                }
            }
        };
        uia_strip::realize(&items[index]);
        // SAFETY: plain UIA calls on live interfaces.
        unsafe {
            let cond = self
                .automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_ButtonControlTypeId.0),
                )
                .map_err(|e| (false, win_error("CreatePropertyCondition", &e)))?;
            items[index]
                .FindFirst(TreeScope_Descendants, &cond)
                .map_err(|e| (true, win_error("FindFirst(close button)", &e)))
        }
    }

    fn close_tab(
        &self,
        top: u64,
        order: &[u64],
        target: u64,
        names: &[String],
        rightmost_only: bool,
    ) -> Result<(), ShellError> {
        // The strip lags behind tab creation: wait (with backoff) until it
        // has an item for every tab, so the item at the tab's position is the tab.
        let catch_up = Instant::now() + Duration::from_millis(UIA_CLOSE_CATCH_UP_MS);
        let mut attempt = 0;
        let button = loop {
            match self.close_button(top, order, target, names, rightmost_only) {
                Ok(button) => break button,
                Err((true, _)) if Instant::now() < catch_up => {
                    std::thread::sleep(uia_strip::backoff(attempt));
                    attempt += 1;
                }
                Err((_, e)) => return Err(e),
            }
        };
        invoke(&button)?;
        let deadline = Instant::now() + Duration::from_millis(UIA_CLOSE_WAIT_MS);
        while tab_children(hwnd_of(top)).contains(&target) {
            if Instant::now() >= deadline {
                return Err(error(0, format!("close: tab {target:#x} did not close")));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

/// Which tab item to close: the one at `index` if its name is one of
/// `expected` (compared without case and without a trailing `\`), else the
/// only item whose name is, else none. Empty expected names match nothing.
pub fn pick_close_item(item_names: &[String], index: usize, expected: &[String]) -> Option<usize> {
    let norm = |s: &str| s.trim_end_matches('\\').to_lowercase();
    let want: Vec<String> = expected.iter().map(|s| norm(s)).filter(|s| !s.is_empty()).collect();
    let hit = |i: usize| item_names.get(i).is_some_and(|n| !n.is_empty() && want.contains(&norm(n)));
    if hit(index) {
        return Some(index);
    }
    let mut hits = (0..item_names.len()).filter(|&i| hit(i));
    match (hits.next(), hits.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Whether a tab with no known name may be closed at visual position `index`
/// of `count` tabs: only the rightmost one, since that is where Explorer
/// appends a new tab (R2, R9). The late tab of a stopped fold (Req 14.13) is
/// closed before it registered, so the strip item cannot be matched by name;
/// closing any other position could close a tab the user has.
pub fn unnamed_close_allowed(index: usize, count: usize) -> bool {
    count > 0 && index + 1 == count
}

/// `ListViewItem` tab items are tabs; `NavigationViewItem` ones are not.
pub fn is_tab_item_class(class: &str) -> bool {
    class.contains(UIA_TAB_ITEM_CLASS) && !class.contains(UIA_EXCLUDED_ITEM_CLASS)
}

fn invoke(element: &IUIAutomationElement) -> Result<(), ShellError> {
    // SAFETY: plain UIA calls on a live element.
    unsafe {
        let pattern: IUIAutomationInvokePattern = element
            .GetCurrentPattern(UIA_InvokePatternId)
            .map_err(|e| win_error("GetCurrentPattern(Invoke)", &e))?
            .cast()
            .map_err(|e| win_error("IUIAutomationInvokePattern", &e))?;
        pattern.Invoke().map_err(|e| win_error("Invoke", &e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_core::model::{Rect, Volume};

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn placement(state: ShowState) -> FinalPlacement {
        FinalPlacement {
            state,
            restored: rect(100, 140, 900, 740),
            monitor: rect(0, 0, 1920, 1080),
            // Taskbar on the left (60 px) and top (40 px): workspace offset (60, 40).
            work: rect(60, 40, 1920, 1080),
        }
    }

    fn loc(path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(Volume::Fixed),
            parsing_name: WideString(path.encode_utf16().collect()),
            display_name: WideString::default(),
            pidl: Vec::new(),
            search: None,
        }
    }

    fn open(id: u64, path: &str, at: Instant) -> PendingOpen {
        PendingOpen {
            request: RequestId(id),
            location: loc(path),
            placement: FinalPlacement {
                restored: rect(id as i32, 0, id as i32 + 500, 400),
                ..placement(ShowState::Normal)
            },
            requested_at: at,
            window: None,
        }
    }

    #[test]
    fn hidden_phase_is_normal_noactivate_in_workspace_coordinates() {
        for state in [
            ShowState::Normal,
            ShowState::Maximized,
            ShowState::Minimized {
                restore_maximized: true,
            },
        ] {
            let wp = window_placement_for(&placement(state), PlacementPhase::Hidden).unwrap();
            assert_eq!(wp.length, size_of::<WINDOWPLACEMENT>() as u32);
            assert_eq!(wp.showCmd, SW_SHOWNOACTIVATE.0 as u32);
            assert_eq!(wp.flags.0, 0);
            let r = wp.rcNormalPosition;
            assert_eq!((r.left, r.top, r.right, r.bottom), (40, 100, 840, 700));
        }
    }

    #[test]
    fn final_phase_show_commands() {
        let normal = window_placement_for(&placement(ShowState::Normal), PlacementPhase::Final);
        assert_eq!(normal.unwrap().showCmd, SW_SHOWNOACTIVATE.0 as u32);
        let max =
            window_placement_for(&placement(ShowState::Maximized), PlacementPhase::Final).unwrap();
        assert_eq!(max.showCmd, SW_SHOWMAXIMIZED.0 as u32);
        assert_eq!(max.flags.0, 0);
        let min_max = window_placement_for(
            &placement(ShowState::Minimized {
                restore_maximized: true,
            }),
            PlacementPhase::Final,
        )
        .unwrap();
        assert_eq!(min_max.showCmd, SW_SHOWMINNOACTIVE.0 as u32);
        assert_eq!(min_max.flags, WPF_RESTORETOMAXIMIZED);
        let min = window_placement_for(
            &placement(ShowState::Minimized {
                restore_maximized: false,
            }),
            PlacementPhase::Final,
        )
        .unwrap();
        assert_eq!(min.showCmd, SW_SHOWMINNOACTIVE.0 as u32);
        assert_eq!(min.flags.0, 0);
    }

    #[test]
    fn placement_out_of_range_is_none() {
        let p = FinalPlacement {
            restored: rect(i32::MIN, 0, 10, 10),
            ..placement(ShowState::Normal)
        };
        assert!(window_placement_for(&p, PlacementPhase::Final).is_none());
    }

    #[test]
    fn select_flags_first_replaces_and_focuses() {
        let first = select_flags(0);
        for f in [
            SVSI_SELECT,
            SVSI_DESELECTOTHERS,
            SVSI_ENSUREVISIBLE,
            SVSI_FOCUSED,
        ] {
            assert_ne!(first & f.0 as u32, 0);
        }
        assert_eq!(select_flags(1), SVSI_SELECT.0 as u32);
        assert_eq!(select_flags(7), SVSI_SELECT.0 as u32);
    }

    #[test]
    fn z_order_raises_last_first() {
        assert_eq!(z_order_calls(&[1, 2, 3]), vec![3, 2, 1]);
        assert!(z_order_calls::<u8>(&[]).is_empty());
    }

    #[test]
    fn create_attempts_repost_once_then_uia_then_give_up() {
        let a = CreateAttempt::first(TabCreateStrategy::DEFAULT);
        assert_eq!(a, CreateAttempt::WmCommand(1));
        assert_eq!(a.wait(), Duration::from_millis(NEW_TAB_REPOST_AFTER_MS));
        let b = a.next().unwrap();
        assert_eq!(b, CreateAttempt::WmCommand(2));
        assert_eq!(b.number(), 2);
        let c = b.next().unwrap();
        assert_eq!(c, CreateAttempt::UiaAddButton);
        assert_eq!(c.strategy(), TabCreateStrategy::FALLBACK);
        assert_eq!(c.number(), 3);
        assert_eq!(c.wait(), Duration::from_millis(UIA_ADD_TAB_WAIT_MS));
        assert_eq!(c.next(), None);
        assert_eq!(
            CreateAttempt::first(TabCreateStrategy::UiaAddButton).next(),
            None
        );
    }

    #[test]
    fn tab_position_drops_gone_and_appends_new() {
        // Tracked a b c d; b closed meanwhile, e created.
        let order = [1, 2, 3, 4];
        let live = [5, 4, 3, 1];
        assert_eq!(tab_position(&order, &live, 3), Some((1, 4)));
        assert_eq!(tab_position(&order, &live, 5), Some((3, 4)));
        assert_eq!(tab_position(&order, &live, 2), None);
        assert_eq!(tab_position(&[], &[9], 9), Some((0, 1)));
    }

    #[test]
    fn merge_tab_order_keeps_order_and_appends_new_reversed() {
        let mut order = vec![1, 2, 3];
        merge_tab_order(&mut order, &[5, 4, 3, 1]);
        assert_eq!(order, vec![1, 3, 4, 5]);
        let mut empty = Vec::new();
        merge_tab_order(&mut empty, &[7]);
        assert_eq!(empty, vec![7]);
    }

    #[test]
    fn tab_item_class_filter() {
        assert!(is_tab_item_class("ListViewItem"));
        assert!(!is_tab_item_class("NavigationViewItem"));
        assert!(!is_tab_item_class("Button"));
    }

    #[test]
    fn open_queue_assigns_fifo_and_claims_own_window() {
        let t = Instant::now();
        let mut q = OpenQueue::new();
        q.push(open(1, r"C:\a", t));
        q.push(open(2, r"C:\b", t));
        let alive = |_| true;
        assert_eq!(q.on_created(0x10, alive).map(|a| a.0), Some(RequestId(1)));
        assert_eq!(q.on_created(0x10, alive), None, "a window is assigned once");
        assert_eq!(q.on_created(0x20, alive).map(|a| a.0), Some(RequestId(2)));
        assert_eq!(q.on_created(0x30, alive), None, "no request left to assign");
        assert_eq!(q.assigned(RequestId(1)), Some(0x10));

        let c = q.claim(0x10, &loc(r"C:\a"), alive);
        assert_eq!(
            c.claimed.map(|(r, _, changed)| (r, changed)),
            Some((RequestId(1), false))
        );
        assert!(c.reassigned.is_empty() && !c.unmatched);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn open_queue_swaps_windows_created_out_of_order() {
        let t = Instant::now();
        let mut q = OpenQueue::new();
        q.push(open(1, r"C:\a", t));
        q.push(open(2, r"C:\b", t));
        let alive = |_| true;
        q.on_created(0x10, alive); // request 1 (but shows b)
        q.on_created(0x20, alive); // request 2 (but shows a)
        let c = q.claim(0x10, &loc(r"C:\b"), alive);
        let (r, p, changed) = c.claimed.unwrap();
        assert_eq!((r, changed), (RequestId(2), true));
        assert_eq!(p.restored.left, 2);
        // 0x20 now belongs to request 1 and gets its placement.
        assert_eq!(c.reassigned.len(), 1);
        assert_eq!(c.reassigned[0].0, 0x20);
        assert_eq!(c.reassigned[0].1.restored.left, 1);
        let c = q.claim(0x20, &loc(r"C:\a"), alive);
        assert_eq!(
            c.claimed.map(|(r, _, ch)| (r, ch)),
            Some((RequestId(1), false))
        );
        assert!(q.is_empty());
    }

    #[test]
    fn open_queue_releases_unmatched_window_and_frees_its_request() {
        let t = Instant::now();
        let mut q = OpenQueue::new();
        q.push(open(1, r"C:\a", t));
        let alive = |_| true;
        q.on_created(0x10, alive); // a user window
        let c = q.claim(0x10, &loc(r"D:\user"), alive);
        assert!(c.unmatched && c.claimed.is_none());
        assert_eq!(q.assigned(RequestId(1)), None);
        // The real window arrives later and is claimed with a placement change
        // only if it was not assigned (it is assigned at CREATE first).
        assert_eq!(q.on_created(0x20, alive).map(|a| a.0), Some(RequestId(1)));
        let c = q.claim(0x20, &loc(r"C:\a"), alive);
        assert_eq!(
            c.claimed.map(|(r, _, ch)| (r, ch)),
            Some((RequestId(1), false))
        );
    }

    #[test]
    fn open_queue_claims_unassigned_request_for_unhidden_window() {
        let t = Instant::now();
        let mut q = OpenQueue::new();
        q.push(open(1, r"C:\a", t));
        // CREATE was missed: the window is claimed directly; placement must be applied.
        let c = q.claim(0x10, &loc(r"C:\a"), |_| true);
        assert_eq!(
            c.claimed.map(|(r, _, ch)| (r, ch)),
            Some((RequestId(1), true))
        );
    }

    #[test]
    fn open_queue_frees_requests_of_destroyed_windows() {
        let t = Instant::now();
        let mut q = OpenQueue::new();
        q.push(open(1, r"C:\a", t));
        q.on_created(0x10, |_| true);
        // Explorer replaced the fresh window 0x10 with 0x20.
        let alive = |h| h != 0x10;
        assert_eq!(q.on_created(0x20, alive).map(|a| a.0), Some(RequestId(1)));
        assert_eq!(q.assigned(RequestId(1)), Some(0x20));
    }

    #[test]
    fn open_queue_expires_old_requests() {
        let t = Instant::now();
        let mut q = OpenQueue::new();
        q.push(open(1, r"C:\a", t));
        q.push(open(2, r"C:\b", t + Duration::from_secs(5)));
        let gone = q.expire(
            t + Duration::from_secs(10),
            Duration::from_millis(OPEN_REQUEST_EXPIRY_MS),
        );
        assert_eq!(
            gone.iter().map(|p| p.request).collect::<Vec<_>>(),
            vec![RequestId(1)]
        );
        assert_eq!(q.len(), 1);
        assert!(q.remove(RequestId(2)).is_some());
        assert!(q.is_empty());
    }

    #[test]
    fn location_pidl_binds_a_folder_and_falls_back_to_stored_pidl() {
        let _sta = StaGuard::init().unwrap();
        let dir = std::env::temp_dir();
        let pidl = location_pidl(&loc(&dir.to_string_lossy())).expect("bind temp dir");
        assert!(pidl_is_well_formed(&pidl) && pidl.len() > 2);
        let mut missing = loc(r"Z:\definitely\not\here\esr");
        missing.pidl = pidl.clone();
        assert_eq!(location_pidl(&missing).unwrap(), pidl);
        assert!(location_pidl(&loc("")).is_err());
    }

    #[test]
    fn location_pidl_of_a_search_is_the_native_search_folder() {
        use esr_core::model::SearchSpec;
        let _sta = StaGuard::init().unwrap();
        let dir = std::env::temp_dir();
        let mut search = loc("search-ms:stale");
        search.kind = LocationKind::Search;
        search.search = Some(SearchSpec {
            query: "needle caf\u{e9}".into(),
            scope: Box::new(loc(&dir.to_string_lossy())),
        });
        let pidl = location_pidl(&search).expect("bind search");
        assert!(pidl_is_well_formed(&pidl) && pidl.len() > 2);
        // Not the Internet-URL item that Explorer opens in a new window.
        assert!(!is_internet_url_pidl(&pidl), "{pidl:?}");
        // A stored Internet-URL PIDL is never used as the fallback.
        let mut url_item = vec![20u8, 0, 0x1F, 0x68];
        url_item.extend(crate::shell_util::INTERNET_FOLDER_ITEM_PREFIX);
        url_item.extend([0, 0]);
        assert!(is_internet_url_pidl(&url_item));
        let mut gone = loc("");
        gone.pidl = url_item;
        assert!(location_pidl(&gone).is_err());
    }

    #[test]
    fn start_stop_and_errors_without_explorer() {
        let _sta = StaGuard::init().unwrap();
        let signals: Rc<RefCell<Vec<NavigatorSignal>>> = Rc::default();
        let sink = signals.clone();
        let mut nav = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(move |s: NavigatorSignal| sink.borrow_mut().push(s)),
        );
        let bogus_win = WinKey { hwnd: 0x1, seq: 1 };
        let bogus_tab = TabKey { hwnd: 0x1, seq: 1 };
        assert!(
            nav.open_window(RequestId(1), &loc(r"C:\"), &placement(ShowState::Normal))
                .is_err(),
            "not started"
        );
        nav.start().unwrap();
        assert!(nav.is_started());
        nav.start().unwrap();
        let mut second = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(|_: NavigatorSignal| {}),
        );
        assert!(second.start().is_err(), "one navigator per thread");
        assert!(nav.create_tab(bogus_win).is_err());
        assert!(nav.reveal(bogus_win).is_err());
        assert!(nav.focus(bogus_win).is_err());
        assert!(nav.set_z_order(&[bogus_win]).is_err());
        assert!(nav.set_z_order(&[]).is_ok());
        assert!(nav.navigate(bogus_tab, &loc(r"C:\")).is_err());
        assert!(nav.close_tab(bogus_tab).is_err());
        assert!(nav.select_items(bogus_tab, &[]).is_err());
        assert_eq!(nav.claim(bogus_win, &loc(r"C:\")), None);
        assert_eq!(nav.pending_opens(), 0);
        nav.stop();
        assert!(!nav.is_started());
        assert!(signals.borrow().is_empty());
        drop(second);
    }

    /// Task 16.3: a tab whose new-tab command was settled by its window
    /// appearing is still "being created" until it registers, so the shell
    /// window source keeps skipping its cross-process reads.
    #[test]
    fn a_tab_counts_as_being_created_until_it_registers() {
        let nav = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(|_: NavigatorSignal| {}),
        );
        let handle = nav.handle();
        let win = WinKey { hwnd: 0x5150, seq: 3 };
        let other = WinKey { hwnd: 0x5150, seq: 4 };
        let tab = TabKey { hwnd: 0x5151, seq: 1 };
        assert!(!handle.creating_tab(win));

        let outstanding = |nav: &ShellNavigator| {
            nav.inner.state.borrow_mut().creates.insert(
                win.hwnd,
                PendingCreate {
                    key: win,
                    baseline: Vec::new(),
                    attempt: CreateAttempt::WmCommand(1),
                    deadline: Instant::now() + Duration::from_secs(1),
                },
            );
        };
        outstanding(&nav);
        assert!(nav.is_creating_tab(win) && handle.creating_tab(win));
        // A later window that reuses the HWND value is not the one creating.
        assert!(!handle.creating_tab(other));

        // The tab's window appeared: the command is settled ...
        nav.inner.settle_create(win.hwnd, win, &[tab.hwnd]);
        assert!(!nav.is_creating_tab(win));
        // ... but the tab is the navigator's own until it registers.
        assert!(handle.creating_tab(win));
        assert!(!handle.creating_tab(other));
        assert_eq!(nav.inner.state.borrow().managed[&win.hwnd].order, vec![tab.hwnd]);

        handle.tab_registered(win, tab);
        assert!(!handle.creating_tab(win));

        // A destroyed window is no longer creating either way.
        outstanding(&nav);
        nav.inner.settle_create(win.hwnd, win, &[]);
        assert!(handle.creating_tab(win));
        handle.window_destroyed(win);
        assert!(!handle.creating_tab(win));

        // While the state is borrowed (a re-entrant call), the answer is false.
        outstanding(&nav);
        let guard = nav.inner.state.borrow_mut();
        assert!(!handle.creating_tab(win));
        drop(guard);
        assert!(handle.creating_tab(win));
        drop(nav);
        assert!(!handle.creating_tab(win), "dropped navigator");
    }

    /// Without a started navigator (no workers) or with the workers switched
    /// off, `attach_browser` declines and the caller attaches the sink itself.
    #[test]
    fn attach_browser_declines_without_workers() {
        let nav = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(|_: NavigatorSignal| {}),
        );
        assert!(nav.inner.state.borrow().workers.is_none());
        let handle = nav.handle();
        // `detach_browser` of an unknown tab is a no-op.
        handle.detach_browser(TabKey { hwnd: 0x77, seq: 1 });
        drop(nav);
        handle.detach_browser(TabKey { hwnd: 0x77, seq: 1 });
    }

    #[test]
    fn new_tab_among_picks_the_earliest_created_new_tab() {
        // Enumeration order lists the newest child first.
        assert_eq!(new_tab_among(&[1, 2], &[3, 2, 1]), Some(3));
        assert_eq!(new_tab_among(&[1], &[4, 3, 1]), Some(3), "earliest of several new tabs");
        assert_eq!(new_tab_among(&[1, 2], &[2, 1]), None);
        assert_eq!(new_tab_among(&[1, 2], &[2]), None, "a closed tab is not a new one");
        assert_eq!(new_tab_among(&[], &[9]), Some(9));
        assert_eq!(new_tab_among(&[], &[]), None);
    }

    #[test]
    fn close_window_posts_sc_close() {
        assert_eq!(CLOSE_WINDOW_SYSCOMMAND, 0xF060);
    }

    /// A tab that registers before the timer saw its window reports
    /// `TabStarted` once, then the command is settled; one the timer already
    /// settled does not report it again.
    #[test]
    fn tab_registered_reports_the_start_of_an_unsettled_command_once() {
        let signals: Rc<RefCell<Vec<NavigatorSignal>>> = Rc::default();
        let sink = signals.clone();
        let nav = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(move |s: NavigatorSignal| sink.borrow_mut().push(s)),
        );
        let win = WinKey { hwnd: 0x6150, seq: 1 };
        let tab = TabKey { hwnd: 0x6151, seq: 1 };
        let handle = nav.handle();
        nav.inner.state.borrow_mut().creates.insert(
            win.hwnd,
            PendingCreate {
                key: win,
                baseline: Vec::new(),
                attempt: CreateAttempt::WmCommand(1),
                deadline: Instant::now() + Duration::from_secs(1),
            },
        );
        handle.tab_registered(win, tab);
        assert_eq!(*signals.borrow(), vec![NavigatorSignal::TabStarted { win, tab }]);
        assert!(!nav.is_creating_tab(win));
        // Registering again (or a tab nobody asked for) reports nothing.
        handle.tab_registered(win, tab);
        assert_eq!(signals.borrow().len(), 1);

        // Settled by the timer first (the signal came from `tick`).
        signals.borrow_mut().clear();
        nav.inner.state.borrow_mut().creates.insert(
            win.hwnd,
            PendingCreate {
                key: win,
                baseline: Vec::new(),
                attempt: CreateAttempt::WmCommand(1),
                deadline: Instant::now() + Duration::from_secs(1),
            },
        );
        nav.inner.settle_create(win.hwnd, win, &[tab.hwnd]);
        handle.tab_registered(win, tab);
        assert!(signals.borrow().is_empty());
    }

    #[test]
    fn a_tab_without_a_name_is_closed_only_when_it_is_the_rightmost() {
        assert!(unnamed_close_allowed(0, 1));
        assert!(unnamed_close_allowed(3, 4));
        assert!(!unnamed_close_allowed(2, 4));
        assert!(!unnamed_close_allowed(0, 2));
        assert!(!unnamed_close_allowed(0, 0));
    }

    /// The late tab of a stopped fold is closed before it registers, so it
    /// never will: a successful close of such a tab ends the window's
    /// "creating a tab" state, while the close of an ordinary tab leaves a
    /// tab that is still registering alone (Req 14.13).
    #[test]
    fn closing_a_tab_that_never_registered_ends_the_creating_state() {
        let nav = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(|_: NavigatorSignal| {}),
        );
        let handle = nav.handle();
        let win = WinKey { hwnd: 0x7150, seq: 1 };
        let late = TabKey { hwnd: 0x7151, seq: 1 };
        let other = TabKey { hwnd: 0x7152, seq: 1 };
        nav.inner.state.borrow_mut().asyncs = Some(Arc::new(AsyncQueue {
            window: 0,
            queue: Mutex::new(VecDeque::new()),
        }));
        let queue = nav.inner.state.borrow().asyncs.clone().unwrap();
        nav.inner.settle_create(win.hwnd, win, &[late.hwnd]);
        assert!(handle.creating_tab(win));

        // An ordinary tab's close does not touch the registering tab.
        queue.queue.lock().unwrap().push_back(AsyncResult::TabClose {
            top: win.hwnd,
            tab: other,
            unregistered: false,
            result: Ok(()),
        });
        nav.inner.drain_async();
        assert!(handle.creating_tab(win));

        // A failed close of the late tab leaves it too.
        queue.queue.lock().unwrap().push_back(AsyncResult::TabClose {
            top: win.hwnd,
            tab: late,
            unregistered: true,
            result: Err(error(0, "x".into())),
        });
        nav.inner.drain_async();
        assert!(handle.creating_tab(win));

        queue.queue.lock().unwrap().push_back(AsyncResult::TabClose {
            top: win.hwnd,
            tab: late,
            unregistered: true,
            result: Ok(()),
        });
        nav.inner.drain_async();
        assert!(!handle.creating_tab(win));
    }

    /// A `CabinetWClass` window of this process stands in for an Explorer
    /// window the navigator did not open (a Fold_Target).
    unsafe extern "system" fn default_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        // SAFETY: default processing for the test window.
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }

    fn foreign_cabinet_window() -> HWND {
        // SAFETY: standard class registration (a repeat fails harmlessly) and
        // creation of a window owned by the calling thread.
        unsafe {
            let module = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                lpfnWndProc: Some(default_proc),
                hInstance: module.into(),
                lpszClassName: w!("CabinetWClass"),
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("CabinetWClass"),
                w!("esr navigator test"),
                windows::Win32::UI::WindowsAndMessaging::WS_OVERLAPPEDWINDOW,
                0,
                0,
                400,
                300,
                None,
                None,
                Some(module.into()),
                None,
            )
            .unwrap()
        }
    }

    /// Req 14.7: a minimized Fold_Target the navigator never opened or hid is
    /// shown in its pre-minimize state (normal at its restored bounds, or
    /// maximized) by `apply_placement`, and `reveal` and `focus` work on it.
    #[test]
    fn placement_focus_and_reveal_work_on_a_window_the_navigator_did_not_open() {
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsZoomed, SW_MINIMIZE, ShowWindow};
        let mut nav = ShellNavigator::new(
            Strategies::DEFAULT,
            WindowKeys::new(),
            TabKeys::new(),
            Rc::new(|_: NavigatorSignal| {}),
        );
        let hwnd = foreign_cabinet_window();
        let win = WinKey { hwnd: hwnd_value(hwnd), seq: 1 };
        // SAFETY: plain reads on the window created above.
        let iconic = || unsafe { IsIconic(hwnd) }.as_bool();
        // SAFETY: as above.
        let zoomed = || unsafe { IsZoomed(hwnd) }.as_bool();
        // SAFETY: as above.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        // SAFETY: as above.
        let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        assert!(iconic());

        // Pre-minimize state: normal at the restored bounds.
        nav.apply_placement(win, &placement(ShowState::Normal)).unwrap();
        assert!(!iconic() && !zoomed());
        let r = get_placement(hwnd).unwrap().rcNormalPosition;
        assert_eq!((r.left, r.top, r.right, r.bottom), (40, 100, 840, 700));
        assert!(!nav.is_hidden(win), "the navigator does not hide what it did not open");

        // Pre-minimize state: maximized.
        // SAFETY: as above.
        let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        assert!(iconic());
        nav.apply_placement(win, &placement(ShowState::Maximized)).unwrap();
        assert!(!iconic() && zoomed());

        // Nothing hidden: reveal changes nothing; focus is Ok whichever way it ends.
        nav.reveal(win).unwrap();
        assert!(crate::shell_util::is_window_visible(hwnd));
        assert!(nav.focus(win).is_ok());
        // Selection and tab operations name tabs, which this window has none of.
        assert!(nav.select_items(TabKey { hwnd: win.hwnd, seq: 1 }, &[]).is_err());

        // SAFETY: destroys the window created above, once.
        unsafe { DestroyWindow(hwnd) }.unwrap();
        assert!(nav.apply_placement(win, &placement(ShowState::Normal)).is_err());
        assert!(nav.focus(win).is_err());
    }
}

#[cfg(test)]
mod close_pick_tests {
    use super::pick_close_item;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_item_at_the_position_wins_when_its_name_matches() {
        let items = v(&["a", r"C:\Users\x\Docs", "c"]);
        assert_eq!(pick_close_item(&items, 1, &v(&[r"c:\users\x\docs\", "Docs"])), Some(1));
    }

    #[test]
    fn a_wrong_item_at_the_position_is_replaced_by_the_only_matching_one() {
        let items = v(&["Docs", "Music", "Pictures"]);
        assert_eq!(pick_close_item(&items, 1, &v(&[r"C:\x\Pictures", "Pictures"])), Some(2));
    }

    #[test]
    fn nothing_is_picked_when_no_item_or_several_items_match() {
        let items = v(&["Docs", "Music", "Docs"]);
        assert_eq!(pick_close_item(&items, 1, &v(&["Pictures"])), None);
        assert_eq!(pick_close_item(&items, 1, &v(&["Docs"])), None);
    }

    #[test]
    fn empty_names_match_nothing() {
        let items = v(&["", "Docs"]);
        assert_eq!(pick_close_item(&items, 0, &v(&[""])), None);
        assert_eq!(pick_close_item(&items, 0, &[]), None);
    }
}