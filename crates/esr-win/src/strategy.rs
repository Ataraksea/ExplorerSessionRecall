//! Strategy selections derived from the verification spikes R1–R9
//! (`HideStrategy`, `TabCreateStrategy`, `TabCloseStrategy`,
//! `TabCreationPipelining`, `EndKindSource`).
//!
//! Requirements 7.1, 7.2, 7.7, 7.9, 13.1.
//!
//! The spikes ran on two machines: an older host on build 26200 and a
//! Windows 11 VM on build 22621.3880. Each enum records the selected
//! approach as its `DEFAULT`. Adapters take a [`Strategies`] value so tests
//! can override a selection. To change a default, rerun the spike and update
//! the matching unit test at the bottom of this file in the same change.
//!
//! This module does not use any `windows` crate types, so it stays trivially
//! testable.
//!
//! # Findings that are not strategy enums
//!
//! * **R5 (tab drag).** `SendInput` drags work. A dragged tab's
//!   `ShellTabWindowClass` HWND is destroyed and recreated in the target
//!   window, for both drag-out and drag-in. Tabs that were not dragged keep
//!   their HWNDs. A drag-out creates a new `CabinetWClass` window, which
//!   appears in `ShellWindows` about 200 ms later. A source window left
//!   with no tabs closes itself. The tracker therefore must not assume a tab
//!   HWND survives a drag. It must match moved tabs by location and UIA name
//!   (see [`TAB_HWND_STABLE_ACROSS_DRAG`]).
//! * **R6 (LaunchTo).** Task 9.1 (`esr_core::launch`) owns this mapping.
//!   Measured results: on 22621, an absent value or 0/2/5 gives Home
//!   (`::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}`), 1 gives This PC
//!   (`::{20D04FE0-3AEA-1069-A2D8-08002B30309D}`), and 3 gives Downloads.
//!   Downloads is reported as a file-system path, so resolve
//!   `FOLDERID_Downloads` `{374DE290-123F-4565-9164-39C4925E467B}`. On both
//!   builds, 4 gives Home, not OneDrive. On 26200, 1 also opened Home.
//! * **R8 (built-in restore heuristic, Req 10.4).** A standard user can read
//!   every input: PersistBrowsers, `WTSSessionInfo` `LogonTime`,
//!   `GetLastInputInfo`, and the shell process creation time. The
//!   true-positive case (built-in restore right after sign-in) was not
//!   tested.
//! * **R9 (30-tab timing, Req 7.1).** The best correct variant opens all
//!   windows at once, keeps at most one `0xA21B` outstanding per window, and
//!   navigates each new tab as soon as it registers, without waiting for
//!   its navigation to finish. It took a median of 5.3 s for 3 windows × 10
//!   tabs and about 4.0 s for 1 window × 30 tabs. **This does not meet the
//!   3 s budget in Req 7.1.** That is an open decision for checkpoint 3. The
//!   requirement is unchanged here. See [`OVERLAP_NAVIGATION_WITH_CREATION`]
//!   and [`OPEN_WINDOWS_IN_PARALLEL`].
//!
//! # UIA and window-message observations
//!
//! * Tab-strip items have UIA class `ListViewItem`. Exclude
//!   `NavigationViewItem` ([`UIA_TAB_ITEM_CLASS`],
//!   [`UIA_EXCLUDED_ITEM_CLASS`]).
//! * Tab names are full paths on 26200 but display names on 22621.
//! * Cache the tab-strip UIA element. A descendant search costs 0.8–2.4 s;
//!   reading the cached element's children costs 2–7 ms.
//! * An overflowing tab strip virtualises its items: only 18–24 of 30 were
//!   exposed.
//! * `ElementFromHandle` can fail transiently with `0x80040201`; retry it
//!   ([`UIA_TRANSIENT_ERROR`]).
//! * `WM_CLOSE` on a window with several tabs closes only one tab.
//! * Window visibility and `QueryActiveShellView` cannot identify the active
//!   tab. Only the UIA `SelectionItemPattern` can.

/// How a restored window is kept invisible until it is revealed (R1,
/// Req 7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HideStrategy {
    /// Open the window with `nShow = SW_HIDE`. Explorer honours it for
    /// `opennewwindow`: the window is created and stays invisible (no
    /// frame, no taskbar button) while its tabs are created, navigated and
    /// activated (WM_COMMAND, `Navigate2` and UIA all work on it). Reveal is
    /// one `SetWindowPlacement` with the final placement, which shows it.
    LaunchHidden,
    /// At `EVENT_OBJECT_CREATE`, in one callback: add `WS_EX_LAYERED`, set
    /// alpha 0, and call `SetWindowPlacement`. To reveal, set alpha 255 and
    /// remove `WS_EX_LAYERED`. Racy: `EVENT_OBJECT_CREATE` fires only when
    /// `CreateWindowEx` returns, and the cross-process style change takes
    /// effect only when Explorer's window thread pumps. Under load
    /// (3 windows opened at once) Explorer showed the window before either
    /// happened, for ~0.6 s at its default position (task 16.3).
    LayeredAlpha,
    /// Only set the placement. The window is visible while tabs are built.
    PlaceOnlyNoHide,
}

impl HideStrategy {
    /// R1 passed `LayeredAlpha` for single windows on 26200 and 22621, but
    /// task 16.3 found it racy under load; `LaunchHidden` never showed a
    /// window before its reveal.
    pub const DEFAULT: Self = Self::LaunchHidden;
}

/// How a new tab is added to an existing window (R2/R4, Req 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCreateStrategy {
    /// Post `WM_COMMAND` [`NEW_TAB_COMMAND`] to a `ShellTabWindowClass` or
    /// to the top-level `CabinetWClass`. The new tab is added rightmost and
    /// selected.
    WmCommand,
    /// Invoke the UIA button with AutomationId [`UIA_ADD_TAB_AUTOMATION_ID`].
    UiaAddButton,
}

impl TabCreateStrategy {
    /// `0xA21B` took about 84–99 ms per tab on 22621. The UIA add button
    /// works but takes about 430 ms.
    pub const DEFAULT: Self = Self::WmCommand;
    /// Used when `WmCommand` produces no tab even after the re-post.
    pub const FALLBACK: Self = Self::UiaAddButton;
}

/// How a single tab is closed (R3, Req 7.7/7.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCloseStrategy {
    /// `IWebBrowser2::Quit`. Does nothing on tabs that are not active, and
    /// sometimes closed the whole window on 26200. Only works on the active
    /// tab or on a window's only tab.
    Quit,
    /// Invoke the tab's UIA "Close Tab" button through `InvokePattern`.
    UiaCloseButton,
}

impl TabCloseStrategy {
    /// The UIA close button worked reliably (about 220–370 ms per tab).
    pub const DEFAULT: Self = Self::UiaCloseButton;
}

/// Whether several new-tab commands may be outstanding in one window at the
/// same time (R9, Req 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCreationPipelining {
    /// At most one [`NEW_TAB_COMMAND`] outstanding per window. The next one
    /// is posted after the previous tab registers.
    Sequential,
    /// Back-to-back posts without waiting.
    Pipelined,
}

impl TabCreationPipelining {
    /// With `Pipelined`, every new tab header except the first kept showing
    /// "Home" permanently, even though the tab contents showed the correct
    /// folders. `Pipelined` cannot be used.
    pub const DEFAULT: Self = Self::Sequential;
}

/// How the Session_End kind (restart or power off) is determined at shutdown
/// (R7, Req 13.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndKindSource {
    /// Read System-log event 1074. Param5 ("restart" / "power off") is
    /// localized. An event older than [`EVENT_1074_MAX_AGE_MS`] is stale
    /// and rejected. If the event is unreadable, stale, or in an unknown
    /// language, fall back to `AssumeShutdown`.
    Event1074,
    /// Always report "Windows shutdown".
    AssumeShutdown,
}

impl EndKindSource {
    /// Event 1074 can be read in about 1 ms. Reading it as a standard user
    /// was not verified directly: UAC was off on 26200. The System log ACL
    /// grants read access to Interactive users.
    pub const DEFAULT: Self = Self::Event1074;
    /// Used when the event is unreadable, stale, or its language is unknown.
    pub const FALLBACK: Self = Self::AssumeShutdown;
}

/// All strategy selections together, so adapters can take an override in
/// tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Strategies {
    pub hide: HideStrategy,
    pub tab_create: TabCreateStrategy,
    pub tab_close: TabCloseStrategy,
    pub tab_pipelining: TabCreationPipelining,
    pub end_kind: EndKindSource,
}

impl Strategies {
    pub const DEFAULT: Strategies = Strategies {
        hide: HideStrategy::DEFAULT,
        tab_create: TabCreateStrategy::DEFAULT,
        tab_close: TabCloseStrategy::DEFAULT,
        tab_pipelining: TabCreationPipelining::DEFAULT,
        end_kind: EndKindSource::DEFAULT,
    };
}

impl Default for Strategies {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// ---------------------------------------------------------------------------
// Constants measured or confirmed by the spikes.
// ---------------------------------------------------------------------------

/// `WM_COMMAND` id that opens a new tab (R2/R4). Used as `WPARAM`.
pub const NEW_TAB_COMMAND: usize = 0xA21B;

/// About 1 in 200 [`NEW_TAB_COMMAND`] posts produced no tab. If no tab has
/// registered after this delay, post the command again.
pub const NEW_TAB_REPOST_AFTER_MS: u64 = 1500;

/// `navNoHistory` flag for `IWebBrowser2::Navigate2`. Pass empty `VARIANT`s
/// for the other arguments. Do **not** `SafeArrayDestroy` the PIDL
/// `SAFEARRAY` passed to `Navigate2`: doing so corrupted the heap in R2.
pub const NAV_NO_HISTORY: i32 = 0x2;

/// R5: a dragged tab's `ShellTabWindowClass` HWND is destroyed and recreated
/// in the target window. Match moved tabs by location and UIA name.
pub const TAB_HWND_STABLE_ACROSS_DRAG: bool = false;

/// R5: delay between a drag-out creating a new `CabinetWClass` window and
/// that window appearing in `ShellWindows`, as measured.
pub const DRAG_OUT_REGISTER_DELAY_MS: u64 = 200;

/// R9: within [`TabCreationPipelining::Sequential`], navigate each new tab
/// as soon as it registers, overlapping with creation of the next tab. Do
/// not wait for the navigation to finish.
pub const OVERLAP_NAVIGATION_WITH_CREATION: bool = true;

/// R9: open all restored windows at once, not one after another.
pub const OPEN_WINDOWS_IN_PARALLEL: bool = true;

/// R9: measured median for 3 windows × 10 tabs with the best correct
/// variant. Over the 3 s budget in Req 7.1; open decision for checkpoint 3.
pub const MEASURED_3X10_MEDIAN_MS: u64 = 5300;

/// R9: measured time for 1 window × 30 tabs with the best correct variant.
pub const MEASURED_1X30_MS: u64 = 4000;

/// R7: an event 1074 older than this when Session_End begins is stale and is
/// not used. This value is a design choice, not a spike measurement.
pub const EVENT_1074_MAX_AGE_MS: u64 = 60_000;

/// R7: time budget for querying event 1074 at `WM_QUERYENDSESSION`
/// (design.md). The spike measured about 1 ms.
pub const EVENT_1074_QUERY_BUDGET_MS: u64 = 300;

/// UIA AutomationId of the tab strip's "new tab" button.
pub const UIA_ADD_TAB_AUTOMATION_ID: &str = "AddButton";

/// UIA class name of tab-strip items.
pub const UIA_TAB_ITEM_CLASS: &str = "ListViewItem";

/// UIA class name that looks like a tab item but is not one; exclude it.
pub const UIA_EXCLUDED_ITEM_CLASS: &str = "NavigationViewItem";

/// Transient `ElementFromHandle` failure code (HRESULT `0x80040201`); retry
/// the call.
pub const UIA_TRANSIENT_ERROR: u32 = 0x8004_0201;

#[cfg(test)]
mod tests {
    use super::*;

    // These tests fix the spike-derived defaults, so any change to them has
    // to be deliberate.

    #[test]
    fn hide_default_is_launch_hidden() {
        assert_eq!(HideStrategy::DEFAULT, HideStrategy::LaunchHidden);
    }

    #[test]
    fn tab_create_default_is_wm_command_with_uia_fallback() {
        assert_eq!(TabCreateStrategy::DEFAULT, TabCreateStrategy::WmCommand);
        assert_eq!(TabCreateStrategy::FALLBACK, TabCreateStrategy::UiaAddButton);
    }

    #[test]
    fn tab_close_default_is_uia_close_button() {
        assert_eq!(TabCloseStrategy::DEFAULT, TabCloseStrategy::UiaCloseButton);
    }

    #[test]
    fn tab_pipelining_default_is_sequential() {
        assert_eq!(
            TabCreationPipelining::DEFAULT,
            TabCreationPipelining::Sequential
        );
    }

    #[test]
    fn end_kind_default_is_event_1074_with_shutdown_fallback() {
        assert_eq!(EndKindSource::DEFAULT, EndKindSource::Event1074);
        assert_eq!(EndKindSource::FALLBACK, EndKindSource::AssumeShutdown);
    }

    #[test]
    fn strategies_default_bundles_each_default() {
        let s = Strategies::default();
        assert_eq!(s, Strategies::DEFAULT);
        assert_eq!(s.hide, HideStrategy::DEFAULT);
        assert_eq!(s.tab_create, TabCreateStrategy::DEFAULT);
        assert_eq!(s.tab_close, TabCloseStrategy::DEFAULT);
        assert_eq!(s.tab_pipelining, TabCreationPipelining::DEFAULT);
        assert_eq!(s.end_kind, EndKindSource::DEFAULT);
    }

    #[test]
    fn spike_constants() {
        assert_eq!(NEW_TAB_COMMAND, 0xA21B);
        assert_eq!(NEW_TAB_REPOST_AFTER_MS, 1500);
        assert_eq!(NAV_NO_HISTORY, 0x2);
        const { assert!(!TAB_HWND_STABLE_ACROSS_DRAG) };
        assert_eq!(UIA_TRANSIENT_ERROR, 0x8004_0201);
    }
}
