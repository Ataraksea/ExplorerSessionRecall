//! Spike R1: hide a new Explorer window before its first frame (Req 7.2, 7.7).
//!
//! ```text
//! cargo test -p esr-win --test spike_r1_hide -- --ignored --nocapture
//! ```
//!
//! Assumption under test (design.md, "Verification spikes", R1): applying
//! `WS_EX_LAYERED` + alpha 0 and the final `SetWindowPlacement` from an
//! out-of-context `EVENT_OBJECT_CREATE` handler hides a window opened with the
//! `opennewwindow` verb before DWM composes its first frame, and the window is
//! first seen (after reveal) at its final placement.
//!
//! Method:
//!
//! 1. A magenta backdrop window (owned by this test) covers the work area of
//!    every monitor so that any Explorer pixel is easy to detect. It is created
//!    before the Explorer window, so the new window sits above it in z-order.
//! 2. `Windows.Graphics.Capture` captures every monitor. Each frame is sampled
//!    (every 4th pixel in x and y over the work area), compared with the
//!    monitor's first frame (the backdrop as DWM composes it; on HDR monitors
//!    it is not pure magenta), and reduced to a bitmask of changed samples
//!    tagged with its `SystemRelativeTime` (QPC time base). Samples that change
//!    before the launch (clocks, animations) are masked as noise.
//! 3. A WinEvent hook (`EVENT_OBJECT_CREATE..EVENT_OBJECT_LOCATIONCHANGE`,
//!    out of context) claims the first new top-level `CabinetWClass` window
//!    and applies hide + final placement inside the callback.
//! 4. `ShellExecuteExW("opennewwindow", SEE_MASK_IDLIST, SW_SHOWNOACTIVATE)`
//!    runs on a separate STA thread, so the hook thread keeps pumping.
//! 5. After the window has been shown (transparent) for 1.5 s it is revealed
//!    (alpha 255, `WS_EX_LAYERED` removed) and observed for 1.2 s more.
//! 6. Secondary check (no frames needed): every ~15 ms and at each SHOW /
//!    LOCATIONCHANGE / STATECHANGE event, a visible window must be either fully
//!    transparent (`GetLayeredWindowAttributes` alpha 0) or at its final rect.
//!
//! Verdict:
//! * capture: no frame composed before the reveal shows changed samples
//!   (beyond a small tolerance); no frame after the reveal shows changes
//!   outside the final rect; and (positive control) some frame after
//!   the reveal shows the window filling its final rect.
//! * polling: no violation recorded.
//!
//! Safety: only the window created by this test is modified or closed (it is
//! identified by HWND, created after the test started, not present in the
//! pre-test window list). Before closing, its original placement is restored
//! while it is transparent, so Explorer does not remember the spike's size.
//! `ExplorerGuard` (`tests/common`) adopts the window and closes it (all tabs),
//! also on panic. Run alone with `--test-threads=1`.

#![cfg(windows)]

mod common;

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::explorer_guard::ExplorerGuard;
use esr_win::shell_util::{StaGuard, is_cabinet_window, window_process_id};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, EnumDisplayMonitors, GetMonitorInfoW, HBRUSH, HDC, HGDIOBJ,
    HMONITOR, MONITORINFO, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_STATECHANGE, EnumWindows, GWL_EXSTYLE,
    GetLayeredWindowAttributes, GetMessageW, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect,
    HWND_TOPMOST, IsIconic, IsWindow, IsWindowVisible, IsZoomed, KillTimer,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, MSG, OBJID_WINDOW, RegisterClassW,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    TranslateMessage, UnregisterClassW, WINDOWPLACEMENT, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{BOOL, HSTRING, Interface, w};

/// Sampling stride in pixels (x and y) for the frame bitmask.
const STRIDE: i32 = 4;
/// A sample has changed if any channel differs from the reference by more than this.
const CHANNEL_TOLERANCE: u8 = 40;
/// Changed samples (outside the noise mask) that count as "something visible".
const SAMPLE_TOLERANCE: u32 = 24;
/// Margin around the final rect for the DWM shadow and invisible resize borders.
const OUTSIDE_MARGIN: i32 = 48;
/// The window stays transparent this long after it first becomes visible.
const HIDDEN_DWELL: Duration = Duration::from_millis(1500);
/// Observation time after the reveal.
const AFTER_REVEAL: Duration = Duration::from_millis(1200);
/// Give up if no window shows up within this time after the launch.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(12);

const MAGENTA: COLORREF = COLORREF(0x00FF_00FF);

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn qpc_100ns() -> i64 {
    let mut count = 0i64;
    let mut freq = 0i64;
    // SAFETY: valid out pointers.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (i128::from(count) * 10_000_000 / i128::from(freq.max(1))) as i64
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: valid out pointer; stale HWNDs just fail.
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    rect
}

fn frame_bounds(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: the buffer is a RECT of the size passed.
    let _ = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    };
    rect
}

fn placement(hwnd: HWND) -> WINDOWPLACEMENT {
    let mut wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: valid, correctly sized out structure.
    let _ = unsafe { GetWindowPlacement(hwnd, &mut wp) };
    wp
}

fn fmt_rect(r: &RECT) -> String {
    format!(
        "({},{})-({},{}) {}x{}",
        r.left,
        r.top,
        r.right,
        r.bottom,
        r.right - r.left,
        r.bottom - r.top
    )
}

fn expand(r: &RECT, by: i32) -> RECT {
    RECT {
        left: r.left - by,
        top: r.top - by,
        right: r.right + by,
        bottom: r.bottom + by,
    }
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn ex_style(hwnd: HWND) -> isize {
    // SAFETY: plain read; stale HWNDs return 0.
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }
}

/// Alpha of a layered window, or `None` if it is not layered with `LWA_ALPHA`.
fn layered_alpha(hwnd: HWND) -> Option<u8> {
    if ex_style(hwnd) & WS_EX_LAYERED.0 as isize == 0 {
        return None;
    }
    let mut alpha = 255u8;
    let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
    // SAFETY: valid out pointers.
    unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) }.ok()?;
    (flags.0 & LWA_ALPHA.0 != 0).then_some(alpha)
}

// ---------------------------------------------------------------------------
// Monitors and backdrop
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Monitor {
    handle: isize,
    rect: RECT,
    work: RECT,
    primary: bool,
}

impl Monitor {
    fn grid(&self) -> (usize, usize) {
        let cols = ((self.work.right - self.work.left + STRIDE - 1) / STRIDE).max(0) as usize;
        let rows = ((self.work.bottom - self.work.top + STRIDE - 1) / STRIDE).max(0) as usize;
        (cols, rows)
    }
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<Monitor>` passed below.
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: valid, correctly sized out structure.
        if unsafe { GetMonitorInfoW(hmon, &mut info) }.as_bool() {
            list.push(Monitor {
                handle: hmon.0 as isize,
                rect: info.rcMonitor,
                work: info.rcWork,
                primary: info.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            });
        }
        true.into()
    }
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only runs during this call; `list` outlives it.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

fn cabinet_windows() -> Vec<isize> {
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

unsafe extern "system" fn backdrop_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwarding the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---------------------------------------------------------------------------
// Frame capture (separate MTA thread)
// ---------------------------------------------------------------------------

struct Frame {
    mon: usize,
    /// `SystemRelativeTime` in 100 ns QPC units.
    t: i64,
    bits: Vec<u64>,
}

struct Stream {
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    staging: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
    got_first: bool,
    /// Samples of the first frame (BGR), the per-monitor reference.
    reference: Option<Vec<[u8; 3]>>,
    reference_info: Option<String>,
}

fn capture_thread(
    mons: Vec<Monitor>,
    stop: Arc<AtomicBool>,
    first_frames: Arc<AtomicUsize>,
) -> Result<(Vec<Frame>, Vec<String>), String> {
    // SAFETY: balanced with RoUninitialize at the end of this thread.
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|e| format!("RoInitialize: {e}"))?;
    let result = capture_loop(&mons, &stop, &first_frames).map_err(|e| format!("capture: {e}"));
    // SAFETY: balanced with RoInitialize above.
    unsafe { RoUninitialize() };
    result
}

fn capture_loop(
    mons: &[Monitor],
    stop: &AtomicBool,
    first_frames: &AtomicUsize,
) -> windows::core::Result<(Vec<Frame>, Vec<String>)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: valid out pointers; default adapter.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.expect("D3D11 device");
    let context = context.expect("D3D11 context");
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: valid DXGI device.
    let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;

    let mut streams = Vec::new();
    for m in mons {
        // SAFETY: a live HMONITOR from EnumDisplayMonitors.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(HMONITOR(m.handle as *mut _))? };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        streams.push(Stream {
            pool,
            session,
            _item: item,
            staging: None,
            got_first: false,
            reference: None,
            reference_info: None,
        });
    }

    let mut frames = Vec::new();
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        let mut any = false;
        for (idx, s) in streams.iter_mut().enumerate() {
            while let Ok(frame) = s.pool.TryGetNextFrame() {
                any = true;
                let t = frame.SystemRelativeTime()?.Duration;
                let bits = analyze(&device, &context, s, &frame, &mons[idx])?;
                let _ = frame.Close();
                frames.push(Frame { mon: idx, t, bits });
                if !s.got_first {
                    s.got_first = true;
                    first_frames.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        if stopping {
            break;
        }
        if !any {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    for s in streams.iter() {
        let _ = s.session.Close();
        let _ = s.pool.Close();
    }
    let infos = streams
        .into_iter()
        .map(|s| s.reference_info.unwrap_or_default())
        .collect();
    Ok((frames, infos))
}

/// Copies the frame to a staging texture and builds the non-magenta bitmask
/// over the monitor's work area (sample (i, j) = screen (work.left + i*S,
/// work.top + j*S)).
fn analyze(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    stream: &mut Stream,
    frame: &Direct3D11CaptureFrame,
    mon: &Monitor,
) -> windows::core::Result<Vec<u64>> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: the capture surface is backed by an ID3D11Texture2D.
    let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid out pointer.
    unsafe { tex.GetDesc(&mut desc) };
    let reuse = matches!(&stream.staging, Some((_, d))
        if d.Width == desc.Width && d.Height == desc.Height && d.Format == desc.Format);
    if !reuse {
        let mut sd = desc;
        sd.Usage = D3D11_USAGE_STAGING;
        sd.BindFlags = 0;
        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        sd.MiscFlags = 0;
        sd.MipLevels = 1;
        sd.ArraySize = 1;
        let mut staging = None;
        // SAFETY: valid descriptor and out pointer.
        unsafe { device.CreateTexture2D(&sd, None, Some(&mut staging))? };
        stream.staging = Some((staging.expect("staging texture"), desc));
    }
    let staging = &stream.staging.as_ref().expect("staging").0;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: both textures have identical size and format; staging is CPU-readable.
    unsafe {
        context.CopyResource(staging, &tex);
        context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let (cols, rows) = mon.grid();
    let mut bits = vec![0u64; (cols * rows).div_ceil(64)];
    let mut samples = vec![[0u8; 3]; cols * rows];
    let pitch = mapped.RowPitch as usize;
    // SAFETY: the mapped subresource has `Height` rows of `RowPitch` bytes.
    let data = unsafe {
        std::slice::from_raw_parts(mapped.pData as *const u8, pitch * desc.Height as usize)
    };
    for j in 0..rows {
        let y = mon.work.top + j as i32 * STRIDE - mon.rect.top;
        if y < 0 || y as u32 >= desc.Height {
            continue;
        }
        let row = &data[y as usize * pitch..y as usize * pitch + desc.Width as usize * 4];
        for i in 0..cols {
            let x = mon.work.left + i as i32 * STRIDE - mon.rect.left;
            if x < 0 || x as u32 >= desc.Width {
                continue;
            }
            let p = &row[x as usize * 4..x as usize * 4 + 3]; // B, G, R
            samples[j * cols + i] = [p[0], p[1], p[2]];
        }
    }
    // SAFETY: balanced with Map above.
    unsafe { context.Unmap(staging, 0) };

    // The first frame of each monitor is its reference (the backdrop as DWM
    // actually composes it, which differs from pure magenta on HDR displays or
    // where another window covers the backdrop). A sample is "changed" if any
    // channel differs from the reference by more than the tolerance.
    let reference = stream.reference.get_or_insert_with(|| samples.clone());
    for (k, (s, r)) in samples.iter().zip(reference.iter()).enumerate() {
        if (0..3).any(|c| s[c].abs_diff(r[c]) > CHANNEL_TOLERANCE) {
            bits[k / 64] |= 1 << (k % 64);
        }
    }
    if stream.reference_info.is_none() {
        let center = samples[(rows / 2) * cols + cols / 2];
        let magenta = samples
            .iter()
            .filter(|p| {
                255 - p[0] <= CHANNEL_TOLERANCE
                    && p[1] <= CHANNEL_TOLERANCE
                    && 255 - p[2] <= CHANNEL_TOLERANCE
            })
            .count();
        stream.reference_info = Some(format!(
            "center BGR {center:?}, {magenta} of {} samples magenta",
            samples.len()
        ));
    }
    Ok(bits)
}

/// Number of unmasked samples of `mon` inside `region`.
fn samples_in(mon: &Monitor, mask: &[u64], region: &RECT) -> u32 {
    let (cols, rows) = mon.grid();
    let mut n = 0;
    for k in 0..cols * rows {
        let x = mon.work.left + (k % cols) as i32 * STRIDE;
        let y = mon.work.top + (k / cols) as i32 * STRIDE;
        let masked = mask.get(k / 64).is_some_and(|w| w >> (k % 64) & 1 == 1);
        if !masked && contains(region, x, y) {
            n += 1;
        }
    }
    n
}

/// Counts changed samples of `frame` (not in the noise `mask`) whose screen
/// position satisfies `pred`, and their bounding box.
fn count(
    frame: &Frame,
    mon: &Monitor,
    mask: &[u64],
    pred: impl Fn(i32, i32) -> bool,
) -> (u32, Option<RECT>) {
    let (cols, _) = mon.grid();
    let mut n = 0;
    let mut bbox: Option<RECT> = None;
    for (w, word) in frame.bits.iter().enumerate() {
        let mut word = *word & !mask.get(w).copied().unwrap_or(0);
        while word != 0 {
            let b = word.trailing_zeros() as usize;
            word &= word - 1;
            let k = w * 64 + b;
            let x = mon.work.left + (k % cols) as i32 * STRIDE;
            let y = mon.work.top + (k / cols) as i32 * STRIDE;
            if pred(x, y) {
                n += 1;
                let r = bbox.get_or_insert(RECT {
                    left: x,
                    top: y,
                    right: x + 1,
                    bottom: y + 1,
                });
                r.left = r.left.min(x);
                r.top = r.top.min(y);
                r.right = r.right.max(x + 1);
                r.bottom = r.bottom.max(y + 1);
            }
        }
    }
    (n, bbox)
}

// ---------------------------------------------------------------------------
// Hook state (hook callbacks run on the test thread while it pumps messages)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Ctx {
    armed: bool,
    t_launch: Option<Instant>,
    preexisting: Vec<isize>,
    final_wp: WINDOWPLACEMENT,
    final_screen: RECT,
    target: Option<isize>,
    target_pid: u32,
    original_wp: Option<WINDOWPLACEMENT>,
    original_ex: isize,
    rect_before: RECT,
    /// Set during the sensitivity control; polling checks stop.
    control: bool,
    claimed_at: Option<Instant>,
    first_visible: Option<Instant>,
    revealed: bool,
    timeline: Vec<String>,
    hide_violations: Vec<String>,
    placement_violations: Vec<String>,
    checks: u32,
}

thread_local! {
    static CTX: RefCell<Ctx> = RefCell::new(Ctx::default());
}

fn ms_since_launch(now: Instant) -> f64 {
    CTX.with(|c| c.borrow().t_launch)
        .map_or(0.0, |t| now.duration_since(t).as_secs_f64() * 1000.0)
}

fn note(line: String) {
    CTX.with(|c| c.borrow_mut().timeline.push(line));
}

unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
        return;
    }
    let now = Instant::now();
    let (armed, target) = CTX.with(|c| {
        let c = c.borrow();
        (c.armed, c.target)
    });
    if !armed {
        return;
    }
    match target {
        Some(t) if t == hwnd.0 as isize => {
            let name = match event {
                EVENT_OBJECT_SHOW => "SHOW",
                EVENT_OBJECT_HIDE => "HIDE",
                EVENT_OBJECT_DESTROY => "DESTROY",
                EVENT_OBJECT_LOCATIONCHANGE => "LOCATIONCHANGE",
                EVENT_OBJECT_STATECHANGE => "STATECHANGE",
                _ => return,
            };
            if event != EVENT_OBJECT_DESTROY {
                let r = window_rect(hwnd);
                note(format!(
                    "{:8.1} ms  {name:<14} rect {} visible={} alpha={:?}",
                    ms_since_launch(now),
                    fmt_rect(&r),
                    // SAFETY: tolerates stale HWNDs.
                    unsafe { IsWindowVisible(hwnd) }.as_bool(),
                    layered_alpha(hwnd)
                ));
                check(hwnd, name);
            }
        }
        None if event == EVENT_OBJECT_CREATE => claim(hwnd, now),
        _ => {}
    }
}

/// Claims the first new top-level `CabinetWClass` window and applies hide + placement.
fn claim(hwnd: HWND, now: Instant) {
    let key = hwnd.0 as isize;
    let (fresh, final_wp) = CTX.with(|c| {
        let c = c.borrow();
        (!c.preexisting.contains(&key), c.final_wp)
    });
    if !fresh || !is_cabinet_window(hwnd) {
        return;
    }
    // Everything below only touches this window, which the test just created.
    // SAFETY: plain reads.
    let visible_before = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let rect_before = window_rect(hwnd);
    let original_wp = placement(hwnd);
    let ex = ex_style(hwnd);
    let pid = window_process_id(hwnd);
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.target = Some(key);
        c.target_pid = pid;
        c.original_wp = Some(original_wp);
        c.original_ex = ex;
        c.rect_before = rect_before;
        c.claimed_at = Some(now);
    });

    let t_apply = Instant::now();
    // SAFETY: style / layered / placement calls on a live window of the same
    // integrity level; `final_wp` is a fully initialized WINDOWPLACEMENT.
    let (r_ex, r_alpha, r_place) = unsafe {
        let prev = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        let alpha = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        // Test harness only: the backdrop is topmost, so the new window is made topmost
        // too (z-order only; it is already fully transparent here).
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let place = SetWindowPlacement(hwnd, &final_wp);
        (prev, alpha, place)
    };
    let apply_ms = t_apply.elapsed().as_secs_f64() * 1000.0;
    note(format!(
        "{:8.1} ms  CREATE claimed {:?} pid {pid}: visible_before={visible_before} \
         rect_before {} ex=0x{ex:x}; apply took {apply_ms:.1} ms \
         (SetWindowLongPtr prev=0x{r_ex:x}, SetLayeredWindowAttributes {:?}, SetWindowPlacement {:?})",
        ms_since_launch(now),
        hwnd,
        fmt_rect(&rect_before),
        r_alpha.as_ref().map_err(|e| e.code()),
        r_place.as_ref().map_err(|e| e.code()),
    ));
    if visible_before {
        CTX.with(|c| {
            c.borrow_mut().hide_violations.push(format!(
                "window already had WS_VISIBLE when EVENT_OBJECT_CREATE was handled (rect {})",
                fmt_rect(&rect_before)
            ))
        });
    }
    check(hwnd, "after-apply");
}

/// Fallback check: a visible window must be fully transparent or at its final rect,
/// and must be fully transparent until the reveal.
fn check(hwnd: HWND, source: &str) {
    let now = Instant::now();
    // SAFETY: tolerant reads.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let alpha = layered_alpha(hwnd);
    let rect = window_rect(hwnd);
    // SAFETY: tolerant reads.
    let (iconic, zoomed) = unsafe { (IsIconic(hwnd).as_bool(), IsZoomed(hwnd).as_bool()) };
    let t = ms_since_launch(now);
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.checks += 1;
        if c.control {
            return;
        }
        if c.first_visible.is_none() && visible {
            c.first_visible = Some(now);
        }
        if !visible {
            return;
        }
        let transparent = alpha == Some(0);
        let at_final = !iconic && !zoomed && rect == c.final_screen;
        if !transparent && !c.revealed {
            let msg = format!(
                "{t:.1} ms [{source}] visible and not transparent before reveal: alpha={alpha:?} rect {}",
                fmt_rect(&rect)
            );
            c.hide_violations.push(msg);
        }
        if !transparent && !at_final {
            let msg = format!(
                "{t:.1} ms [{source}] visible, not transparent, not at final rect: alpha={alpha:?} \
                 rect {} iconic={iconic} zoomed={zoomed}",
                fmt_rect(&rect)
            );
            c.placement_violations.push(msg);
        }
    });
}

/// Pumps messages (hook callbacks run inside GetMessageW) until `tick` returns true.
/// `tick` runs about every 15 ms.
fn pump(mut tick: impl FnMut() -> bool) {
    // SAFETY: thread timer, no callback; killed below.
    let timer = unsafe { SetTimer(None, 0, 15, None) };
    loop {
        let mut msg = MSG::default();
        // SAFETY: valid out pointer.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        if msg.message == WM_TIMER && msg.hwnd.is_invalid() {
            if tick() {
                break;
            }
            continue;
        }
        // SAFETY: standard dispatch of a retrieved message.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // SAFETY: the timer created above.
    unsafe {
        let _ = KillTimer(None, timer);
    }
}

fn pump_for(d: Duration) {
    let end = Instant::now() + d;
    pump(|| Instant::now() >= end);
}

// ---------------------------------------------------------------------------
// Cleanup guard
// ---------------------------------------------------------------------------

struct Cleanup {
    hook: Option<HWINEVENTHOOK>,
    backdrops: Vec<HWND>,
    brush: Option<HBRUSH>,
    class_registered: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(h) = self.hook.take() {
            // SAFETY: the hook installed by this test.
            let _ = unsafe { UnhookWinEvent(h) };
        }
        let (target, pid, original_wp) = CTX.with(|c| {
            let c = c.borrow();
            (c.target, c.target_pid, c.original_wp)
        });
        if let Some(t) = target {
            let hwnd = HWND(t as *mut _);
            // Only the window this test created: same HWND, still a cabinet, same process.
            // SAFETY: tolerant checks.
            if unsafe { IsWindow(Some(hwnd)) }.as_bool()
                && is_cabinet_window(hwnd)
                && window_process_id(hwnd) == pid
            {
                // SAFETY: calls on the test's own window.
                unsafe {
                    // Transparent while its original (Explorer-chosen) placement is restored,
                    // so Explorer remembers its own size rather than the spike's.
                    let _ = SetWindowLongPtrW(
                        hwnd,
                        GWL_EXSTYLE,
                        ex_style(hwnd) | WS_EX_LAYERED.0 as isize,
                    );
                    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
                    if let Some(mut wp) = original_wp {
                        wp.showCmd = SW_SHOWNOACTIVATE.0 as u32;
                        let _ = SetWindowPlacement(hwnd, &wp);
                    }
                }
                // Closing (every tab, repeated WM_CLOSE) is left to the ExplorerGuard,
                // which drops after this guard.
            }
        }
        for b in self.backdrops.drain(..) {
            // SAFETY: windows created by this thread.
            let _ = unsafe { DestroyWindow(b) };
        }
        if self.class_registered {
            // SAFETY: class registered by this test; its windows are destroyed.
            unsafe {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let _ = UnregisterClassW(w!("EsrSpikeR1Backdrop"), Some(hinst.into()));
            }
        }
        if let Some(b) = self.brush.take() {
            // SAFETY: brush created by this test, no longer used by a class.
            let _ = unsafe { DeleteObject(HGDIOBJ(b.0)) };
        }
    }
}

// ---------------------------------------------------------------------------
// The spike
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spike: opens and closes a File Explorer window on the interactive desktop"]
fn spike_r1_layered_alpha_hides_new_window_before_first_frame() {
    // Physical pixels everywhere (capture, monitor info, Explorer's windows).
    // SAFETY: plain calls; failure (already set) is fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _sta = StaGuard::init().expect("STA");

    // Declared before `cleanup`, so it drops after it: `Cleanup` restores the
    // window's placement, then the guard closes it and deletes the temp root.
    let guard = ExplorerGuard::new("r1");
    let temp_dir = guard.dir("r1_window");

    let mut cleanup = Cleanup {
        hook: None,
        backdrops: Vec::new(),
        brush: None,
        class_registered: false,
    };

    let mons = monitors();
    assert!(!mons.is_empty(), "no monitors");
    let primary = *mons.iter().find(|m| m.primary).unwrap_or(&mons[0]);
    for (i, m) in mons.iter().enumerate() {
        println!(
            "monitor {i}: rect {} work {} primary={}",
            fmt_rect(&m.rect),
            fmt_rect(&m.work),
            m.primary
        );
    }

    // Final placement: odd offsets on the primary monitor so it differs from
    // Explorer's default position.
    let ww = primary.work.right - primary.work.left;
    let wh = primary.work.bottom - primary.work.top;
    let final_screen = RECT {
        left: primary.work.left + ww / 8 + 13,
        top: primary.work.top + wh / 8 + 7,
        right: primary.work.left + ww / 8 + 13 + (ww * 3 / 4).min(1400),
        bottom: primary.work.top + wh / 8 + 7 + (wh / 2).min(1000),
    };
    // rcNormalPosition is in workspace coordinates (work-area relative).
    let dx = primary.work.left - primary.rect.left;
    let dy = primary.work.top - primary.rect.top;
    let final_wp = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        showCmd: SW_SHOWNOACTIVATE.0 as u32,
        rcNormalPosition: RECT {
            left: final_screen.left - dx,
            top: final_screen.top - dy,
            right: final_screen.right - dx,
            bottom: final_screen.bottom - dy,
        },
        ..Default::default()
    };
    println!("final rect (screen) {}", fmt_rect(&final_screen));

    // Backdrops.
    // SAFETY: standard class registration and window creation on this thread.
    unsafe {
        let hinst = GetModuleHandleW(None).expect("module");
        let brush = CreateSolidBrush(MAGENTA);
        cleanup.brush = Some(brush);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(backdrop_proc),
            hInstance: hinst.into(),
            hbrBackground: brush,
            lpszClassName: w!("EsrSpikeR1Backdrop"),
            ..Default::default()
        };
        cleanup.class_registered = RegisterClassW(&wc) != 0;
        for m in &mons {
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                w!("EsrSpikeR1Backdrop"),
                w!("ESR spike R1 backdrop"),
                WS_POPUP | WS_VISIBLE,
                m.work.left,
                m.work.top,
                m.work.right - m.work.left,
                m.work.bottom - m.work.top,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .expect("backdrop window");
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = UpdateWindow(hwnd);
            cleanup.backdrops.push(hwnd);
        }
    }
    pump_for(Duration::from_millis(400));

    // Capture.
    let stop = Arc::new(AtomicBool::new(false));
    let first_frames = Arc::new(AtomicUsize::new(0));
    let capture = {
        let (mons, stop, first_frames) = (mons.clone(), stop.clone(), first_frames.clone());
        std::thread::spawn(move || capture_thread(mons, stop, first_frames))
    };
    let wait_end = Instant::now() + Duration::from_secs(5);
    pump(|| {
        first_frames.load(Ordering::SeqCst) >= mons.len()
            || Instant::now() >= wait_end
            || capture.is_finished()
    });
    if capture.is_finished() {
        let err = capture.join().expect("capture thread").err();
        panic!("capture thread ended early: {err:?}");
    }
    assert_eq!(
        first_frames.load(Ordering::SeqCst),
        mons.len(),
        "not every monitor delivered a first frame"
    );
    pump_for(Duration::from_millis(400)); // baseline frames

    // Hook + launch.
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.preexisting = cabinet_windows();
        c.final_wp = final_wp;
        c.final_screen = final_screen;
    });
    // SAFETY: out-of-context hook; callback is a valid extern "system" fn.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(on_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    assert!(!hook.is_invalid(), "SetWinEventHook failed");
    cleanup.hook = Some(hook);

    let launch_qpc = qpc_100ns();
    let t_launch = Instant::now();
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        c.armed = true;
        c.t_launch = Some(t_launch);
    });
    let launcher = {
        let dir = temp_dir.clone();
        std::thread::spawn(move || -> Result<f64, String> {
            let _sta = StaGuard::init().map_err(|e| e.to_string())?;
            let t = Instant::now();
            // SAFETY: path is a valid HSTRING; the PIDL is freed below.
            unsafe {
                let pidl = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
                if pidl.is_null() {
                    return Err("ILCreateFromPathW failed".into());
                }
                let mut sei = SHELLEXECUTEINFOW {
                    cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
                    fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                    lpVerb: w!("opennewwindow"),
                    lpIDList: pidl.cast(),
                    nShow: SW_SHOWNOACTIVATE.0,
                    ..Default::default()
                };
                let r = ShellExecuteExW(&mut sei);
                ILFree(Some(pidl));
                r.map_err(|e| format!("ShellExecuteExW: {e}"))?;
            }
            Ok(t.elapsed().as_secs_f64() * 1000.0)
        })
    };

    let mut reveal: Option<(Instant, i64, RECT)> = None;
    let mut timed_out = false;
    pump(|| {
        let now = Instant::now();
        let (target, claimed_at, first_visible) = CTX.with(|c| {
            let c = c.borrow();
            (c.target, c.claimed_at, c.first_visible)
        });
        let Some(t) = target else {
            timed_out = now.duration_since(t_launch) > LAUNCH_TIMEOUT;
            return timed_out;
        };
        let hwnd = HWND(t as *mut _);
        // The claimed window is new (not pre-existing): adopt it (idempotent).
        guard.adopt(hwnd);
        check(hwnd, "poll");
        match reveal {
            None => {
                let ready = first_visible.is_some_and(|v| now.duration_since(v) >= HIDDEN_DWELL)
                    && claimed_at.is_some_and(|c| now.duration_since(c) >= HIDDEN_DWELL);
                if ready {
                    let rect = window_rect(hwnd);
                    let qpc = qpc_100ns();
                    let original_ex = CTX.with(|c| {
                        let mut c = c.borrow_mut();
                        c.revealed = true;
                        c.original_ex
                    });
                    // SAFETY: calls on the test's own window.
                    unsafe {
                        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
                        if original_ex & WS_EX_LAYERED.0 as isize == 0 {
                            let _ = SetWindowLongPtrW(
                                hwnd,
                                GWL_EXSTYLE,
                                ex_style(hwnd) & !(WS_EX_LAYERED.0 as isize),
                            );
                        }
                    }
                    note(format!(
                        "{:8.1} ms  REVEAL at rect {} (DWM frame {})",
                        ms_since_launch(now),
                        fmt_rect(&rect),
                        fmt_rect(&frame_bounds(hwnd))
                    ));
                    reveal = Some((now, qpc, rect));
                }
                if now.duration_since(t_launch) > LAUNCH_TIMEOUT + HIDDEN_DWELL {
                    timed_out = true;
                    return true;
                }
                false
            }
            Some((at, _, _)) => now.duration_since(at) >= AFTER_REVEAL,
        }
    });
    let end_rect = CTX
        .with(|c| c.borrow().target)
        .map(|t| (window_rect(HWND(t as *mut _)), placement(HWND(t as *mut _))));

    // Sensitivity control: move the (now opaque) spike window to the rect
    // Explorer created it at, keep capturing, and require the capture to see
    // it there. This shows a flash at the default position would have been
    // detected, whichever monitor it is on.
    let mut control: Option<(i64, RECT)> = None;
    let (target, rect_before) = CTX.with(|c| {
        let c = c.borrow();
        (c.target, c.rect_before)
    });
    if let (Some(t), false) = (target, timed_out) {
        let hwnd = HWND(t as *mut _);
        CTX.with(|c| c.borrow_mut().control = true);
        let qpc = qpc_100ns();
        // SAFETY: the test's own window.
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                rect_before.left,
                rect_before.top,
                rect_before.right - rect_before.left,
                rect_before.bottom - rect_before.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        pump_for(Duration::from_millis(600));
        control = Some((qpc, window_rect(hwnd)));
    }
    let launch_ms = launcher.join().expect("launcher thread");

    stop.store(true, Ordering::SeqCst);
    let (frames, ref_infos) = capture.join().expect("capture thread").expect("capture");

    // Restore the spike window's placement and remove the backdrops, then close
    // the window (all tabs) and remove the temp folder.
    drop(cleanup);
    let guard_result = guard.finish();
    if let Err(e) = &guard_result {
        println!("cleanup: {e}");
    }

    // ------------------------------------------------------------------
    // Report
    // ------------------------------------------------------------------
    let ctx = CTX.with(|c| std::mem::take(&mut *c.borrow_mut()));
    println!("\nShellExecuteExW returned after {launch_ms:?} ms");
    println!("timeline (ms since launch):");
    for line in &ctx.timeline {
        println!("  {line}");
    }
    println!("polling checks: {}", ctx.checks);
    if let Some((rect, wp)) = &end_rect {
        println!(
            "end rect {} (requested {}), normal pos {} showCmd {}",
            fmt_rect(rect),
            fmt_rect(&final_screen),
            fmt_rect(&wp.rcNormalPosition),
            wp.showCmd
        );
    }
    assert!(ctx.target.is_some(), "no new Explorer window was observed");
    assert!(!timed_out, "timed out (window never became visible?)");
    let (_, reveal_qpc, reveal_rect) = reveal.expect("reveal happened");

    // Capture analysis. Each frame is compared with its monitor's first frame;
    // samples that changed during the baseline period (before the launch:
    // animations, clocks, other apps) form a per-monitor noise mask and are
    // ignored afterwards.
    let to_ms = |t: i64| (t - launch_qpc) as f64 / 10_000.0;
    let mut noise: Vec<Vec<u64>> = mons
        .iter()
        .map(|m| {
            let (cols, rows) = m.grid();
            vec![0u64; (cols * rows).div_ceil(64)]
        })
        .collect();
    let mut baseline_frames = vec![0usize; mons.len()];
    for f in frames.iter().filter(|f| f.t < launch_qpc) {
        for (n, b) in noise[f.mon].iter_mut().zip(&f.bits) {
            *n |= *b;
        }
        baseline_frames[f.mon] += 1;
    }
    println!("\ncapture: {} frames total", frames.len());
    for (i, m) in mons.iter().enumerate() {
        let (cols, rows) = m.grid();
        let masked: u32 = noise[i].iter().map(|w| w.count_ones()).sum();
        println!(
            "  monitor {i}: reference {}; {} baseline frame(s); {masked} of {} samples masked as noise",
            ref_infos.get(i).map_or("", String::as_str),
            baseline_frames[i],
            cols * rows
        );
    }
    let outside = expand(&reveal_rect, OUTSIDE_MARGIN);
    let inside = expand(&reveal_rect, -24);
    let control_qpc = control.map_or(i64::MAX, |(q, _)| q);
    let control_inside = control.map(|(_, r)| expand(&r, -24));
    let mut pre_reveal_bad = Vec::new();
    let mut post_reveal_bad = Vec::new();
    let mut max_inside_fraction = 0.0f64;
    let mut max_control_fraction = 0.0f64;
    let mut pre_frames = 0;
    let mut post_frames = 0;
    let mut last_count: Vec<Option<u32>> = vec![None; mons.len()];
    for f in frames.iter().filter(|f| f.t >= launch_qpc) {
        let m = &mons[f.mon];
        let mask = &noise[f.mon];
        let (all, bbox) = count(f, m, mask, |_, _| true);
        let phase = if f.t < reveal_qpc {
            "before reveal"
        } else if f.t < control_qpc {
            "after reveal"
        } else {
            "control"
        };
        let line = format!(
            "monitor {} t={:8.1} ms changed {} bbox {} ({phase})",
            f.mon,
            to_ms(f.t),
            all,
            bbox.map_or("-".into(), |b| fmt_rect(&b)),
        );
        if last_count[f.mon] != Some(all) {
            println!("  frame {line}");
            last_count[f.mon] = Some(all);
        }
        if f.t < reveal_qpc {
            pre_frames += 1;
            if all > SAMPLE_TOLERANCE {
                pre_reveal_bad.push(line);
            }
        } else if f.t < control_qpc {
            post_frames += 1;
            let (out, obox) = count(f, m, mask, |x, y| !contains(&outside, x, y));
            if out > SAMPLE_TOLERANCE {
                post_reveal_bad.push(format!(
                    "{line}; outside final rect {out} bbox {}",
                    obox.map_or("-".into(), |b| fmt_rect(&b))
                ));
            }
            let total = samples_in(m, mask, &inside);
            if total > 0 {
                let (inn, _) = count(f, m, mask, |x, y| contains(&inside, x, y));
                max_inside_fraction = max_inside_fraction.max(f64::from(inn) / f64::from(total));
            }
        } else if let Some(region) = &control_inside {
            let total = samples_in(m, mask, region);
            if total > 0 {
                let (inn, _) = count(f, m, mask, |x, y| contains(region, x, y));
                max_control_fraction = max_control_fraction.max(f64::from(inn) / f64::from(total));
            }
        }
    }
    println!(
        "capture: {pre_frames} frame(s) after launch and before reveal, {post_frames} after reveal; \
         best fill of final rect after reveal {:.0}%; best fill of Explorer's default rect {} \
         during the control {:.0}%",
        max_inside_fraction * 100.0,
        control.map_or("-".into(), |(_, r)| fmt_rect(&r)),
        max_control_fraction * 100.0
    );

    let capture_hide_ok = pre_reveal_bad.is_empty();
    let capture_place_ok = post_reveal_bad.is_empty();
    let positive_control = max_inside_fraction >= 0.5;
    let sensitivity_control = max_control_fraction >= 0.5;
    let poll_hide_ok = ctx.hide_violations.is_empty();
    let poll_place_ok = ctx.placement_violations.is_empty();
    let at_requested = reveal_rect == final_screen;

    println!("\n=== Spike R1 result ===");
    println!(
        "method: Windows.Graphics.Capture (all monitors, magenta backdrop, 1/16 sampling, diff vs first frame) + ~15 ms/event polling"
    );
    println!("positive control (window seen in final rect after reveal): {positive_control}");
    println!(
        "sensitivity control (window seen when moved to Explorer's default rect): {sensitivity_control}"
    );
    println!("capture: no frame shows the window before reveal: {capture_hide_ok}");
    for l in &pre_reveal_bad {
        println!("    pre-reveal: {l}");
    }
    println!(
        "capture: no frame shows the window outside its final rect after reveal: {capture_place_ok}"
    );
    for l in &post_reveal_bad {
        println!("    post-reveal: {l}");
    }
    println!("polling: transparent until reveal: {poll_hide_ok}");
    for l in &ctx.hide_violations {
        println!("    {l}");
    }
    println!("polling: never visible+opaque away from final rect: {poll_place_ok}");
    for l in &ctx.placement_violations {
        println!("    {l}");
    }
    println!(
        "rect at reveal equals requested rect: {at_requested} ({} vs {})",
        fmt_rect(&reveal_rect),
        fmt_rect(&final_screen)
    );
    let layered_ok = capture_hide_ok && capture_place_ok && poll_hide_ok && poll_place_ok;
    println!(
        "verdict: HideStrategy::{}",
        if layered_ok {
            "LayeredAlpha"
        } else {
            "PlaceOnlyNoHide (LayeredAlpha failed)"
        }
    );

    assert!(
        positive_control,
        "inconclusive: capture never saw the revealed window"
    );
    assert!(
        sensitivity_control,
        "inconclusive: capture did not see the window at Explorer's default rect, so a flash there could go undetected"
    );
    assert!(capture_hide_ok, "a frame showed the window before reveal");
    assert!(
        capture_place_ok,
        "a frame showed the window at an intermediate placement"
    );
    assert!(
        poll_hide_ok,
        "window was visible and not transparent before reveal"
    );
    assert!(
        poll_place_ok,
        "window was visible and opaque away from its final rect"
    );
    guard_result.expect("test windows left open");
}
