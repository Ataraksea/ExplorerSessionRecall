//! `DesktopView` computation with `IVirtualDesktopManager` (tab folding,
//! Req 14.9, 14.11, 14.12).
//!
//! A new window's own Virtual_Desktop cannot be read at its first signal:
//! `GetWindowDesktopId` fails with `TYPE_E_ELEMENTNOTFOUND` (F4, builds 22621
//! and 26300; on 26300 it is readable after the first navigation). Windows puts
//! a new top-level window on the current desktop, so the candidate's desktop is
//! taken to be the current one until its id can be read.
//!
//! [`desktop_view`] is the pure computation, over the [`DesktopQuery`] trait so
//! it is unit-tested with fake answers:
//!
//! * `Known(set)`, candidate id unreadable: the windows for which
//!   `IsWindowOnCurrentVirtualDesktop` is true. That call is also true for
//!   windows shown on all desktops, which therefore count for every desktop
//!   (14.11) without the undocumented pinning API.
//! * `Known(set)`, candidate id readable: the windows with the same
//!   `GetWindowDesktopId`, plus, while the candidate is on the current desktop,
//!   the windows on the current desktop.
//! * `Unknown` (14.12): a call failed for an open window whose membership
//!   could not be decided otherwise; the core then treats every open window as
//!   on the candidate's desktop.
//!
//! The candidate itself is always in a `Known` set. The core never stores the
//! result: every classification and every `FoldContext` brings a fresh view.
//!
//! [`VirtualDesktops`] is the COM-backed [`DesktopQuery`]. The interface is
//! created per thread (it is not `Send`); the calls cost well under 1 ms (F4).

use std::collections::BTreeSet;

use esr_core::engine::{DesktopView, WinKey};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
use windows::core::Result;

use crate::keys::hwnd_of;

/// A Virtual_Desktop id (`GUID::to_u128`).
pub type DesktopId = u128;

/// A failed IVirtualDesktopManager call (the HRESULT is not interesting to the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryFailed;

/// The two IVirtualDesktopManager questions the computation asks. An `Err`
/// is any failed call.
pub trait DesktopQuery {
    /// `IsWindowOnCurrentVirtualDesktop`. True for windows shown on all
    /// desktops as well.
    fn on_current_desktop(&self, hwnd: u64) -> std::result::Result<bool, QueryFailed>;

    /// `GetWindowDesktopId`. Fails for a window that is too new to have one.
    fn desktop_id(&self, hwnd: u64) -> std::result::Result<DesktopId, QueryFailed>;
}

/// The `DesktopView` of `candidate`: which of the `open` windows are on its
/// Virtual_Desktop (see the module docs). `open` is every window the core
/// tracks; it may or may not contain `candidate`, which is always in a `Known`
/// set and is never asked about its membership of the current desktop except
/// to learn whether it is on it.
pub fn desktop_view(query: &impl DesktopQuery, candidate: WinKey, open: &[WinKey]) -> DesktopView {
    let others = open.iter().copied().filter(|w| w.hwnd != candidate.hwnd);
    let mut set: BTreeSet<WinKey> = BTreeSet::new();
    set.insert(candidate);

    match query.desktop_id(candidate.hwnd) {
        // The normal case at the first signal: the candidate is on the current desktop.
        Err(QueryFailed) => {
            for w in others {
                match query.on_current_desktop(w.hwnd) {
                    Ok(true) => {
                        set.insert(w);
                    }
                    Ok(false) => {}
                    Err(QueryFailed) => return DesktopView::Unknown,
                }
            }
        }
        Ok(candidate_desktop) => {
            let candidate_on_current = match query.on_current_desktop(candidate.hwnd) {
                Ok(on) => on,
                Err(QueryFailed) => return DesktopView::Unknown,
            };
            for w in others {
                let id = query.desktop_id(w.hwnd);
                if id == Ok(candidate_desktop) {
                    set.insert(w);
                } else if candidate_on_current {
                    // Another desktop's id, or no id: the current-desktop test
                    // also finds the windows shown on all desktops.
                    match query.on_current_desktop(w.hwnd) {
                        Ok(true) => {
                            set.insert(w);
                        }
                        Ok(false) => {}
                        Err(QueryFailed) => return DesktopView::Unknown,
                    }
                } else if id.is_err() {
                    // Not on the current desktop and no id to compare.
                    return DesktopView::Unknown;
                }
            }
        }
    }
    DesktopView::Known(set)
}

/// The `IVirtualDesktopManager` of the calling thread.
///
/// Create it on a thread that has initialized COM (the probe pool threads and
/// the window event source's thread have). It is `!Send`: each thread creates
/// its own.
#[derive(Debug, Clone)]
pub struct VirtualDesktops {
    manager: IVirtualDesktopManager,
}

impl VirtualDesktops {
    /// Creates the manager. Fails if the shell does not provide it.
    pub fn new() -> Result<Self> {
        // SAFETY: valid CLSID, no aggregation; the caller's thread has COM initialized.
        let manager = unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL)? };
        Ok(Self { manager })
    }

    /// The `DesktopView` of `candidate` among `open` (see [`desktop_view`]).
    pub fn view(&self, candidate: WinKey, open: &[WinKey]) -> DesktopView {
        desktop_view(self, candidate, open)
    }
}

impl DesktopQuery for VirtualDesktops {
    fn on_current_desktop(&self, hwnd: u64) -> std::result::Result<bool, QueryFailed> {
        // SAFETY: plain COM call on a live interface; a stale HWND just fails.
        unsafe { self.manager.IsWindowOnCurrentVirtualDesktop(hwnd_of(hwnd)) }
            .map(|b| b.as_bool())
            .map_err(|_| QueryFailed)
    }

    fn desktop_id(&self, hwnd: u64) -> std::result::Result<DesktopId, QueryFailed> {
        // SAFETY: as above.
        unsafe { self.manager.GetWindowDesktopId(hwnd_of(hwnd)) }
            .map(|g| g.to_u128())
            .map_err(|_| QueryFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn w(n: u64) -> WinKey {
        WinKey { hwnd: n, seq: n }
    }

    /// Fake answers per HWND: `(on current desktop, desktop id)`; a missing
    /// field makes that call fail.
    #[derive(Default)]
    struct Fake(HashMap<u64, (Option<bool>, Option<DesktopId>)>);

    impl Fake {
        fn with(mut self, hwnd: u64, current: Option<bool>, id: Option<DesktopId>) -> Self {
            self.0.insert(hwnd, (current, id));
            self
        }
    }

    impl DesktopQuery for Fake {
        fn on_current_desktop(&self, hwnd: u64) -> std::result::Result<bool, QueryFailed> {
            self.0.get(&hwnd).and_then(|e| e.0).ok_or(QueryFailed)
        }
        fn desktop_id(&self, hwnd: u64) -> std::result::Result<DesktopId, QueryFailed> {
            self.0.get(&hwnd).and_then(|e| e.1).ok_or(QueryFailed)
        }
    }

    fn known(ns: &[u64]) -> DesktopView {
        DesktopView::Known(ns.iter().map(|&n| w(n)).collect())
    }

    #[test]
    fn unreadable_candidate_id_uses_the_current_desktop() {
        // 1 current, 2 other desktop, 3 shown on all desktops (current test true).
        let q = Fake::default()
            .with(1, Some(true), None)
            .with(2, Some(false), Some(20))
            .with(3, Some(true), Some(10))
            .with(9, None, None);
        let open = [w(1), w(2), w(3)];
        assert_eq!(desktop_view(&q, w(9), &open), known(&[9, 1, 3]));
    }

    #[test]
    fn readable_candidate_id_on_the_current_desktop_adds_current_windows() {
        // Candidate 9 on desktop 10 (current); 1 same id; 2 other desktop;
        // 3 pinned (no id, current test true); 4 other id but current (all desktops).
        let q = Fake::default()
            .with(9, Some(true), Some(10))
            .with(1, Some(true), Some(10))
            .with(2, Some(false), Some(20))
            .with(3, Some(true), None)
            .with(4, Some(true), Some(30));
        assert_eq!(desktop_view(&q, w(9), &[w(1), w(2), w(3), w(4)]), known(&[9, 1, 3, 4]));
    }

    #[test]
    fn readable_candidate_id_on_another_desktop_uses_ids_only() {
        // Candidate 9 on desktop 20, which is not the current one.
        let q = Fake::default()
            .with(9, Some(false), Some(20))
            .with(1, Some(true), Some(10))
            .with(2, Some(false), Some(20))
            .with(3, Some(false), Some(20));
        assert_eq!(desktop_view(&q, w(9), &[w(1), w(2), w(3)]), known(&[9, 2, 3]));
    }

    #[test]
    fn a_failed_call_for_an_open_window_is_unknown() {
        // Candidate id unreadable and the current-desktop test fails for 2.
        let q = Fake::default().with(9, None, None).with(1, Some(true), None).with(2, None, None);
        assert_eq!(desktop_view(&q, w(9), &[w(1), w(2)]), DesktopView::Unknown);

        // Candidate on another desktop and window 2 has no id to compare.
        let q = Fake::default().with(9, Some(false), Some(20)).with(2, Some(false), None);
        assert_eq!(desktop_view(&q, w(9), &[w(2)]), DesktopView::Unknown);

        // Candidate id readable but its own current-desktop test fails.
        let q = Fake::default().with(9, None, Some(20)).with(1, Some(true), Some(20));
        assert_eq!(desktop_view(&q, w(9), &[w(1)]), DesktopView::Unknown);

        // Candidate on the current desktop, window 2 has no id and no current answer.
        let q = Fake::default().with(9, Some(true), Some(10)).with(2, None, None);
        assert_eq!(desktop_view(&q, w(9), &[w(2)]), DesktopView::Unknown);
    }

    #[test]
    fn no_other_windows_gives_just_the_candidate_and_ignores_it_in_open() {
        let q = Fake::default().with(9, None, None);
        assert_eq!(desktop_view(&q, w(9), &[]), known(&[9]));
        assert_eq!(desktop_view(&q, w(9), &[w(9)]), known(&[9]), "the candidate in `open` is not asked twice");
    }

    #[test]
    fn real_manager_answers_for_the_desktop_window_when_available() {
        // Smoke test of the COM path: skipped quietly where the shell has no manager.
        let Ok(_com) = crate::shell_util::StaGuard::init() else { return };
        let Ok(vd) = VirtualDesktops::new() else { return };
        let hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::GetShellWindow() };
        if hwnd.0.is_null() {
            return;
        }
        let view = vd.view(w(1), &[WinKey { hwnd: crate::keys::hwnd_value(hwnd), seq: 1 }]);
        assert!(matches!(view, DesktopView::Known(_) | DesktopView::Unknown));
    }
}
