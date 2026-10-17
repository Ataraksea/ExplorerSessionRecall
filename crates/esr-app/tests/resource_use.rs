//! Resource use of the Session_Monitor (task 16.4; Requirements 1.5, 1.6).
//!
//! All tests are `#[ignore]` (they run the real runtime against the real
//! shell). Run them alone, one at a time:
//!
//! ```text
//! cargo test -p esr-app --test resource_use -- --ignored --nocapture --test-threads=1
//! ```
//!
//! # Isolation
//!
//! Every measurement is taken on a **separate child process**: the test
//! binary re-starts itself (`current_exe`) running only the helper test
//! [`resource_child`], selected through the `ESR_RESOURCE_CHILD`
//! environment variable (without it the helper returns at once, so a plain
//! `--ignored` run is harmless). The child runs `esr_app::runtime::run` with
//! a temporary data directory (`%TEMP%\esr_resource_*`, deleted afterwards),
//! `RuntimeOptions { ui: false, panic_guard: false, .. }`, and no
//! Session_File, so it never restores anything, never touches the real data
//! directory, the real settings, or start at sign-in, and does not take the
//! single-instance mutex. It is stopped with `ESR_EXIT`, posted only to the
//! `ESR.Engine` window owned by the child's PID (never to a real instance),
//! and killed if it does not exit (also on panic, and by its own 10-minute
//! watchdog if the parent dies).
//!
//! Every test holds the `ExplorerGuard` (and with it the session-wide
//! Explorer test mutex) for its whole duration, so no other guarded test
//! opens or closes Explorer windows while a measurement runs. Windows the
//! user had open before the test are tracked read-only by the child (as by
//! a real instance) and are never touched; any user input during a CPU
//! interval is reported.
//!
//! # What is measured
//!
//! * **Idle CPU (Req 1.5)**: CPU of the child over 60 s of wall time, after
//!   a settle period, as a percentage of one logical processor. Measured
//!   two ways, asserting on the larger: `GetProcessTimes` (kernel + user,
//!   charged at clock ticks, so short bursts can read as 0) and
//!   `QueryProcessCycleTime` (exact cycles, converted with a rate
//!   calibrated on this thread). Once with no test window open, once with
//!   two guard-opened Explorer windows being tracked.
//! * **Private memory (Req 1.6)**: `PROCESS_MEMORY_COUNTERS_EX.PrivateUsage`
//!   (the process's private commit, "Commit size" in Task Manager; stricter
//!   than the private working set, which is printed too).
//!
//!   Opening 100 real Explorer windows with 500 tabs on a test VM is too
//!   heavy and too risky, so the 100 × 5 load is synthetic: in one child
//!   process the full real runtime runs with all adapters started (the
//!   baseline), and next to it a second `RuntimeCore` with inert ports (a
//!   real `DurableFileStore` and log thread in the temp directory, no-op
//!   navigator/strip/prober, nothing that can touch Explorer, the registry,
//!   or start at sign-in) is fed `WindowScanned` observations for 100
//!   windows × 5 tabs with realistic Locations (local, Unicode, UNC,
//!   mapped-drive, long > 260-char paths, shell folders; PIDLs sized like
//!   real file-system ITEMIDLISTs), then ten rounds of navigations with
//!   checkpoint writes. The whole child process must stay below 50 MB. This
//!   over-counts (two Engines, two writer and log threads, the test
//!   harness) and does not include the per-window state of the real
//!   adapters (COM sinks, UIA subscriptions, hooks), which
//!   [`private_memory_per_real_window`] measures with up to 9 real windows
//!   and extrapolates.
//!
//! The test binary is a debug build unless run with `--release`; numbers
//! from a release `esr.exe` are lower.

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{BufRead, BufReader};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use esr_app::instance::{ENGINE_WINDOW_CLASS, InstanceMessages};
use esr_app::ports::{Navigator, Notifier, Options, Prober, Startup, StartupMechanism, TabStrip};
use esr_app::runtime::{
    Host, Inbound, LogThread, Mailbox, Ports, RuntimeCore, RuntimeOptions, Storage, local_now,
    now, read_environment, run, startup_input,
};
use esr_app::ui::UiCommand;
use esr_core::engine::{
    Environment, Event, ExplorerOptionValues, IoErrorKind, ItemId, Millis, ObservedLocation,
    ProbeId, RequestId, SessionFileRead, ShellError, TabKey, TabObservation, TabStripState,
    WinKey, WindowObservation,
};
use esr_core::format::parse;
use esr_core::model::{
    DisplayRef, LocationId, LocationKind, Placement, Rect, SessionDoc, ShellKind, ShowState,
    Timestamp, Volume, WideString,
};
use esr_core::plan::placement::FinalPlacement;
use esr_core::settings::Settings;
use esr_win::file_store::DurableFileStore;
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows};
use explorer_guard::{ExplorerGuard, parsing_name, pump_for, pump_once};
use windows::Win32::Foundation::{FILETIME, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, PostMessageW, SW_SHOWNOACTIVATE,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Requirement 1.5: average CPU below 1 % of one logical processor.
const CPU_LIMIT_PERCENT: f64 = 1.0;
/// Requirement 1.6: private memory below 50 MB.
const MEMORY_LIMIT_BYTES: usize = 50 * 1024 * 1024;
/// Length of an idle CPU interval (Req 1.5).
const IDLE_INTERVAL: Duration = Duration::from_secs(60);
/// Time the child gets after start (or after windows opened) before measuring.
const SETTLE: Duration = Duration::from_secs(10);

const CHILD_ENV: &str = "ESR_RESOURCE_CHILD";
const DATA_ENV: &str = "ESR_RESOURCE_DATA_DIR";
const MODE_RUNTIME: &str = "runtime";
const MODE_SYNTHETIC: &str = "synthetic";
/// The child exits on its own after this long (parent died or hung).
const CHILD_WATCHDOG: Duration = Duration::from_secs(10 * 60);
/// Prefix of the metric lines the synthetic child prints.
const METRIC: &str = "ESR_METRIC";

const MB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------------------
// Process metrics
// ---------------------------------------------------------------------------

/// `PROCESS_MEMORY_COUNTERS_EX2` (the `EX` layout plus two fields, Windows
/// 10 1809+). Declared here because the `Win32_System_ProcessStatus`
/// feature of the `windows` crate is not enabled for this crate.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ProcessMemoryCountersEx2 {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
    private_working_set_size: usize,
    shared_commit_usage: u64,
}

/// Size of the `PROCESS_MEMORY_COUNTERS_EX` prefix.
const COUNTERS_EX_SIZE: usize = std::mem::offset_of!(ProcessMemoryCountersEx2, private_working_set_size);

// Declared here because the `windows` crate features that carry these
// (`Win32_System_ProcessStatus`, `Win32_System_WindowsProgramming`) are not
// enabled for this crate.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut ProcessMemoryCountersEx2, cb: u32) -> i32;
    fn QueryProcessCycleTime(process: *mut c_void, cycles: *mut u64) -> i32;
    fn QueryThreadCycleTime(thread: *mut c_void, cycles: *mut u64) -> i32;
}

#[derive(Clone, Copy, Debug, Default)]
struct Memory {
    /// Private commit (`PrivateUsage`), the measure Req 1.6 is checked on.
    private: usize,
    /// Peak commit (`PeakPagefileUsage`).
    peak_private: usize,
    /// Private working set (0 if the EX2 layout is unavailable).
    private_ws: usize,
    working_set: usize,
}

impl Memory {
    fn describe(&self) -> String {
        format!(
            "private {:.2} MB (peak {:.2} MB), private working set {:.2} MB, working set {:.2} MB",
            self.private as f64 / MB,
            self.peak_private as f64 / MB,
            self.private_ws as f64 / MB,
            self.working_set as f64 / MB
        )
    }
}

fn memory_of(process: HANDLE) -> Memory {
    let mut c = ProcessMemoryCountersEx2::default();
    // SAFETY: valid, sized out structure; the handle has query access.
    let mut ok = unsafe {
        K32GetProcessMemoryInfo(process.0, &mut c, size_of::<ProcessMemoryCountersEx2>() as u32)
    } != 0;
    if !ok {
        c = ProcessMemoryCountersEx2::default();
        // SAFETY: as above, with the smaller EX layout.
        ok = unsafe { K32GetProcessMemoryInfo(process.0, &mut c, COUNTERS_EX_SIZE as u32) } != 0;
        c.private_working_set_size = 0;
    }
    assert!(ok, "GetProcessMemoryInfo failed: {}", std::io::Error::last_os_error());
    Memory {
        private: c.private_usage,
        peak_private: c.peak_pagefile_usage,
        private_ws: c.private_working_set_size,
        working_set: c.working_set_size,
    }
}

fn ticks(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// Kernel + user CPU time of `process`. Windows charges these at clock
/// ticks (≈ 15.6 ms), so short bursts can be missed; see [`cycles_of`].
fn cpu_time_of(process: HANDLE) -> Duration {
    let (mut create, mut exit, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers; the handle has query access.
    unsafe { GetProcessTimes(process, &mut create, &mut exit, &mut kernel, &mut user) }
        .expect("GetProcessTimes");
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

/// CPU cycles charged to every thread of `process` (exact accounting, not
/// tick-sampled).
fn cycles_of(process: HANDLE) -> u64 {
    let mut cycles = 0u64;
    // SAFETY: valid out pointer; the handle has query access.
    let ok = unsafe { QueryProcessCycleTime(process.0, &mut cycles) } != 0;
    assert!(ok, "QueryProcessCycleTime: {}", std::io::Error::last_os_error());
    cycles
}

/// Cycle-counter rate, calibrated with a 300 ms busy loop on this thread.
/// Preemption during the loop only lowers the estimate, which raises the
/// cycle-based CPU percentage (conservative).
fn cycles_per_second() -> f64 {
    let thread_cycles = || {
        let mut c = 0u64;
        // SAFETY: pseudo-handle of the current thread; valid out pointer.
        let ok = unsafe { QueryThreadCycleTime(GetCurrentThread().0, &mut c) } != 0;
        assert!(ok, "QueryThreadCycleTime: {}", std::io::Error::last_os_error());
        c
    };
    let c0 = thread_cycles();
    let t0 = Instant::now();
    let mut x = 0u64;
    while t0.elapsed() < Duration::from_millis(300) {
        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    let rate = (thread_cycles() - c0) as f64 / t0.elapsed().as_secs_f64();
    println!("cycle counter: {:.3} GHz (calibrated)", rate / 1e9);
    rate
}

// ---------------------------------------------------------------------------
// Child process
// ---------------------------------------------------------------------------

/// A child copy of this test binary running [`resource_child`]. Dropping it
/// stops the child: `ESR_EXIT` to its Engine window, then a kill.
struct ChildRuntime {
    child: Child,
    data_dir: PathBuf,
    metrics: Option<std::sync::mpsc::Receiver<String>>,
}

impl ChildRuntime {
    fn spawn(mode: &str) -> Self {
        let data_dir =
            std::env::temp_dir().join(format!("esr_resource_{mode}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = Process::new(exe);
        cmd.args(["resource_child", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, mode)
            .env(DATA_ENV, &data_dir)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit());
        let piped = mode == MODE_SYNTHETIC;
        cmd.stdout(if piped { Stdio::piped() } else { Stdio::inherit() });
        let mut child = cmd.spawn().expect("start the child process");
        let metrics = piped.then(|| {
            let out = child.stdout.take().expect("child stdout");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    println!("  [child] {line}");
                    // The harness may print "test resource_child ... " first.
                    if let Some(i) = line.find(METRIC) {
                        let _ = tx.send(line[i..].to_string());
                    }
                }
            });
            rx
        });
        println!("started child PID {} ({mode}), data dir {}", child.id(), data_dir.display());
        Self { child, data_dir, metrics }
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.child.as_raw_handle())
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Waits for the child's `ESR.Engine` window.
    fn wait_engine(&mut self, timeout: Duration) -> Option<HWND> {
        let pid = self.pid();
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(h) = engine_window_of(pid) {
                return Some(h);
            }
            if !self.running() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// `ESR_EXIT` to the child's Engine window, then waits; kills on timeout.
    /// Returns the exit code if the child exited by itself.
    fn stop(&mut self) -> Option<i32> {
        if !self.running() {
            return self.child.try_wait().ok().flatten().and_then(|s| s.code());
        }
        if let Some(engine) = engine_window_of(self.pid()) {
            let exit = InstanceMessages::register().exit;
            // SAFETY: posting a registered message to the child's own window.
            let _ = unsafe { PostMessageW(Some(engine), exit, WPARAM(0), LPARAM(0)) };
        }
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("child PID {} did not exit after ESR_EXIT; killing it", self.pid());
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }

    /// The child's metric lines as `name -> value`, until it exits.
    fn collect_metrics(&mut self, timeout: Duration) -> HashMap<String, f64> {
        let mut map = HashMap::new();
        let Some(rx) = self.metrics.take() else { return map };
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    let mut parts = line.split_whitespace().skip(1);
                    if let (Some(k), Some(v)) = (parts.next(), parts.next())
                        && let Ok(v) = v.parse::<f64>()
                    {
                        map.insert(k.to_string(), v);
                    }
                }
                Err(_) => break,
            }
        }
        map
    }

    fn read_doc(&self) -> Option<SessionDoc> {
        parse(&std::fs::read(self.data_dir.join("session.esr")).ok()?).ok()
    }
}

impl Drop for ChildRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
        for _ in 0..20 {
            if !self.data_dir.exists() || std::fs::remove_dir_all(&self.data_dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// The `ESR.Engine` window owned by process `pid`.
fn engine_window_of(pid: u32) -> Option<HWND> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: plain window enumeration by class.
        let hwnd = unsafe { FindWindowExW(None, after, &class, PCWSTR::null()) }.ok()?;
        if hwnd.is_invalid() {
            return None;
        }
        let mut owner = 0;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        if owner == pid {
            return Some(hwnd);
        }
        after = Some(hwnd);
    }
}

/// Helper process for the tests below (see the module docs). Does nothing
/// unless `ESR_RESOURCE_CHILD` is set.
#[test]
#[ignore = "helper process of the resource-use tests; does nothing when run directly"]
fn resource_child() {
    let Ok(mode) = std::env::var(CHILD_ENV) else { return };
    let data_dir = PathBuf::from(std::env::var_os(DATA_ENV).expect("ESR_RESOURCE_DATA_DIR"));
    std::thread::spawn(|| {
        std::thread::sleep(CHILD_WATCHDOG);
        eprintln!("resource child: watchdog expired, exiting");
        std::process::exit(3);
    });
    match mode.as_str() {
        MODE_RUNTIME => {
            let code = run_runtime(&data_dir);
            assert_eq!(code, 0, "runtime exit code");
        }
        MODE_SYNTHETIC => synthetic_child(&data_dir),
        other => panic!("unknown child mode {other}"),
    }
}

/// The real runtime on the calling thread, until `ESR_EXIT`.
fn run_runtime(data_dir: &Path) -> i32 {
    let storage = Storage::open(data_dir);
    let input = startup_input(SessionFileRead::Absent, false);
    let options = RuntimeOptions { ui: false, panic_guard: false, ..RuntimeOptions::default() };
    run(Settings::default(), input, storage, options)
}

fn metric(name: &str, value: impl std::fmt::Display) {
    println!("{METRIC} {name} {value}");
}

fn self_memory() -> Memory {
    // SAFETY: pseudo-handle of the current process.
    memory_of(unsafe { GetCurrentProcess() })
}

fn report_memory(stage: &str, m: Memory) {
    metric(&format!("{stage}_private"), m.private);
    metric(&format!("{stage}_private_ws"), m.private_ws);
    println!("{stage}: {}", m.describe());
}

/// Synthetic child: the real runtime (baseline), plus a second
/// `RuntimeCore` with inert ports holding 100 windows × 5 tabs.
fn synthetic_child(data_dir: &Path) {
    let runtime = {
        let dir = data_dir.join("runtime");
        std::thread::Builder::new()
            .name("esr-engine-resource".into())
            .spawn(move || run_runtime(&dir))
            .expect("spawn the runtime")
    };
    let pid = std::process::id();
    let end = Instant::now() + Duration::from_secs(20);
    while engine_window_of(pid).is_none() {
        assert!(Instant::now() < end, "the runtime created no ESR.Engine window");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(SETTLE);
    let baseline = self_memory();
    report_memory("baseline", baseline);

    // The synthetic Live_Session.
    let synthetic_dir = data_dir.join("synthetic");
    let inbox = std::sync::Arc::new(Mailbox::new());
    let files = {
        let sink = inbox.clone();
        DurableFileStore::open(
            &synthetic_dir,
            Box::new(move |seq, result| sink.push(Inbound::Event(Event::WriteCompleted { seq, result }))),
        )
    };
    let log_thread = LogThread::spawn(synthetic_dir.clone());
    let ports = Ports {
        navigator: Box::new(Inert),
        tab_strip: Box::new(Inert),
        prober: Box::new(Inert),
        hider: Box::new(Inert),
        fold_probe: Box::new(Inert),
        files: Box::new(files),
        log: Box::new(log_thread.writer()),
        options: Box::new(Inert),
        notifier: Box::new(Inert),
        startup: Box::new(Inert),
        host: Box::new(InertHost),
    };
    let mut t = now();
    let mut core = RuntimeCore::new(
        Settings::default(),
        startup_input(SessionFileRead::Absent, false),
        ports,
        t,
    );
    let displays = read_environment(t).displays;
    let display = displays
        .iter()
        .find(|d| d.primary)
        .or(displays.first())
        .map(|d| DisplayRef { device: d.device.clone(), monitor: d.monitor })
        .unwrap_or(DisplayRef {
            device: r"\\.\DISPLAY1".into(),
            monitor: Rect { left: 0, top: 0, right: 1920, bottom: 1080 },
        });

    let mut rng = LocationGen::new(0x5eed_1234);
    let mut session = SyntheticSession::new(&mut rng, &display);
    let mut location_bytes = 0usize;
    for w in &session.windows {
        for (_, loc) in &w.tabs {
            location_bytes += loc.parsing_name.len() * 2 + loc.display_name.len() * 2 + loc.pidl.len();
        }
    }
    metric("location_bytes", location_bytes);
    for obs in session.observations() {
        t = t + 3;
        core.handle(t, Inbound::Event(Event::WindowScanned(obs)));
    }
    settle(&mut core, &inbox, &mut t);
    let tracker = core.engine().tracker();
    metric("tracked_windows", tracker.window_count());
    metric("tracked_tabs", tracker.tab_count());
    let loaded = self_memory();
    report_memory("loaded", loaded);

    // Churn: navigate every tab of 20 windows per round, with checkpoints.
    let mut steady_max = loaded;
    for round in 0..10 {
        session.navigate_some(&mut rng, round);
        for obs in session.observations() {
            t = t + 3;
            core.handle(t, Inbound::Event(Event::WindowScanned(obs)));
        }
        settle(&mut core, &inbox, &mut t);
        let m = self_memory();
        if m.private > steady_max.private {
            steady_max = m;
        }
    }
    report_memory("steady_max", steady_max);
    let tracker = core.engine().tracker();
    metric("final_windows", tracker.window_count());
    metric("final_tabs", tracker.tab_count());
    let doc = parse(&std::fs::read(synthetic_dir.join("session.esr")).unwrap_or_default());
    match doc {
        Ok(doc) => {
            metric("file_windows", doc.snapshot.windows.len());
            metric("file_tabs", doc.snapshot.tab_count());
        }
        Err(e) => println!("synthetic Session_File unreadable: {e:?}"),
    }
    if let Ok(meta) = std::fs::metadata(synthetic_dir.join("session.esr")) {
        metric("file_bytes", meta.len());
    }
    metric("peak_private", self_memory().peak_private);

    drop(core);
    drop(log_thread);
    // Stop the real runtime of this process.
    if let Some(engine) = engine_window_of(pid) {
        let exit = InstanceMessages::register().exit;
        // SAFETY: posting a registered message to this process's own window.
        let _ = unsafe { PostMessageW(Some(engine), exit, WPARAM(0), LPARAM(0)) };
    }
    assert_eq!(runtime.join().expect("runtime thread"), 0);
    metric("done", 1);
}

/// Delivers write completions and due timers until nothing is due within 5 s.
fn settle(core: &mut RuntimeCore, inbox: &Mailbox, t: &mut Millis) {
    for _ in 0..200 {
        let _ = core.ports_mut().files.flush(5_000);
        let at = *t;
        core.drain(inbox, &mut || at);
        match core.next_deadline() {
            Some(d) if d.0 <= t.0 + 5_000 => {
                if d > *t {
                    *t = d;
                }
                core.tick(*t);
            }
            _ => break,
        }
    }
    let _ = core.ports_mut().files.flush(5_000);
    let at = *t;
    core.drain(inbox, &mut || at);
}

// --- inert ports (nothing reaches Explorer, the registry, or the logon task)

struct Inert;

fn inert_error() -> ShellError {
    ShellError { code: 0, message: "inert test port".into() }
}

impl Navigator for Inert {
    fn open_window(&mut self, _: RequestId, _: &LocationId, _: &FinalPlacement) -> Result<(), ShellError> {
        Err(inert_error())
    }
    fn apply_placement(&mut self, _: WinKey, _: &FinalPlacement) -> Result<(), ShellError> {
        Ok(())
    }
    fn create_tab(&mut self, _: WinKey) -> Result<(), ShellError> {
        Err(inert_error())
    }
    fn navigate(&mut self, _: TabKey, _: &LocationId) -> Result<(), ShellError> {
        Err(inert_error())
    }
    fn close_tab(&mut self, _: TabKey) -> Result<(), ShellError> {
        Err(inert_error())
    }
    fn select_items(&mut self, _: TabKey, _: &[ItemId]) -> Result<(), ShellError> {
        Ok(())
    }
    fn reveal(&mut self, _: WinKey) -> Result<(), ShellError> {
        Ok(())
    }
    fn set_z_order(&mut self, _: &[WinKey]) -> Result<(), ShellError> {
        Ok(())
    }
    fn focus(&mut self, _: WinKey) -> Result<(), ShellError> {
        Ok(())
    }
}

impl TabStrip for Inert {
    fn read(&mut self, _: WinKey) -> Result<TabStripState, ShellError> {
        Err(inert_error())
    }
    fn activate(&mut self, _: esr_core::engine::OpId, _: WinKey, _: usize) -> Result<(), ShellError> {
        Err(inert_error())
    }
    fn watch(&mut self, _: WinKey) -> Result<(), ShellError> {
        Ok(())
    }
    fn unwatch(&mut self, _: WinKey) {}
}

impl Prober for Inert {
    fn probe(&mut self, _: ProbeId, _: &LocationId) {}
}

impl esr_app::ports::Hider for Inert {
    fn hide(&mut self, _: WinKey) -> Result<(), ShellError> {
        Err(inert_error())
    }
    fn reveal(&mut self, _: WinKey) -> Result<(), ShellError> {
        Ok(())
    }
    fn sweep(&mut self) -> u32 {
        0
    }
}

impl esr_app::ports::FoldProbe for Inert {
    fn probe_fold_context(&mut self, _: esr_core::engine::FoldProbeId, _: WinKey) {}
}

impl Options for Inert {
    fn read(&mut self) -> ExplorerOptionValues {
        ExplorerOptionValues::default()
    }
    fn set_persist_browsers(&mut self, _: bool) -> Result<(), IoErrorKind> {
        Err(IoErrorKind::AccessDenied)
    }
}

impl Notifier for Inert {
    fn notify_omitted(&mut self, _: u32) -> Result<(), ShellError> {
        Ok(())
    }
}

impl Startup for Inert {
    fn set_enabled(&mut self, _: bool) -> Result<StartupMechanism, ShellError> {
        Err(inert_error())
    }
    fn unregister(&mut self) -> Result<(), ShellError> {
        Err(inert_error())
    }
}

struct InertHost;

impl Host for InertHost {
    fn rescan_window(&mut self, _: WinKey) {}
    fn rescan_all(&mut self) -> bool {
        true
    }
    fn watch_process(&mut self, _: u32) -> Result<(), ShellError> {
        Ok(())
    }
    fn forget_window(&mut self, _: WinKey) {}
    fn environment(&mut self, now: Millis) -> Environment {
        read_environment(now)
    }
    fn local_now(&mut self) -> Timestamp {
        local_now()
    }
    fn ui(&mut self, _: UiCommand) {}
}

// --- synthetic Live_Session ---------------------------------------------

const SYNTHETIC_WINDOWS: usize = 100;
const SYNTHETIC_TABS_PER_WINDOW: usize = 5;
/// Main Explorer process and a separate-process Explorer (Req 2.5).
const SYNTHETIC_PIDS: [u32; 2] = [4_242, 4_343];

const WORDS: &[&str] = &[
    "Projects", "Documents", "Reports", "2024", "2025", "Quarterly", "Archive", "Invoices",
    "src", "assets", "build-output", "Customer Data", "Marketing Material", "Übersicht",
    "Präsentationen", "資料", "写真", "проекты", "Résumés", "Backups", "node_modules",
    "Design Reviews", "Release Notes", "very_long_directory_name_for_testing_purposes",
];

/// Deterministic xorshift generator of realistic Locations.
struct LocationGen(u64);

impl LocationGen {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn word(&mut self) -> &'static str {
        WORDS[self.below(WORDS.len())]
    }

    /// `prefix` plus components until the path is at least `min_len` long.
    fn path(&mut self, prefix: &str, min_len: usize, max_components: usize) -> Vec<String> {
        let mut comps = Vec::new();
        let mut len = prefix.len();
        while (len < min_len || comps.is_empty()) && comps.len() < max_components {
            let c = format!("{} {}", self.word(), self.below(1000));
            len += c.len() + 1;
            comps.push(c);
        }
        comps
    }

    /// A Location: 65 % local, 10 % long (> 260 chars), 10 % UNC, 10 %
    /// mapped drive, 5 % shell folders.
    fn location(&mut self) -> LocationId {
        let roll = self.below(100);
        let (kind, prefix, min, max_components) = match roll {
            0..=64 => (LocationKind::FileSystem(Volume::Fixed), r"C:\Users\TestUser", 30 + self.below(200), 20),
            65..=74 => (LocationKind::FileSystem(Volume::Fixed), r"D:\Data", 300 + self.below(700), 200),
            75..=84 => (
                LocationKind::FileSystem(Volume::Unc),
                r"\\fileserver01.corp.example\Shared",
                40 + self.below(160),
                20,
            ),
            85..=94 => (LocationKind::FileSystem(Volume::Mapped), "Z:", 20 + self.below(120), 20),
            _ => {
                return LocationId {
                    case_sensitive: false,
                    kind: LocationKind::Shell(ShellKind::ThisPc),
                    parsing_name: wide("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
                    display_name: wide("This PC"),
                    pidl: pidl_of(&["This PC"], 20),
                    search: None,
                };
            }
        };
        let comps = self.path(prefix, min, max_components);
        let parsing = std::iter::once(prefix)
            .chain(comps.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\\");
        let comp_refs: Vec<&str> = comps.iter().map(String::as_str).collect();
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: wide(&parsing),
            display_name: wide(comps.last().map_or("C:", String::as_str)),
            pidl: pidl_of(&comp_refs, 0),
            search: None,
        }
    }
}

fn wide(s: &str) -> WideString {
    WideString::new(s.encode_utf16().collect())
}

/// An ITEMIDLIST of realistic size: a 20-byte root item, then one item per
/// component sized like a file-system item with its `BEEF0004` extension
/// block (≈ 62 bytes plus the UTF-16 long name), then the terminator.
/// `fixed_item` > 0 makes a single item of that size instead.
fn pidl_of(components: &[&str], fixed_item: usize) -> Vec<u8> {
    let mut sizes = vec![20usize];
    if fixed_item == 0 {
        sizes.extend(components.iter().map(|c| 62 + 2 * (c.encode_utf16().count() + 1)));
    } else {
        sizes = vec![fixed_item];
    }
    let mut out = Vec::new();
    for (i, size) in sizes.into_iter().enumerate() {
        out.extend_from_slice(&(size as u16).to_le_bytes());
        out.extend((2..size).map(|j| (i * 31 + j * 7) as u8));
    }
    out.extend_from_slice(&[0, 0]);
    out
}

struct SyntheticWindow {
    win: WinKey,
    pid: u32,
    tabs: Vec<(TabKey, LocationId)>,
    active: usize,
    placement: Placement,
}

struct SyntheticSession {
    windows: Vec<SyntheticWindow>,
}

impl SyntheticSession {
    fn new(rng: &mut LocationGen, display: &DisplayRef) -> Self {
        let windows = (0..SYNTHETIC_WINDOWS)
            .map(|w| {
                let tabs = (0..SYNTHETIC_TABS_PER_WINDOW)
                    .map(|t| {
                        let key = TabKey { hwnd: 0x20_0000 + (w * 16 + t) as u64 * 4, seq: (w * 16 + t) as u64 + 1 };
                        (key, rng.location())
                    })
                    .collect();
                let x = (w % 10) as i32 * 40;
                let y = (w / 10) as i32 * 30;
                SyntheticWindow {
                    win: WinKey { hwnd: 0x10_0000 + w as u64 * 4, seq: w as u64 + 1 },
                    pid: SYNTHETIC_PIDS[usize::from(w % 7 == 0)],
                    tabs,
                    active: w % SYNTHETIC_TABS_PER_WINDOW,
                    placement: Placement {
                        state: if w % 9 == 0 { ShowState::Maximized } else { ShowState::Normal },
                        restored: Rect { left: x, top: y, right: x + 1200, bottom: y + 800 },
                        display: display.clone(),
                    },
                }
            })
            .collect();
        Self { windows }
    }

    fn observations(&self) -> Vec<WindowObservation> {
        self.windows
            .iter()
            .map(|w| WindowObservation {
                win: w.win,
                pid: w.pid,
                tabs: w
                    .tabs
                    .iter()
                    .map(|(tab, loc)| TabObservation { tab: *tab, location: ObservedLocation::Known(loc.clone()) })
                    .collect(),
                active: Some(w.tabs[w.active].0),
                strip: Some(TabStripState {
                    names: w.tabs.iter().map(|(_, l)| l.display_name.clone()).collect(),
                    selected: Some(w.active),
                }),
                placement: w.placement.clone(),
            })
            .collect()
    }

    /// Navigates every tab of 20 windows (a different fifth each round).
    fn navigate_some(&mut self, rng: &mut LocationGen, round: usize) {
        for (i, w) in self.windows.iter_mut().enumerate() {
            if i % 5 == round % 5 {
                for (_, loc) in &mut w.tabs {
                    *loc = rng.location();
                }
                w.active = (w.active + 1) % w.tabs.len();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Real Explorer windows (through the guard)
// ---------------------------------------------------------------------------

fn normalize(p: &str) -> String {
    p.strip_prefix(r"\\?\").unwrap_or(p).trim_end_matches('\\').to_lowercase()
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Opens a new Explorer window on `dir` (from a launcher STA thread).
fn request_open(dir: PathBuf) -> Result<(), String> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        // SAFETY: valid path string; the PIDL is freed after the call.
        unsafe {
            let pidl = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
            if pidl.is_null() {
                return Err("ILCreateFromPathW failed".to_string());
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
            r.map_err(|e| format!("ShellExecuteExW: {e}"))
        }
    })
    .join()
    .map_err(|_| "launcher panicked".to_string())?
}

/// Opens a window on `dir` and adopts it into the guard.
fn open_window(guard: &ExplorerGuard, dir: &Path) -> Result<HWND, String> {
    request_open(dir.to_path_buf())?;
    let target = normalize(&dir.to_string_lossy());
    wait(Duration::from_secs(20), || {
        enumerate_all_explorer_windows()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| !guard.is_pre_existing(g.top_hwnd) && !guard.is_adopted(g.top_hwnd))
            .find(|g| {
                g.tabs.iter().any(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .is_some_and(|n| normalize(&n) == target)
                })
            })
            .map(|g| {
                guard.adopt(g.top_hwnd);
                g.top_hwnd
            })
    })
    .ok_or_else(|| format!("no window opened on {}", dir.display()))
}

/// Waits until the child's Session_File lists every folder.
fn wait_tracked(child: &ChildRuntime, folders: &[PathBuf]) -> Result<(), String> {
    let targets: Vec<String> = folders.iter().map(|f| normalize(&f.to_string_lossy())).collect();
    wait(Duration::from_secs(15), || {
        let doc = child.read_doc()?;
        let listed: Vec<String> = doc
            .snapshot
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .map(|t| normalize(&t.location.parsing_name.to_string_lossy()))
            .collect();
        targets.iter().all(|t| listed.contains(t)).then_some(())
    })
    .ok_or_else(|| "the child's Session_File does not list every test window".into())
}

/// Last input tick (`GetLastInputInfo`), to report input during an interval.
fn last_input() -> Option<u64> {
    esr_app::runtime::last_input_at(now()).map(|m| m.0)
}

struct CpuSample {
    /// The larger of the time-based and the cycle-based figure.
    percent: f64,
    cpu: Duration,
    cycles: u64,
    wall: Duration,
    user_input: bool,
}

/// CPU of the child over [`IDLE_INTERVAL`] (pumping this thread's messages),
/// from `GetProcessTimes` and from `QueryProcessCycleTime`.
fn measure_idle_cpu(child: &mut ChildRuntime, label: &str, hz: f64) -> CpuSample {
    let input_before = last_input();
    let cpu0 = cpu_time_of(child.handle());
    let cycles0 = cycles_of(child.handle());
    let t0 = Instant::now();
    pump_for(IDLE_INTERVAL);
    let cpu1 = cpu_time_of(child.handle());
    let cycles1 = cycles_of(child.handle());
    let wall = t0.elapsed();
    let user_input = last_input() != input_before;
    assert!(child.running(), "the child exited during the measurement ({label})");
    let cpu = cpu1.saturating_sub(cpu0);
    let cycles = cycles1.saturating_sub(cycles0);
    let by_time = cpu.as_secs_f64() / wall.as_secs_f64() * 100.0;
    let by_cycles = cycles as f64 / hz / wall.as_secs_f64() * 100.0;
    println!(
        "idle CPU ({label}) over {:.2} s: GetProcessTimes {:.1} ms = {by_time:.4} %; \
         {cycles} cycles ≈ {:.2} ms = {by_cycles:.4} % of one logical processor{}",
        wall.as_secs_f64(),
        cpu.as_secs_f64() * 1000.0,
        cycles as f64 / hz * 1000.0,
        if user_input { " (user input occurred during the interval)" } else { "" }
    );
    CpuSample { percent: by_time.max(by_cycles), cpu, cycles, wall, user_input }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Req 1.5: idle CPU < 1 % over 60 s, with no test window and with two
/// tracked guard-opened windows.
#[test]
#[ignore = "runs the runtime against File Explorer and opens 2 windows; ~3 min; run alone with --test-threads=1"]
fn idle_cpu_below_one_percent_over_60_s() {
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("resource_cpu");
    let hz = cycles_per_second();
    let mut child = ChildRuntime::spawn(MODE_RUNTIME);
    assert!(child.wait_engine(Duration::from_secs(20)).is_some(), "the child created no ESR.Engine window");
    pump_for(SETTLE);
    let startup_cycles = cycles_of(child.handle());
    println!(
        "startup + {SETTLE:?} settle: GetProcessTimes {:.1} ms, {:.1} ms by cycles; {}",
        cpu_time_of(child.handle()).as_secs_f64() * 1000.0,
        startup_cycles as f64 / hz * 1000.0,
        memory_of(child.handle()).describe()
    );

    let without = measure_idle_cpu(&mut child, "no test window", hz);
    println!("after idle interval: {}", memory_of(child.handle()).describe());

    let folders = vec![guard.dir("cpu_a"), guard.dir("cpu_b")];
    let opened = (|| -> Result<(), String> {
        for f in &folders {
            open_window(&guard, f)?;
        }
        wait_tracked(&child, &folders)
    })();
    let with = opened.as_ref().ok().map(|()| {
        pump_for(SETTLE);
        let s = measure_idle_cpu(&mut child, "2 tracked test windows", hz);
        println!("with 2 test windows: {}", memory_of(child.handle()).describe());
        s
    });

    let exit = child.stop();
    let cleanup = guard.finish();
    drop(child);

    opened.unwrap();
    let with = with.unwrap();
    cleanup.unwrap();
    assert_eq!(exit, Some(0), "child exit code after ESR_EXIT");
    for (label, s) in [("no test window", &without), ("2 test windows", &with)] {
        assert!(
            s.percent < CPU_LIMIT_PERCENT,
            "idle CPU ({label}) {:.4} % ({:?} / {} cycles over {:?}, user input: {}) is not below {CPU_LIMIT_PERCENT} %",
            s.percent,
            s.cpu,
            s.cycles,
            s.wall,
            s.user_input
        );
    }
}

/// Req 1.6: private memory < 50 MB with 100 windows and 500 tabs (the full
/// runtime plus a synthetic 100 × 5 Live_Session in one process; see the
/// module docs for why the windows are synthetic).
#[test]
#[ignore = "runs the runtime against File Explorer; run alone with --test-threads=1"]
fn private_memory_below_50_mb_with_100_windows_and_500_tabs() {
    // No window is opened; the guard serialises against other Explorer tests,
    // whose windows the child's runtime would otherwise track.
    let guard = ExplorerGuard::new("resource_memory");
    let mut child = ChildRuntime::spawn(MODE_SYNTHETIC);
    let m = child.collect_metrics(Duration::from_secs(240));
    let exit = child.child.wait().ok().and_then(|s| s.code());
    drop(child);
    guard.finish().unwrap();

    let get = |k: &str| *m.get(k).unwrap_or_else(|| panic!("child metric {k} missing: {m:?}"));
    assert_eq!(get("done"), 1.0, "the child did not finish");
    assert_eq!(exit, Some(0), "child exit code");
    assert_eq!(get("tracked_windows") as usize, SYNTHETIC_WINDOWS);
    assert_eq!(get("tracked_tabs") as usize, SYNTHETIC_WINDOWS * SYNTHETIC_TABS_PER_WINDOW);
    assert_eq!(get("final_tabs") as usize, SYNTHETIC_WINDOWS * SYNTHETIC_TABS_PER_WINDOW);
    assert_eq!(get("file_windows") as usize, SYNTHETIC_WINDOWS, "checkpoint lists every window");
    assert_eq!(get("file_tabs") as usize, SYNTHETIC_WINDOWS * SYNTHETIC_TABS_PER_WINDOW);

    let mb = |k: &str| get(k) / MB;
    println!("--- private memory (Req 1.6, limit 50 MB) ---");
    println!("runtime baseline (adapters started):     {:.2} MB", mb("baseline_private"));
    println!("with 100 windows / 500 tabs:             {:.2} MB", mb("loaded_private"));
    println!("max over 10 navigation rounds:           {:.2} MB", mb("steady_max_private"));
    println!("synthetic Live_Session delta:            {:.2} MB", mb("loaded_private") - mb("baseline_private"));
    println!("peak private commit (whole child run):   {:.2} MB", mb("peak_private"));
    println!(
        "Location payload {:.2} MB; Session_File {:.2} MB",
        mb("location_bytes"),
        mb("file_bytes")
    );
    for k in ["baseline_private", "loaded_private", "steady_max_private"] {
        assert!(
            (get(k) as usize) < MEMORY_LIMIT_BYTES,
            "{k} = {:.2} MB is not below 50 MB",
            mb(k)
        );
    }
}

/// Req 1.6 with real windows: the marginal cost of the real adapters per
/// tracked window, from window 1 to window 9 (all guard-opened; the first
/// window's one-time costs are excluded from the marginal), extrapolated to
/// 100 windows and, as a coarse upper bound that charges every tab a whole
/// window's adapter state, to 500 single-tab windows. Only the measured
/// value is asserted; the extrapolations are reported.
#[test]
#[ignore = "runs the runtime against File Explorer and opens 9 windows; run alone with --test-threads=1"]
fn private_memory_per_real_window() {
    const N: usize = 9;
    let _sta = StaGuard::init().expect("STA init");
    let guard = ExplorerGuard::new("resource_windows");
    let mut child = ChildRuntime::spawn(MODE_RUNTIME);
    assert!(child.wait_engine(Duration::from_secs(20)).is_some(), "the child created no ESR.Engine window");
    pump_for(SETTLE);
    let before = memory_of(child.handle());
    println!("before: {}", before.describe());

    let folders: Vec<PathBuf> = (0..N).map(|i| guard.dir(&format!("mem_{i}"))).collect();
    let mut first = None;
    let opened = (|| -> Result<Memory, String> {
        open_window(&guard, &folders[0])?;
        wait_tracked(&child, &folders[..1])?;
        pump_for(Duration::from_secs(5));
        let m = memory_of(child.handle());
        println!("with 1 test window: {}", m.describe());
        first = Some(m);
        for f in &folders[1..] {
            open_window(&guard, f)?;
        }
        wait_tracked(&child, &folders)?;
        pump_for(Duration::from_secs(5));
        let m = memory_of(child.handle());
        println!("with {N} test windows: {}", m.describe());
        Ok(m)
    })();

    let exit = child.stop();
    let cleanup = guard.finish();
    drop(child);
    let after = opened.unwrap();
    cleanup.unwrap();
    let first = first.expect("measured with one window");
    assert_eq!(exit, Some(0), "child exit code after ESR_EXIT");

    let one_time = first.private as f64 - before.private as f64;
    let marginal = after.private.saturating_sub(first.private) as f64 / (N - 1) as f64;
    let at_100 = first.private as f64 + marginal * 99.0;
    let at_500 = first.private as f64 + marginal * 499.0;
    println!("--- real windows (Req 1.6, limit 50 MB) ---");
    println!("first window (incl. one-time costs):   {:+.1} KB", one_time / 1024.0);
    println!("marginal per further window:           {:.1} KB", marginal / 1024.0);
    println!("extrapolated to 100 windows:           {:.2} MB", at_100 / MB);
    println!("extrapolated to 500 single-tab windows: {:.2} MB (coarse upper bound)", at_500 / MB);
    if at_500 >= MEMORY_LIMIT_BYTES as f64 {
        println!("WARNING: the 500-window upper bound is not below 50 MB");
    }
    assert!(after.private < MEMORY_LIMIT_BYTES, "private {:.2} MB is not below 50 MB", after.private as f64 / MB);
}
