//! `DisplayInfo`: connected displays via `EnumDisplayMonitors` + `GetMonitorInfoW`.
//!
//! All rectangles are screen coordinates in physical pixels. Windows reports
//! physical pixels only to per-monitor-DPI-aware callers; DPI-unaware or
//! system-aware callers get virtualized (scaled) rectangles. The app sets the
//! process to per-monitor-aware v2, but every function here also switches the
//! calling thread to `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` for the
//! duration of the call (and restores the previous context afterwards), so the
//! results are physical pixels regardless of the process awareness.
//!
//! [`display_for_window`] gives the monitor and work rects of the display a
//! window is on, which the scanner needs to convert `GetWindowPlacement`
//! workspace coordinates to screen coordinates
//! (`esr_core::plan::placement::workspace_to_screen`).
//!
//! Requirements 5.5, 5.8, 5.12.

use esr_core::model::{Display, Rect};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

/// The `DisplayInfo` adapter (backs the `Displays` port in `esr-app`).
#[derive(Debug, Default, Clone, Copy)]
pub struct DisplayInfo;

impl DisplayInfo {
    pub fn new() -> Self {
        Self
    }

    /// Every connected display, in `EnumDisplayMonitors` order.
    pub fn displays(&self) -> Vec<Display> {
        displays()
    }

    /// The display `hwnd` is on (nearest display if it is off-screen).
    pub fn display_for_window(&self, hwnd: HWND) -> Option<Display> {
        display_for_window(hwnd)
    }
}

/// Every connected display, in `EnumDisplayMonitors` order. Displays whose
/// `GetMonitorInfoW` call fails (for example, one disconnected mid-enumeration)
/// are skipped. Empty only if enumeration itself fails.
pub fn displays() -> Vec<Display> {
    let _dpi = ThreadDpiScope::per_monitor_v2();
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `monitors`, which outlives the
    // synchronous enumeration; no HDC or clip rect is passed.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut monitors as *mut Vec<HMONITOR> as isize),
        )
    };
    monitors.into_iter().filter_map(monitor_display).collect()
}

/// The display `hwnd` is on (`MonitorFromWindow(MONITOR_DEFAULTTONEAREST)`),
/// with its monitor and work rects in physical pixels. `None` if the monitor
/// information cannot be read.
pub fn display_for_window(hwnd: HWND) -> Option<Display> {
    let _dpi = ThreadDpiScope::per_monitor_v2();
    // SAFETY: MonitorFromWindow tolerates stale HWNDs (it then falls back to
    // the nearest/primary monitor).
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        return None;
    }
    monitor_display(monitor)
}

/// `EnumDisplayMonitors` callback: appends the monitor handle to the
/// `Vec<HMONITOR>` passed through `data`.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<HMONITOR>` passed by `displays`, alive
    // for the whole synchronous enumeration.
    let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    monitors.push(monitor);
    true.into()
}

/// Reads one monitor's device name and rects.
fn monitor_display(monitor: HMONITOR) -> Option<Display> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is a writable MONITORINFOEXW whose cbSize says so; a
    // MONITORINFOEXW starts with a MONITORINFO.
    let ok = unsafe {
        GetMonitorInfoW(
            monitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let device_len = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    Some(Display {
        device: String::from_utf16_lossy(&info.szDevice[..device_len]),
        monitor: to_rect(info.monitorInfo.rcMonitor),
        work: to_rect(info.monitorInfo.rcWork),
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

fn to_rect(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// Switches the calling thread to per-monitor-aware v2 and restores the
/// previous thread DPI awareness context on drop.
struct ThreadDpiScope {
    previous: DPI_AWARENESS_CONTEXT,
}

impl ThreadDpiScope {
    fn per_monitor_v2() -> Self {
        // SAFETY: valid predefined context. Returns null on failure, in which
        // case the thread keeps its current awareness.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { previous }
    }
}

impl Drop for ThreadDpiScope {
    fn drop(&mut self) {
        if !self.previous.0.is_null() {
            // SAFETY: restores the context returned by the matching call above.
            unsafe { SetThreadDpiAwarenessContext(self.previous) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetDesktopWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::w;

    fn contains(outer: &Rect, inner: &Rect) -> bool {
        inner.left >= outer.left
            && inner.top >= outer.top
            && inner.right <= outer.right
            && inner.bottom <= outer.bottom
    }

    #[test]
    fn displays_are_well_formed() {
        let displays = DisplayInfo::new().displays();
        assert!(!displays.is_empty(), "no displays enumerated");
        assert_eq!(
            displays.iter().filter(|d| d.primary).count(),
            1,
            "{displays:?}"
        );
        for d in &displays {
            assert!(!d.device.is_empty(), "{d:?}");
            assert!(d.monitor.is_positive(), "{d:?}");
            assert!(d.work.is_positive(), "{d:?}");
            assert!(contains(&d.monitor, &d.work), "work outside monitor: {d:?}");
        }
        let primary = displays.iter().find(|d| d.primary).unwrap();
        // The primary monitor's top-left is the origin of the virtual screen.
        assert_eq!(
            (primary.monitor.left, primary.monitor.top),
            (0, 0),
            "{primary:?}"
        );
    }

    #[test]
    fn desktop_window_maps_to_a_known_display() {
        // SAFETY: no preconditions.
        let desktop = unsafe { GetDesktopWindow() };
        let d = display_for_window(desktop).expect("desktop display");
        assert!(displays().contains(&d), "{d:?}");
        assert!(contains(&d.monitor, &d.work));
    }

    #[test]
    fn hidden_window_on_primary_maps_to_primary() {
        // A hidden (never shown) popup of the predefined STATIC class at the
        // primary monitor's origin; no class registration needed.
        // SAFETY: valid predefined class name; no parent, menu, or params.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("esr-display-info-test"),
                WS_POPUP,
                10,
                10,
                100,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create hidden window");
        let d = display_for_window(hwnd);
        // SAFETY: we own the window and destroy it on this (creating) thread.
        unsafe { DestroyWindow(hwnd) }.expect("destroy window");
        let d = d.expect("display for hidden window");
        assert!(d.primary, "{d:?}");
        assert!(displays().contains(&d));
    }

    #[test]
    fn thread_dpi_context_is_restored() {
        use windows::Win32::UI::HiDpi::{
            AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
        };
        // SAFETY: no preconditions.
        let before = unsafe { GetThreadDpiAwarenessContext() };
        let _ = displays();
        // SAFETY: no preconditions; both contexts are valid.
        let after = unsafe { GetThreadDpiAwarenessContext() };
        assert!(unsafe { AreDpiAwarenessContextsEqual(before, after) }.as_bool());
    }
}
