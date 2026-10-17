//! `LocationProber`: location availability probes with a 5 s deadline
//! (Requirements 8.9, 9.1, 9.2, 9.3; design.md, "Threads" → Probe pool, and
//! "Fallback (Req 9)").
//!
//! # Threads and deadline
//!
//! [`LocationProber`] owns a pool of MTA worker threads (4 by default) and a
//! supervisor thread. Every [`probe`](LocationProber::probe) gets one deadline
//! (5 s by default) counted from the call. Its result is reported through the
//! `on_result` callback **exactly once, no later than the deadline**, even when
//! the worker that runs it hangs:
//!
//! - At the deadline the supervisor reports [`ProbeOutcome::Timeout`] (or, in
//!   the ancestor phase, `Missing` with every unanswered ancestor marked not
//!   available) and calls `CancelSynchronousIo` on every worker still running
//!   an item of that probe.
//! - A worker that has not returned [`CANCEL_GRACE`] after the cancellation is
//!   abandoned (its thread is detached and its late result ignored) and
//!   replaced, so the pool keeps its capacity.
//! - A worker whose item has been running for longer than [`STALL_AFTER`] does
//!   not count as capacity: while work is queued, extra workers are started so
//!   that `threads` responsive workers are available. A few hanging network
//!   probes therefore never delay local probes until they time out. Extra
//!   workers retire when idle. The total thread count, including abandoned
//!   threads, is capped at [`MAX_THREADS`].
//!
//! Each worker enters the MTA (design.md, "Probe pool": MTA). Shell objects
//! with an apartment threading model are then hosted by COM in its host STA.
//!
//! # Probing one Location ([`check_location`])
//!
//! - **FileSystem.** The stored parsing name is normalised with
//!   [`probe_path`], made absolute with `GetFullPathNameW`, and probed in its
//!   `\\?\` / `\\?\UNC\` form (so paths longer than `MAX_PATH` work) with
//!   `GetFileAttributesExW`, then `FindFirstFileExW(<path>\*)` to check that
//!   the folder can be listed. The stored text is never changed. Relative and
//!   other unrecognised forms are `Unresolvable`. Errors are mapped by
//!   [`outcome_for_win32_error`].
//! - **A file instead of a folder** is `Missing`: the *folder* the tab showed
//!   no longer exists, so the tab is redirected to the nearest existing
//!   ancestor (normally the folder that holds the file). A `.zip` file is the
//!   exception: Explorer browses it as a folder, so it is `Available`. This is
//!   what makes the archive itself a usable ancestor of a ZIP folder.
//! - **ZipFolder.** Bound through the Shell (below) and required to be a
//!   folder. On failure the archive file (the Location itself or its deepest
//!   `.zip` ancestor) is probed as a file-system item: access denied, timeout
//!   or unresolvable are reported as such; otherwise the ZIP folder is
//!   `Missing` and its ancestors are probed (inside the archive, then the
//!   archive, then the folders above it).
//! - **Shell.** `SHParseDisplayName` on the parsing name; if that fails,
//!   `SHCreateItemFromIDList` on the stored PIDL bytes followed by
//!   `IShellItem::GetAttributes(SFGAO_VALIDATE)` (wrapping a PIDL alone
//!   validates nothing). If both fail: kinds whose availability depends on
//!   something outside the PC or on a file (portable devices, network
//!   locations, libraries, saved searches) are `ShellUnavailable`; any other
//!   kind (This PC, Home, a `::{GUID}` nobody registers, …) is
//!   `Unresolvable`. A saved search (`.search-ms` file) whose file is gone is
//!   therefore omitted, never redirected to the folder that held it.
//! - **Search.** The scope is probed with the same rules (recursively); the
//!   search is `Available` iff its scope is, otherwise `ShellUnavailable`
//!   (Req 8.9). A Search without a scope falls back to binding its parsing
//!   name through the Shell.
//!
//! # Ancestors
//!
//! A FileSystem or ZipFolder Location whose check is `Missing` has all its
//! [`ancestors`] queued at once (ahead of other work) and probed in parallel
//! under the probe's single deadline. Each is reported as available iff its
//! own check is `Available` before the deadline (exists, accessible,
//! responded). They are reported deepest first, as listed by [`ancestors`].
//!
//! # Error mapping
//!
//! `ERROR_BAD_NET_NAME` ("the network name cannot be found") is `Missing`,
//! not `Timeout`: the server answered, and said the share does not exist. A
//! server that cannot be reached fails with `ERROR_BAD_NETPATH` or one of the
//! other network-unreachable errors, which are `Timeout` (design.md,
//! "Timeout, or a network-unreachable class of error"). The share root is the
//! shallowest UNC ancestor, so a missing share ends as `Omitted(NotExist)`.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use esr_core::engine::ProbeId;
use esr_core::model::{LocationId, LocationKind, ShellKind};
use esr_core::plan::fallback::{
    AncestorProbe, ProbeOutcome, ancestors, component_count, probe_path,
};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_NET_NAME, ERROR_BAD_NET_RESP, ERROR_BAD_NETPATH,
    ERROR_BAD_PATHNAME, ERROR_CANT_ACCESS_FILE, ERROR_CONNECTION_ABORTED, ERROR_CONNECTION_REFUSED,
    ERROR_CONNECTION_UNAVAIL, ERROR_DEV_NOT_EXIST, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND,
    ERROR_HOST_UNREACHABLE, ERROR_INVALID_DRIVE, ERROR_INVALID_NAME, ERROR_LOGON_FAILURE,
    ERROR_NETNAME_DELETED, ERROR_NETWORK_ACCESS_DENIED, ERROR_NETWORK_BUSY,
    ERROR_NETWORK_UNREACHABLE, ERROR_NO_MORE_FILES, ERROR_NO_NET_OR_BAD_PATH, ERROR_NO_NETWORK,
    ERROR_NOT_CONNECTED, ERROR_NOT_READY, ERROR_OPERATION_ABORTED, ERROR_PATH_NOT_FOUND,
    ERROR_PORT_UNREACHABLE, ERROR_PROTOCOL_UNREACHABLE, ERROR_REM_NOT_LIST, ERROR_SEM_TIMEOUT,
    ERROR_TIMEOUT, ERROR_UNEXP_NET_ERR, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FIND_FIRST_EX_FLAGS, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, GetFileAttributesExW, GetFileExInfoStandard,
    GetFullPathNameW, WIN32_FILE_ATTRIBUTE_DATA, WIN32_FIND_DATAW,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
};
use windows::Win32::System::IO::CancelSynchronousIo;
use windows::Win32::System::SystemServices::{SFGAO_FLAGS, SFGAO_FOLDER, SFGAO_VALIDATE};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromIDList, SHParseDisplayName};
use windows::core::PCWSTR;

/// Pool size from design.md ("Probe pool": MTA, 4 threads).
pub const DEFAULT_THREADS: usize = 4;
/// Probe deadline (Req 9.3).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);
/// A worker busy for longer than this no longer counts as pool capacity.
pub const STALL_AFTER: Duration = Duration::from_millis(500);
/// How long a cancelled worker has to return before it is abandoned.
pub const CANCEL_GRACE: Duration = Duration::from_millis(250);
/// Most threads alive at once, including abandoned ones.
pub const MAX_THREADS: usize = 64;
/// An extra (above `threads`) worker retires after being idle this long.
const IDLE_RETIRE: Duration = Duration::from_secs(2);
/// How long `Drop` waits for workers to finish before detaching them.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(500);
/// Nesting limit for search scopes.
const MAX_SEARCH_DEPTH: usize = 8;

/// Receives each probe's result, on a pool or supervisor thread.
pub type ResultCallback = Box<dyn Fn(ProbeId, ProbeOutcome) + Send + Sync>;

/// Checks one Location without ancestors; `Missing { ancestors: [] }` means
/// "does not exist". [`check_location`] is the real one; tests inject others.
pub type CheckFn = Arc<dyn Fn(&LocationId) -> ProbeOutcome + Send + Sync>;

/// Location availability probes on a deadline-bounded thread pool.
pub struct LocationProber {
    shared: Arc<Shared>,
    supervisor: Option<JoinHandle<()>>,
}

impl LocationProber {
    /// Starts `threads` workers (at least 1) probing with [`check_location`].
    pub fn new(threads: usize, deadline: Duration, on_result: ResultCallback) -> Self {
        Self::with_check(threads, deadline, on_result, Arc::new(check_location))
    }

    /// Like [`new`](Self::new) with a custom single-Location check.
    pub fn with_check(
        threads: usize,
        deadline: Duration,
        on_result: ResultCallback,
        check: CheckFn,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            on_result,
            check,
            threads: threads.max(1),
            deadline,
            closed: AtomicBool::new(false),
        });
        {
            let mut st = shared.lock();
            for _ in 0..shared.threads {
                spawn_worker(&shared, &mut st);
            }
        }
        let s = Arc::clone(&shared);
        let supervisor = thread::Builder::new()
            .name("esr-probe-supervisor".into())
            .spawn(move || supervisor_main(&s))
            .ok();
        Self { shared, supervisor }
    }

    /// Starts a probe. `on_result(probe, outcome)` is called exactly once, at
    /// the latest when the deadline expires.
    pub fn probe(&self, probe: ProbeId, location: LocationId) {
        let mut st = self.shared.lock();
        if st.shutdown {
            return;
        }
        let key = st.next_job;
        st.next_job += 1;
        st.queue.push_back(Item {
            job: key,
            slot: Slot::Main,
            location: location.clone(),
        });
        st.jobs.insert(
            key,
            Job {
                probe,
                location,
                deadline: Instant::now() + self.shared.deadline,
                ancestors: None,
                pending: 0,
            },
        );
        self.shared.cv.notify_all();
    }

    /// `(live workers, abandoned threads still running)`.
    #[cfg(test)]
    fn worker_counts(&self) -> (usize, usize) {
        let st = self.shared.lock();
        (st.live(), st.abandoned)
    }
}

impl Drop for LocationProber {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        {
            let mut st = self.shared.lock();
            st.shutdown = true;
            st.queue.clear();
            st.jobs.clear();
            for w in st.workers.values().filter(|w| w.current.is_some()) {
                cancel_io(&w.handle);
            }
            self.shared.cv.notify_all();
        }
        if let Some(h) = self.supervisor.take() {
            let _ = h.join();
        }
        let until = Instant::now() + SHUTDOWN_WAIT;
        let mut st = self.shared.lock();
        loop {
            let now = Instant::now();
            if st.workers.values().all(|w| w.exited) || now >= until {
                break;
            }
            st = self.shared.wait_for(st, until - now);
        }
        // Workers still running are detached; they find their record gone and exit.
        let handles: Vec<JoinHandle<()>> = st
            .workers
            .drain()
            .filter(|(_, w)| w.exited)
            .filter_map(|(_, mut w)| w.handle.take())
            .collect();
        drop(st);
        for h in handles {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Pool state
// ---------------------------------------------------------------------------

struct Shared {
    state: Mutex<State>,
    /// Signalled on every state change; workers, the supervisor and `Drop` wait on it.
    cv: Condvar,
    on_result: ResultCallback,
    check: CheckFn,
    threads: usize,
    deadline: Duration,
    /// Set by `Drop`: no further callbacks.
    closed: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, g: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.cv.wait(g).unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_for<'a>(&self, g: MutexGuard<'a, State>, d: Duration) -> MutexGuard<'a, State> {
        self.cv
            .wait_timeout(g, d)
            .map(|(g, _)| g)
            .unwrap_or_else(|e| e.into_inner().0)
    }

    /// Delivers completions; call without holding the lock.
    fn fire(&self, done: Vec<(ProbeId, ProbeOutcome)>) {
        for (probe, outcome) in done {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            (self.on_result)(probe, outcome);
        }
    }
}

#[derive(Default)]
struct State {
    shutdown: bool,
    next_job: u64,
    next_worker: u64,
    jobs: HashMap<u64, Job>,
    queue: VecDeque<Item>,
    workers: HashMap<u64, WorkerRec>,
    /// Abandoned threads that have not exited yet.
    abandoned: usize,
}

struct Job {
    probe: ProbeId,
    location: LocationId,
    deadline: Instant,
    /// `Some` once the Location was found missing: each ancestor with its
    /// result (`None` while unanswered).
    ancestors: Option<Vec<(LocationId, Option<bool>)>>,
    pending: usize,
}

#[derive(Clone, Copy, Debug)]
enum Slot {
    Main,
    Ancestor(usize),
}

struct Item {
    job: u64,
    slot: Slot,
    location: LocationId,
}

struct WorkerRec {
    handle: Option<JoinHandle<()>>,
    current: Option<Running>,
    exited: bool,
}

struct Running {
    job: u64,
    started: Instant,
    cancelled_at: Option<Instant>,
}

type Completion = (ProbeId, ProbeOutcome);

impl State {
    fn live(&self) -> usize {
        self.workers.values().filter(|w| !w.exited).count()
    }

    fn idle(&self) -> usize {
        self.workers
            .values()
            .filter(|w| !w.exited && w.current.is_none())
            .count()
    }

    /// Live workers that are idle or have been busy for less than [`STALL_AFTER`].
    fn responsive(&self, now: Instant) -> usize {
        self.workers
            .values()
            .filter(|w| {
                !w.exited
                    && w.current
                        .as_ref()
                        .is_none_or(|r| now.duration_since(r.started) < STALL_AFTER)
            })
            .count()
    }

    fn finish(&mut self, key: u64, outcome: ProbeOutcome, done: &mut Vec<Completion>) {
        if let Some(job) = self.jobs.remove(&key) {
            self.queue.retain(|i| i.job != key);
            done.push((job.probe, outcome));
        }
    }

    /// Next queued item whose probe is still within its deadline.
    fn pop_runnable(&mut self, now: Instant) -> Option<Item> {
        while let Some(item) = self.queue.pop_front() {
            if self.jobs.get(&item.job).is_some_and(|j| now < j.deadline) {
                return Some(item);
            }
        }
        None
    }

    /// Records the result of one item.
    fn apply(&mut self, item: Item, outcome: ProbeOutcome, done: &mut Vec<Completion>) {
        let Some(job) = self.jobs.get_mut(&item.job) else {
            return;
        };
        match item.slot {
            Slot::Main => {
                let fs_like = matches!(
                    job.location.kind,
                    LocationKind::FileSystem(_) | LocationKind::ZipFolder
                );
                if fs_like && matches!(outcome, ProbeOutcome::Missing { .. }) {
                    let anc = ancestors(&job.location);
                    if !anc.is_empty() {
                        job.pending = anc.len();
                        // Ahead of other work, deepest first.
                        for (i, a) in anc.iter().enumerate().rev() {
                            self.queue.push_front(Item {
                                job: item.job,
                                slot: Slot::Ancestor(i),
                                location: a.clone(),
                            });
                        }
                        job.ancestors = Some(anc.into_iter().map(|a| (a, None)).collect());
                        return;
                    }
                    self.finish(
                        item.job,
                        ProbeOutcome::Missing {
                            ancestors: Vec::new(),
                        },
                        done,
                    );
                } else {
                    self.finish(item.job, outcome, done);
                }
            }
            Slot::Ancestor(i) => {
                if let Some(entry) = job.ancestors.as_mut().and_then(|l| l.get_mut(i))
                    && entry.1.is_none()
                {
                    entry.1 = Some(outcome == ProbeOutcome::Available);
                    job.pending -= 1;
                }
                if job.pending == 0 {
                    let list = job.ancestors.take().unwrap_or_default();
                    self.finish(item.job, missing_from(list), done);
                }
            }
        }
    }
}

fn missing_from(list: Vec<(LocationId, Option<bool>)>) -> ProbeOutcome {
    ProbeOutcome::Missing {
        ancestors: list
            .into_iter()
            .map(|(location, available)| AncestorProbe {
                location,
                available: available.unwrap_or(false),
            })
            .collect(),
    }
}

fn cancel_io(handle: &Option<JoinHandle<()>>) {
    if let Some(h) = handle {
        // SAFETY: the JoinHandle keeps the thread handle open; cancelling
        // synchronous I/O on a live or finished thread is harmless.
        let _ = unsafe { CancelSynchronousIo(HANDLE(h.as_raw_handle())) };
    }
}

fn spawn_worker(shared: &Arc<Shared>, st: &mut State) -> bool {
    let id = st.next_worker;
    st.next_worker += 1;
    let s = Arc::clone(shared);
    // The new thread blocks on the state lock (held by the caller) until its
    // record has been inserted.
    match thread::Builder::new()
        .name(format!("esr-probe-{id}"))
        .spawn(move || worker_main(&s, id))
    {
        Ok(h) => {
            st.workers.insert(
                id,
                WorkerRec {
                    handle: Some(h),
                    current: None,
                    exited: false,
                },
            );
            true
        }
        Err(_) => false,
    }
}

/// Keeps `threads` live workers, and `threads` responsive ones while work is queued.
fn ensure_capacity(shared: &Arc<Shared>, st: &mut State, now: Instant) {
    while !st.shutdown && st.live() + st.abandoned < MAX_THREADS {
        let need = st.live() < shared.threads
            || (!st.queue.is_empty()
                && st.responsive(now) < shared.threads
                && st.idle() < st.queue.len());
        if !need || !spawn_worker(shared, st) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

/// MTA membership for the current thread.
struct MtaGuard(bool);

impl MtaGuard {
    fn init() -> Self {
        // SAFETY: no reserved pointer; valid flags.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(hr.is_ok())
    }
}

impl Drop for MtaGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

fn worker_main(shared: &Arc<Shared>, id: u64) {
    let _com = MtaGuard::init();
    worker_loop(shared, id);
}

fn worker_loop(shared: &Arc<Shared>, id: u64) {
    let mut st = shared.lock();
    loop {
        // Wait for work, or exit.
        let mut idle_expired = false;
        let item = loop {
            if !st.workers.contains_key(&id) {
                st.abandoned = st.abandoned.saturating_sub(1);
                return;
            }
            if st.shutdown {
                return mark_exited(shared, st, id);
            }
            let now = Instant::now();
            if let Some(item) = st.pop_runnable(now) {
                break item;
            }
            if st.live() > shared.threads {
                if idle_expired {
                    return mark_exited(shared, st, id);
                }
                let before = Instant::now();
                st = shared.wait_for(st, IDLE_RETIRE);
                idle_expired = before.elapsed() >= IDLE_RETIRE;
            } else {
                st = shared.wait(st);
            }
        };

        let now = Instant::now();
        if let Some(rec) = st.workers.get_mut(&id) {
            rec.current = Some(Running {
                job: item.job,
                started: now,
                cancelled_at: None,
            });
        }
        drop(st);
        let check = Arc::clone(&shared.check);
        let outcome = catch_unwind(AssertUnwindSafe(|| check(&item.location)))
            .unwrap_or(ProbeOutcome::Unresolvable);
        st = shared.lock();

        let Some(rec) = st.workers.get_mut(&id) else {
            // Abandoned after a timeout: the probe has been reported already.
            st.abandoned = st.abandoned.saturating_sub(1);
            return;
        };
        rec.current = None;
        let mut done = Vec::new();
        st.apply(item, outcome, &mut done);
        let retire = st.live() > shared.threads && st.queue.is_empty();
        shared.cv.notify_all();
        if !done.is_empty() {
            drop(st);
            shared.fire(done);
            st = shared.lock();
        }
        if retire && st.workers.contains_key(&id) {
            return mark_exited(shared, st, id);
        }
    }
}

fn mark_exited(shared: &Shared, mut st: MutexGuard<'_, State>, id: u64) {
    if let Some(rec) = st.workers.get_mut(&id) {
        rec.exited = true;
    }
    shared.cv.notify_all();
}

fn supervisor_main(shared: &Arc<Shared>) {
    let mut st = shared.lock();
    while !st.shutdown {
        let now = Instant::now();
        let mut done = Vec::new();

        // Expired probes: report now, cancel their running items.
        let expired: Vec<u64> = st
            .jobs
            .iter()
            .filter(|(_, j)| j.deadline <= now)
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            let Some(job) = st.jobs.remove(&key) else {
                continue;
            };
            st.queue.retain(|i| i.job != key);
            let outcome = match job.ancestors {
                Some(list) => missing_from(list),
                None => ProbeOutcome::Timeout,
            };
            done.push((job.probe, outcome));
            for w in st.workers.values_mut() {
                if let Some(r) = w.current.as_mut()
                    && r.job == key
                    && r.cancelled_at.is_none()
                {
                    cancel_io(&w.handle);
                    r.cancelled_at = Some(now);
                }
            }
        }

        // Cancellation did not take effect: abandon (detach) and replace.
        let stuck: Vec<u64> = st
            .workers
            .iter()
            .filter(|(_, w)| {
                !w.exited
                    && w.current
                        .as_ref()
                        .and_then(|r| r.cancelled_at)
                        .is_some_and(|c| now >= c + CANCEL_GRACE)
            })
            .map(|(k, _)| *k)
            .collect();
        for id in stuck {
            st.workers.remove(&id);
            st.abandoned += 1;
        }

        // Reap retired workers.
        let exited: Vec<u64> = st
            .workers
            .iter()
            .filter(|(_, w)| w.exited)
            .map(|(k, _)| *k)
            .collect();
        let joins: Vec<JoinHandle<()>> = exited
            .iter()
            .filter_map(|id| st.workers.remove(id).and_then(|mut w| w.handle.take()))
            .collect();

        ensure_capacity(shared, &mut st, now);

        if !done.is_empty() || !joins.is_empty() {
            drop(st);
            shared.fire(done);
            for h in joins {
                let _ = h.join();
            }
            st = shared.lock();
            continue;
        }

        st = match next_wake(&st, now) {
            Some(t) => shared.wait_for(st, t.saturating_duration_since(now)),
            None => shared.wait(st),
        };
    }
}

/// Earliest time the supervisor has something to do.
fn next_wake(st: &State, now: Instant) -> Option<Instant> {
    let deadlines = st.jobs.values().map(|j| j.deadline);
    let workers = st.workers.values().filter_map(|w| {
        let r = w.current.as_ref()?;
        match r.cancelled_at {
            Some(c) => Some(c + CANCEL_GRACE),
            None if !st.queue.is_empty() && now.duration_since(r.started) < STALL_AFTER => {
                Some(r.started + STALL_AFTER)
            }
            None => None,
        }
    });
    deadlines.chain(workers).min()
}

// ---------------------------------------------------------------------------
// Probing one Location
// ---------------------------------------------------------------------------

const BACKSLASH: u16 = b'\\' as u16;
/// `\\?\`
const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];
/// `\\.\`
const DEVICE: [u16; 4] = [BACKSLASH, BACKSLASH, b'.' as u16, BACKSLASH];

fn missing() -> ProbeOutcome {
    ProbeOutcome::Missing {
        ancestors: Vec::new(),
    }
}

/// Checks one Location synchronously, without probing ancestors (`Missing`
/// carries no ancestors). Must be called on a COM-initialised thread for
/// Shell, ZIP and search Locations.
pub fn check_location(loc: &LocationId) -> ProbeOutcome {
    check_at_depth(loc, 0)
}

fn check_at_depth(loc: &LocationId, depth: usize) -> ProbeOutcome {
    match loc.kind {
        LocationKind::FileSystem(_) => check_file_system(loc.parsing_name.units()),
        LocationKind::ZipFolder => check_zip_folder(loc),
        LocationKind::Shell(kind) => match shell_bind(loc.parsing_name.units(), &loc.pidl) {
            Some(_) => ProbeOutcome::Available,
            None => shell_failure(kind),
        },
        LocationKind::Search => match &loc.search {
            Some(spec) if depth < MAX_SEARCH_DEPTH => {
                match check_at_depth(&spec.scope, depth + 1) {
                    ProbeOutcome::Available => ProbeOutcome::Available,
                    _ => ProbeOutcome::ShellUnavailable,
                }
            }
            Some(_) => ProbeOutcome::Unresolvable,
            None => match shell_bind(loc.parsing_name.units(), &loc.pidl) {
                Some(_) => ProbeOutcome::Available,
                None => ProbeOutcome::Unresolvable,
            },
        },
    }
}

/// Outcome when neither the parsing name nor the PIDL of a Shell Location binds.
fn shell_failure(kind: ShellKind) -> ProbeOutcome {
    match kind {
        // Exist in principle, but depend on a device, the network, or a
        // library or saved-search file that can go away (a missing saved
        // search is omitted, never redirected).
        ShellKind::PortableDevice
        | ShellKind::Network
        | ShellKind::Library
        | ShellKind::SavedSearch => ProbeOutcome::ShellUnavailable,
        _ => ProbeOutcome::Unresolvable,
    }
}

fn check_zip_folder(loc: &LocationId) -> ProbeOutcome {
    if shell_bind(loc.parsing_name.units(), &loc.pidl).is_some_and(|a| a & SFGAO_FOLDER.0 != 0) {
        return ProbeOutcome::Available;
    }
    let archive = if is_zip_name(loc.parsing_name.units()) {
        Some(loc.parsing_name.units().to_vec())
    } else {
        ancestors(loc)
            .into_iter()
            .find(|a| {
                matches!(a.kind, LocationKind::FileSystem(_)) && is_zip_name(a.parsing_name.units())
            })
            .map(|a| a.parsing_name.0)
    };
    match archive.map(|a| check_file_system(&a)) {
        Some(
            o @ (ProbeOutcome::AccessDenied | ProbeOutcome::Timeout | ProbeOutcome::Unresolvable),
        ) => o,
        _ => missing(),
    }
}

/// Ends in `.zip` (ASCII case-insensitive), ignoring trailing separators.
fn is_zip_name(path: &[u16]) -> bool {
    let mut end = path.len();
    while end > 0 && path[end - 1] == BACKSLASH {
        end -= 1;
    }
    let p = &path[..end];
    p.len() >= 4
        && p[p.len() - 4..]
            .iter()
            .zip(b".zip")
            .all(|(&u, &b)| u8::try_from(u).is_ok_and(|c| c.eq_ignore_ascii_case(&b)))
}

/// Probes a file-system folder (see the module docs).
fn check_file_system(stored: &[u16]) -> ProbeOutcome {
    let Some(path) = fs_probe_form(stored) else {
        return ProbeOutcome::Unresolvable;
    };
    let mut z = path.clone();
    z.push(0);
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    // SAFETY: `z` is NUL-terminated; `data` is the out buffer for GetFileExInfoStandard.
    if let Err(e) = unsafe {
        GetFileAttributesExW(
            PCWSTR(z.as_ptr()),
            GetFileExInfoStandard,
            (&raw mut data).cast::<c_void>(),
        )
    } {
        return outcome_for_error(&e);
    }
    if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return if is_zip_name(stored) {
            ProbeOutcome::Available
        } else {
            missing()
        };
    }

    // The folder must also be listable by the current user.
    let mut pattern = path;
    if pattern.last() != Some(&BACKSLASH) {
        pattern.push(BACKSLASH);
    }
    pattern.extend([u16::from(b'*'), 0]);
    let mut find = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated; `find` is the out buffer for FindExInfoBasic.
    let r = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&raw mut find).cast::<c_void>(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_FLAGS(0),
        )
    };
    match r {
        Ok(h) => {
            // SAFETY: `h` is a valid search handle from FindFirstFileExW.
            let _ = unsafe { FindClose(h) };
            ProbeOutcome::Available
        }
        // An empty folder (a root has no `.` / `..` entries).
        Err(e) if matches!(win32_code(&e), Some(c) if c == ERROR_FILE_NOT_FOUND.0 || c == ERROR_NO_MORE_FILES.0) => {
            ProbeOutcome::Available
        }
        Err(e) => outcome_for_error(&e),
    }
}

/// The `\\?\` / `\\?\UNC\` form handed to the file-system APIs, or `None` for
/// a relative or otherwise unrecognised path.
fn fs_probe_form(stored: &[u16]) -> Option<Vec<u16>> {
    let p = probe_path(stored);
    // Kept verbatim by probe_path (dropping the prefix would change its meaning).
    if p.starts_with(&VERBATIM) {
        return Some(p);
    }
    component_count(&p)?;
    let full = full_path_name(&p).unwrap_or(p);
    if full.starts_with(&VERBATIM) || full.starts_with(&DEVICE) {
        return Some(full);
    }
    let mut out = VERBATIM.to_vec();
    if full.len() >= 2 && full[0] == BACKSLASH && full[1] == BACKSLASH {
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(&full[2..]);
    } else {
        out.extend_from_slice(&full);
    }
    Some(out)
}

fn full_path_name(p: &[u16]) -> Option<Vec<u16>> {
    let mut z = p.to_vec();
    z.push(0);
    let mut buf = vec![0u16; 32_768 + 64];
    // SAFETY: `z` is NUL-terminated and `buf` is a writable buffer.
    let n = unsafe { GetFullPathNameW(PCWSTR(z.as_ptr()), Some(&mut buf), None) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    buf.truncate(n);
    Some(buf)
}

/// Binds through the Shell: `SHParseDisplayName`, else the stored PIDL with
/// `SFGAO_VALIDATE`. Returns the item's attributes (at least `SFGAO_FOLDER`).
fn shell_bind(parsing_name: &[u16], pidl: &[u8]) -> Option<u32> {
    if !parsing_name.is_empty() {
        let mut z = parsing_name.to_vec();
        z.push(0);
        let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut attrs = 0u32;
        // SAFETY: `z` is NUL-terminated; `out` and `attrs` are valid out pointers.
        let r = unsafe {
            SHParseDisplayName(
                PCWSTR(z.as_ptr()),
                None::<&IBindCtx>,
                &mut out,
                SFGAO_FOLDER.0,
                Some(&mut attrs),
            )
        };
        if !out.is_null() {
            // SAFETY: the PIDL was allocated by SHParseDisplayName with the COM allocator.
            unsafe { CoTaskMemFree(Some(out.cast_const().cast::<c_void>())) };
        }
        if r.is_ok() {
            return Some(attrs);
        }
    }
    if pidl_is_well_formed(pidl) {
        // SAFETY: `pidl` is a terminated ITEMIDLIST (checked above); ITEMIDLIST is packed(1).
        let item: windows::core::Result<IShellItem> =
            unsafe { SHCreateItemFromIDList(pidl.as_ptr().cast()) };
        if let Ok(item) = item {
            // SAFETY: plain COM call on a live interface.
            let attrs =
                unsafe { item.GetAttributes(SFGAO_FLAGS(SFGAO_VALIDATE.0 | SFGAO_FOLDER.0)) };
            if let Ok(a) = attrs {
                return Some(a.0);
            }
        }
    }
    None
}

/// At least one SHITEMID, every `cb` in bounds, and a 2-byte zero terminator.
/// The desktop PIDL (terminator only) is rejected: it would "validate" anything.
fn pidl_is_well_formed(bytes: &[u8]) -> bool {
    let mut at = 0usize;
    let mut items = 0usize;
    loop {
        let Some(cb) = bytes.get(at..at + 2) else {
            return false;
        };
        let cb = usize::from(u16::from_le_bytes([cb[0], cb[1]]));
        if cb == 0 {
            return items > 0;
        }
        if cb < 2 || at + cb > bytes.len() {
            return false;
        }
        at += cb;
        items += 1;
    }
}

fn win32_code(e: &windows::core::Error) -> Option<u32> {
    let hr = e.code().0 as u32;
    (hr & 0xFFFF_0000 == 0x8007_0000).then_some(hr & 0xFFFF)
}

fn outcome_for_error(e: &windows::core::Error) -> ProbeOutcome {
    win32_code(e).map_or(ProbeOutcome::Unresolvable, outcome_for_win32_error)
}

/// Maps a Win32 error from a file-system probe to its outcome (see the module
/// docs for `ERROR_BAD_NET_NAME`). Unknown errors are `Unresolvable`.
pub fn outcome_for_win32_error(code: u32) -> ProbeOutcome {
    const MISSING: &[u32] = &[
        ERROR_FILE_NOT_FOUND.0,
        ERROR_PATH_NOT_FOUND.0,
        ERROR_INVALID_DRIVE.0,
        ERROR_NOT_READY.0,
        ERROR_BAD_NET_NAME.0,
        ERROR_INVALID_NAME.0,
        ERROR_BAD_PATHNAME.0,
        ERROR_DIRECTORY.0,
    ];
    const DENIED: &[u32] = &[
        ERROR_ACCESS_DENIED.0,
        ERROR_NETWORK_ACCESS_DENIED.0,
        ERROR_LOGON_FAILURE.0,
        ERROR_CANT_ACCESS_FILE.0,
    ];
    const NETWORK: &[u32] = &[
        ERROR_REM_NOT_LIST.0,
        ERROR_BAD_NETPATH.0,
        ERROR_NETWORK_BUSY.0,
        ERROR_DEV_NOT_EXIST.0,
        ERROR_BAD_NET_RESP.0,
        ERROR_UNEXP_NET_ERR.0,
        ERROR_NETNAME_DELETED.0,
        ERROR_SEM_TIMEOUT.0,
        ERROR_OPERATION_ABORTED.0,
        ERROR_CONNECTION_UNAVAIL.0,
        ERROR_NO_NET_OR_BAD_PATH.0,
        ERROR_NO_NETWORK.0,
        ERROR_CONNECTION_REFUSED.0,
        ERROR_NETWORK_UNREACHABLE.0,
        ERROR_HOST_UNREACHABLE.0,
        ERROR_PROTOCOL_UNREACHABLE.0,
        ERROR_PORT_UNREACHABLE.0,
        ERROR_CONNECTION_ABORTED.0,
        ERROR_TIMEOUT.0,
        ERROR_NOT_CONNECTED.0,
    ];
    if MISSING.contains(&code) {
        missing()
    } else if DENIED.contains(&code) {
        ProbeOutcome::AccessDenied
    } else if NETWORK.contains(&code) {
        ProbeOutcome::Timeout
    } else {
        ProbeOutcome::Unresolvable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_core::model::{SearchSpec, Volume, WideString};
    use esr_core::plan::fallback::{Fallback, choose_fallback};
    use esr_core::search::build_search_ms;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
    const HOME: &str = "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}";
    const GARBAGE: &str = "::{0BADF00D-1234-5678-9ABC-DEF012345678}";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir()
                .join(format!("esr-prober-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn loc(kind: LocationKind, name: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: name.into(),
            display_name: name.into(),
            pidl: Vec::new(),
            search: None,
        }
    }

    fn fs_loc(p: &Path) -> LocationId {
        let mut l = loc(LocationKind::FileSystem(Volume::Fixed), "");
        l.parsing_name = WideString::new(p.as_os_str().encode_wide().collect());
        l
    }

    fn search(scope: LocationId) -> LocationId {
        let query: WideString = "needle".into();
        let mut l = loc(LocationKind::Search, "");
        l.parsing_name = build_search_ms(&query, &scope.parsing_name).unwrap();
        l.search = Some(SearchSpec {
            query,
            scope: Box::new(scope),
        });
        l
    }

    type Rx = mpsc::Receiver<(ProbeId, ProbeOutcome, Instant)>;

    fn prober(threads: usize, deadline: Duration, check: Option<CheckFn>) -> (LocationProber, Rx) {
        let (tx, rx) = mpsc::channel();
        let cb: ResultCallback = Box::new(move |id, o| {
            let _ = tx.send((id, o, Instant::now()));
        });
        let p = match check {
            Some(c) => LocationProber::with_check(threads, deadline, cb, c),
            None => LocationProber::new(threads, deadline, cb),
        };
        (p, rx)
    }

    /// Probes each Location with the real check and returns outcomes in input order.
    fn probe_all(locs: &[LocationId]) -> Vec<ProbeOutcome> {
        let (p, rx) = prober(DEFAULT_THREADS, DEFAULT_DEADLINE, None);
        for (i, l) in locs.iter().enumerate() {
            p.probe(ProbeId(i as u64), l.clone());
        }
        let mut out = vec![None; locs.len()];
        for _ in locs {
            let (id, o, _) = rx
                .recv_timeout(DEFAULT_DEADLINE + Duration::from_secs(2))
                .expect("probe result");
            assert!(out[id.0 as usize].replace(o).is_none(), "reported twice");
        }
        out.into_iter().map(Option::unwrap).collect()
    }

    #[test]
    fn existing_folder_is_available() {
        let t = TempDir::new("avail");
        let long =
            t.0.join("x".repeat(120))
                .join("y".repeat(120))
                .join("z".repeat(60));
        std::fs::create_dir_all(&long).unwrap();
        assert!(long.as_os_str().len() > 260);
        let verbatim = PathBuf::from(format!(r"\\?\{}", t.0.display()));
        let got = probe_all(&[
            fs_loc(&t.0),
            fs_loc(&long),
            fs_loc(&verbatim),
            fs_loc(Path::new(r"C:\")),
        ]);
        assert_eq!(got, vec![ProbeOutcome::Available; 4]);
    }

    #[test]
    fn missing_nested_path_reports_ancestors() {
        let t = TempDir::new("missing");
        let l = fs_loc(&t.0.join("gone").join("deeper"));
        let [got] = probe_all(std::slice::from_ref(&l)).try_into().unwrap();
        let ProbeOutcome::Missing { ancestors: anc } = &got else {
            panic!("{got:?}")
        };
        let expected = ancestors(&l);
        assert_eq!(
            anc.iter().map(|a| a.location.clone()).collect::<Vec<_>>(),
            expected
        );
        assert!(!anc[0].available, "{anc:?}");
        assert!(anc[1].available, "{anc:?}");
        assert!(anc.last().unwrap().available, "drive root: {anc:?}");
        assert_eq!(
            choose_fallback(&l, &got),
            Fallback::Redirected(fs_loc(&t.0))
        );
    }

    #[test]
    fn file_instead_of_folder_is_missing() {
        let t = TempDir::new("file");
        let file = t.0.join("note.txt");
        std::fs::write(&file, b"x").unwrap();
        let l = fs_loc(&file);
        let [got] = probe_all(std::slice::from_ref(&l)).try_into().unwrap();
        assert!(
            matches!(&got, ProbeOutcome::Missing { ancestors } if ancestors[0].available),
            "{got:?}"
        );
        assert_eq!(
            choose_fallback(&l, &got),
            Fallback::Redirected(fs_loc(&t.0))
        );
    }

    #[test]
    fn zip_folder_in_missing_archive_is_missing() {
        let t = TempDir::new("zip");
        let mut l = fs_loc(&t.0.join("nope.zip").join("inner"));
        l.kind = LocationKind::ZipFolder;
        let [got] = probe_all(std::slice::from_ref(&l)).try_into().unwrap();
        let ProbeOutcome::Missing { ancestors: anc } = &got else {
            panic!("{got:?}")
        };
        assert!(!anc[0].available, "archive: {anc:?}");
        assert!(anc[1].available, "{anc:?}");
        assert_eq!(
            choose_fallback(&l, &got),
            Fallback::Redirected(fs_loc(&t.0))
        );
    }

    #[test]
    fn missing_saved_search_is_omitted_not_redirected() {
        let t = TempDir::new("saved");
        let present = t.0.join("present.search-ms");
        std::fs::write(&present, SAVED_SEARCH_XML).unwrap();
        let mut gone = fs_loc(&t.0.join("gone.search-ms"));
        gone.kind = LocationKind::Shell(ShellKind::SavedSearch);
        let mut here = fs_loc(&present);
        here.kind = LocationKind::Shell(ShellKind::SavedSearch);
        let got = probe_all(&[gone.clone(), here]);
        assert_eq!(got[0], ProbeOutcome::ShellUnavailable);
        assert_eq!(got[1], ProbeOutcome::Available);
        assert_eq!(
            choose_fallback(&gone, &got[0]),
            Fallback::Omitted(esr_core::plan::fallback::OmitReason::ShellUnavailable)
        );
    }

    const SAVED_SEARCH_XML: &str = r#"<?xml version="1.0"?>
<persistedQuery version="1.0"><query><conditions><condition type="leafCondition" property="System.Generic.String" operator="wordmatch" propertyType="string" value="x"/></conditions><scope><include knownFolder="{FDD39AD0-238F-46AF-ADB4-6C85480369C7}"/></scope></query></persistedQuery>
"#;

    #[test]
    fn shell_locations() {
        let got = probe_all(&[
            loc(LocationKind::Shell(ShellKind::ThisPc), THIS_PC),
            loc(LocationKind::Shell(ShellKind::Home), HOME),
            loc(LocationKind::Shell(ShellKind::Other), GARBAGE),
            loc(
                LocationKind::Shell(ShellKind::PortableDevice),
                &format!(
                    r"{THIS_PC}\\\?\usb#vid_0000&pid_0000#esr#{{6ac27878-a6fa-4155-ba85-f98f491d4f33}}"
                ),
            ),
        ]);
        assert_eq!(
            got,
            [
                ProbeOutcome::Available,
                ProbeOutcome::Available,
                ProbeOutcome::Unresolvable,
                ProbeOutcome::ShellUnavailable
            ]
        );
    }

    #[test]
    fn search_follows_its_scope() {
        let t = TempDir::new("search");
        let got = probe_all(&[
            search(fs_loc(&t.0.join("gone"))),
            search(fs_loc(&t.0)),
            search(loc(LocationKind::Shell(ShellKind::ThisPc), THIS_PC)),
        ]);
        assert_eq!(
            got,
            [
                ProbeOutcome::ShellUnavailable,
                ProbeOutcome::Available,
                ProbeOutcome::Available
            ]
        );
    }

    #[test]
    fn error_mapping() {
        // The server answered "no such share": Missing, not Timeout (Req 9.3).
        assert_eq!(outcome_for_win32_error(ERROR_BAD_NET_NAME.0), missing());
        for c in [
            ERROR_FILE_NOT_FOUND,
            ERROR_PATH_NOT_FOUND,
            ERROR_INVALID_DRIVE,
            ERROR_NOT_READY,
            ERROR_INVALID_NAME,
            ERROR_BAD_PATHNAME,
            ERROR_DIRECTORY,
        ] {
            assert_eq!(outcome_for_win32_error(c.0), missing(), "{c:?}");
        }
        for c in [
            ERROR_ACCESS_DENIED,
            ERROR_NETWORK_ACCESS_DENIED,
            ERROR_LOGON_FAILURE,
            ERROR_CANT_ACCESS_FILE,
        ] {
            assert_eq!(
                outcome_for_win32_error(c.0),
                ProbeOutcome::AccessDenied,
                "{c:?}"
            );
        }
        // An unreachable server (or a cancelled call) is Timeout.
        for c in [
            ERROR_BAD_NETPATH,
            ERROR_REM_NOT_LIST,
            ERROR_NETWORK_BUSY,
            ERROR_DEV_NOT_EXIST,
            ERROR_BAD_NET_RESP,
            ERROR_UNEXP_NET_ERR,
            ERROR_NETNAME_DELETED,
            ERROR_CONNECTION_UNAVAIL,
            ERROR_CONNECTION_REFUSED,
            ERROR_CONNECTION_ABORTED,
            ERROR_NO_NETWORK,
            ERROR_NOT_CONNECTED,
            ERROR_NETWORK_UNREACHABLE,
            ERROR_HOST_UNREACHABLE,
            ERROR_PROTOCOL_UNREACHABLE,
            ERROR_PORT_UNREACHABLE,
            ERROR_NO_NET_OR_BAD_PATH,
            ERROR_OPERATION_ABORTED,
            ERROR_SEM_TIMEOUT,
            ERROR_TIMEOUT,
        ] {
            assert_eq!(outcome_for_win32_error(c.0), ProbeOutcome::Timeout, "{c:?}");
        }
        assert_eq!(outcome_for_win32_error(31), ProbeOutcome::Unresolvable);
        assert_eq!(outcome_for_win32_error(0), ProbeOutcome::Unresolvable);
        // HRESULTs: Win32 errors are unwrapped; anything else is Unresolvable.
        let from_win32 = |c: u32| windows::core::Error::from(windows::core::HRESULT::from_win32(c));
        assert_eq!(
            outcome_for_error(&from_win32(ERROR_BAD_NET_NAME.0)),
            missing()
        );
        assert_eq!(
            outcome_for_error(&from_win32(ERROR_BAD_NETPATH.0)),
            ProbeOutcome::Timeout
        );
        assert_eq!(
            outcome_for_error(&windows::Win32::Foundation::E_FAIL.into()),
            ProbeOutcome::Unresolvable
        );
        assert_eq!(
            check_location(&fs_loc(Path::new(r"relative\path"))),
            ProbeOutcome::Unresolvable
        );
    }

    #[test]
    fn file_in_place_of_folder_is_missing_except_zip() {
        let t = TempDir::new("filekind");
        let txt = t.0.join("note.txt");
        let zip = t.0.join("Archive.ZIP");
        std::fs::write(&txt, b"x").unwrap();
        std::fs::write(&zip, b"PK").unwrap();
        assert_eq!(check_location(&fs_loc(&txt)), missing());
        assert_eq!(check_location(&fs_loc(&zip)), ProbeOutcome::Available);
        assert_eq!(check_location(&fs_loc(&t.0)), ProbeOutcome::Available);
        assert_eq!(check_location(&fs_loc(&t.0.join("gone"))), missing());
    }

    #[test]
    fn pidl_validation() {
        assert!(pidl_is_well_formed(&[4, 0, 9, 9, 0, 0]));
        assert!(!pidl_is_well_formed(&[0, 0]), "desktop");
        assert!(!pidl_is_well_formed(&[]));
        assert!(!pidl_is_well_formed(&[4, 0, 9, 9]), "unterminated");
        assert!(!pidl_is_well_formed(&[9, 0, 1, 0, 0]), "overrun");
        assert!(!pidl_is_well_formed(&[1, 0, 0, 0]), "cb < 2");
    }

    /// Sleeps for a Location whose last component is `hang`; a last component
    /// `gone` is missing; anything else is available.
    fn hang_check(hang_for: Duration) -> CheckFn {
        Arc::new(move |l: &LocationId| {
            let name = l.parsing_name.to_string_lossy();
            if name.ends_with("hang") {
                thread::sleep(hang_for);
            }
            if name.ends_with("gone") {
                missing()
            } else {
                ProbeOutcome::Available
            }
        })
    }

    #[test]
    fn hanging_probe_times_out_and_pool_recovers() {
        let deadline = Duration::from_millis(300);
        let slack = Duration::from_millis(250);
        let (p, rx) = prober(1, deadline, Some(hang_check(Duration::from_secs(3))));
        let start = Instant::now();
        p.probe(
            ProbeId(1),
            loc(LocationKind::FileSystem(Volume::Unc), r"\\srv\share\hang"),
        );
        let (id, o, at) = rx.recv_timeout(deadline + Duration::from_secs(2)).unwrap();
        assert_eq!((id, o), (ProbeId(1), ProbeOutcome::Timeout));
        let took = at - start;
        assert!(took >= deadline && took < deadline + slack, "{took:?}");

        // The sleeping worker cannot be cancelled: it is abandoned and replaced.
        thread::sleep(CANCEL_GRACE + Duration::from_millis(150));
        assert_eq!(p.worker_counts(), (1, 1));
        for i in 2..6 {
            let sent = Instant::now();
            p.probe(
                ProbeId(i),
                loc(LocationKind::FileSystem(Volume::Fixed), r"C:\fine"),
            );
            let (id, o, at) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!((id, o), (ProbeId(i), ProbeOutcome::Available));
            assert!(at - sent < deadline, "{:?}", at - sent);
        }
        // Dropping does not wait for the abandoned thread.
        let t = Instant::now();
        drop(p);
        assert!(t.elapsed() < SHUTDOWN_WAIT + slack);
    }

    /// An anonymous pipe; closing the write end first unblocks any reader.
    struct Pipe {
        read: isize,
        write: isize,
    }

    impl Pipe {
        fn new() -> Self {
            let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
            // SAFETY: valid out pointers; default security, default buffer size.
            unsafe {
                windows::Win32::System::Pipes::CreatePipe(&raw mut read, &raw mut write, None, 0)
            }
            .expect("CreatePipe");
            Self {
                read: read.0 as isize,
                write: write.0 as isize,
            }
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            use windows::Win32::Foundation::CloseHandle;
            // SAFETY: both handles are owned by this value and closed once.
            unsafe {
                let _ = CloseHandle(HANDLE(self.write as *mut c_void));
                let _ = CloseHandle(HANDLE(self.read as *mut c_void));
            }
        }
    }

    /// The deadline path with an I/O call that `CancelSynchronousIo` can
    /// interrupt (a synchronous `ReadFile` on an empty pipe, standing in for
    /// an SMB call to an unreachable server): Timeout at the deadline, the
    /// blocked call returns `ERROR_OPERATION_ABORTED`, and the worker is kept
    /// rather than abandoned (Req 9.3).
    #[test]
    fn timeout_cancels_blocking_io_and_keeps_the_worker() {
        use windows::Win32::Storage::FileSystem::ReadFile;

        let pipe = Pipe::new();
        let read = pipe.read;
        let results: Arc<Mutex<Vec<Option<u32>>>> = Arc::default();
        let sink = Arc::clone(&results);
        let check: CheckFn = Arc::new(move |l: &LocationId| {
            if l.parsing_name.to_string_lossy().ends_with("block") {
                let mut buf = [0u8; 1];
                let mut n = 0u32;
                // SAFETY: valid buffer and out pointer; the handle outlives the prober.
                let r = unsafe {
                    ReadFile(
                        HANDLE(read as *mut c_void),
                        Some(&mut buf),
                        Some(&raw mut n),
                        None,
                    )
                };
                sink.lock()
                    .unwrap()
                    .push(r.err().and_then(|e| win32_code(&e)));
            }
            ProbeOutcome::Available
        });

        let deadline = Duration::from_millis(200);
        let (p, rx) = prober(1, deadline, Some(check));
        let start = Instant::now();
        p.probe(
            ProbeId(1),
            loc(LocationKind::FileSystem(Volume::Unc), r"\\srv\share\block"),
        );
        let (id, o, at) = rx.recv_timeout(deadline + Duration::from_secs(2)).unwrap();
        assert_eq!((id, o), (ProbeId(1), ProbeOutcome::Timeout));
        let took = at - start;
        assert!(
            took >= deadline && took < deadline + Duration::from_millis(250),
            "{took:?}"
        );

        // The cancelled read returns well within the grace period.
        let until = Instant::now() + CANCEL_GRACE;
        while results.lock().unwrap().is_empty() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*results.lock().unwrap(), [Some(ERROR_OPERATION_ABORTED.0)]);
        thread::sleep(CANCEL_GRACE + Duration::from_millis(100));
        assert_eq!(p.worker_counts(), (1, 0), "worker kept, none abandoned");

        // The same worker serves the next probe; its late result was ignored.
        p.probe(
            ProbeId(2),
            loc(LocationKind::FileSystem(Volume::Fixed), r"C:\fine"),
        );
        let (id, o, _) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!((id, o), (ProbeId(2), ProbeOutcome::Available));
        assert!(rx.try_recv().is_err(), "probe 1 reported once");
        drop(p);
        drop(pipe);
    }

    #[test]
    fn hanging_probes_do_not_starve_fast_ones() {
        let deadline = Duration::from_millis(2000);
        let (p, rx) = prober(2, deadline, Some(hang_check(Duration::from_secs(3))));
        let start = Instant::now();
        p.probe(
            ProbeId(1),
            loc(LocationKind::FileSystem(Volume::Unc), r"\\srv\a\hang"),
        );
        p.probe(
            ProbeId(2),
            loc(LocationKind::FileSystem(Volume::Unc), r"\\srv\b\hang"),
        );
        thread::sleep(Duration::from_millis(50));
        p.probe(
            ProbeId(3),
            loc(LocationKind::FileSystem(Volume::Fixed), r"C:\fast"),
        );
        let (id, o, at) = rx.recv_timeout(deadline).unwrap();
        assert_eq!((id, o), (ProbeId(3), ProbeOutcome::Available));
        assert!(
            at - start < STALL_AFTER + Duration::from_millis(400),
            "{:?}",
            at - start
        );
        for _ in 0..2 {
            let (_, o, at) = rx.recv_timeout(deadline + Duration::from_secs(1)).unwrap();
            assert_eq!(o, ProbeOutcome::Timeout);
            assert!(at - start < deadline + Duration::from_millis(250));
        }
    }

    #[test]
    fn ancestors_share_the_deadline() {
        let deadline = Duration::from_millis(400);
        let (p, rx) = prober(
            DEFAULT_THREADS,
            deadline,
            Some(hang_check(Duration::from_secs(2))),
        );
        let l = loc(LocationKind::FileSystem(Volume::Fixed), r"C:\a\hang\gone");
        let start = Instant::now();
        p.probe(ProbeId(7), l.clone());
        let (id, o, at) = rx.recv_timeout(deadline + Duration::from_secs(2)).unwrap();
        assert_eq!(id, ProbeId(7));
        assert!(
            at - start < deadline + Duration::from_millis(250),
            "{:?}",
            at - start
        );
        let ProbeOutcome::Missing { ancestors: anc } = &o else {
            panic!("{o:?}")
        };
        let flags: Vec<(String, bool)> = anc
            .iter()
            .map(|a| (a.location.parsing_name.to_string_lossy(), a.available))
            .collect();
        assert_eq!(
            flags,
            [
                (r"C:\a\hang".into(), false),
                (r"C:\a".into(), true),
                (r"C:\".into(), true)
            ]
        );
        assert_eq!(
            choose_fallback(&l, &o),
            Fallback::Redirected(loc(l.kind, r"C:\a"))
        );
    }

    #[test]
    #[ignore = "needs name resolution; may take the full deadline"]
    fn unreachable_unc_share_returns_within_deadline() {
        let l = loc(
            LocationKind::FileSystem(Volume::Unc),
            r"\\nonexistent-host-esr\share\dir",
        );
        let start = Instant::now();
        let [got] = probe_all(&[l]).try_into().unwrap();
        assert!(start.elapsed() < DEFAULT_DEADLINE + Duration::from_millis(500));
        assert!(
            matches!(got, ProbeOutcome::Timeout | ProbeOutcome::Missing { .. }),
            "{got:?}"
        );
    }
}
