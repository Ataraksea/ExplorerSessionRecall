//! Shared proptest strategies for the session model (Req 8.4, 11.5).
//!
//! - [`wide_string`]: UTF-16 strings weighted towards letters, NFC/NFD pairs,
//!   unpaired surrogates, `%`, `=`, spaces and controls, with lengths 0, 1,
//!   2–40, 259–261 (around `MAX_PATH`) and 32,767 (`MAX_WIDE_LEN`, low weight).
//! - [`location_id`]: every [`LocationKind`], search specs with plain scopes.
//! - [`displays`]: 1–4 non-overlapping displays, exactly one primary at the
//!   origin, others placed left (negative x) or right with vertical offsets.
//! - [`snapshot`] / [`snapshot_with`]: valid by construction within the caps.
//! - [`session_doc`], [`timestamp`].
//! - [`availability_for`]: probe outcomes and latencies for every location of
//!   a snapshot (test-side types, see [`TestProbeOutcome`]).
//! - [`fold_scenario`], [`fold_windows`], [`desktop_view`], [`fold_location`]:
//!   contexts for the pure `fold::select` (Req 14.20); a [`FoldScenario`]
//!   owns the data a `FoldCtx` borrows.
//! - [`fold_burst`], [`fold_burst_with_faults`], [`fold_failure_schedule`]:
//!   bursts of candidates (creation order, Location arrival order, Location
//!   equalities) and failure schedules for the simulator (Req 14.19, 14.20).

use std::collections::{BTreeSet, HashMap, HashSet};

use esr_core::engine::{DesktopView, ItemId, WinKey};
use esr_core::fold::{FoldCtx, FoldWindow};
use esr_core::model::*;
use esr_sim::explorer::{Op, Pick, ProcessChoice};
use esr_sim::fold::{failure_schedule, fold_faults_strategy, selection_strategy, DesktopSpec, FoldFaults};
use proptest::collection::{btree_set, vec};
use proptest::prelude::*;
use proptest::sample::{select, subsequence};

// ---------------------------------------------------------------------------
// WideString
// ---------------------------------------------------------------------------

/// NFC form and NFD form of the same characters.
pub const NFC_NFD_PAIRS: &[(&[u16], &[u16])] = &[
    (&[0x00E9], &[0x0065, 0x0301]),         // é
    (&[0x00C5], &[0x0041, 0x030A]),         // Å
    (&[0x00F1], &[0x006E, 0x0303]),         // ñ
    (&[0x00FC], &[0x0075, 0x0308]),         // ü
    (&[0x1E69], &[0x0073, 0x0323, 0x0307]), // ṩ
    (&[0x30AC], &[0x30AB, 0x3099]),         // ガ
    (&[0xAC00], &[0x1100, 0x1161]),         // 가
];

/// Non-ASCII letters (all BMP, single unit).
const OTHER_LETTERS: &[u16] = &[0x00DF, 0x0416, 0x03A9, 0x05D0, 0x0627, 0x65E5, 0x0130, 0x0131];

fn ascii_letter() -> impl Strategy<Value = u16> {
    prop_oneof![(b'a'..=b'z').prop_map(u16::from), (b'A'..=b'Z').prop_map(u16::from)]
}

fn letter() -> impl Strategy<Value = u16> {
    prop_oneof![
        6 => ascii_letter(),
        2 => (b'0'..=b'9').prop_map(u16::from),
        2 => select(OTHER_LETTERS),
    ]
}

/// A short run of code units; strings are built by cycling a few of these.
fn chunk() -> impl Strategy<Value = Vec<u16>> {
    prop_oneof![
        8 => letter().prop_map(|c| vec![c]),
        3 => (select(NFC_NFD_PAIRS), 0u8..3).prop_map(|((nfc, nfd), form)| match form {
            0 => nfc.to_vec(),
            1 => nfd.to_vec(),
            _ => [nfc, nfd].concat(),
        }),
        // High surrogate followed by a non-surrogate: unpaired.
        2 => (0xD800u16..=0xDBFF, ascii_letter()).prop_map(|(h, l)| vec![h, l]),
        // Low surrogate preceded by a non-surrogate: unpaired.
        2 => (ascii_letter(), 0xDC00u16..=0xDFFF).prop_map(|(l, s)| vec![l, s]),
        // Well-formed supplementary character.
        1 => (0xD800u16..=0xDBFF, 0xDC00u16..=0xDFFF).prop_map(|(h, l)| vec![h, l]),
        2 => Just(vec![u16::from(b'%')]),
        2 => Just(vec![u16::from(b'=')]),
        2 => Just(vec![u16::from(b' ')]),
        2 => prop_oneof![0u16..=0x1F, Just(0x7F)].prop_map(|c| vec![c]),
        1 => select(&b"\\:./\"';#&[]{}"[..]).prop_map(|c| vec![u16::from(c)]),
    ]
}

/// Lengths 0, 1, 2–40, 259–261 and `MAX_WIDE_LEN` (low weight).
pub fn wide_len() -> impl Strategy<Value = usize> {
    prop_oneof![
        2 => Just(0usize),
        2 => Just(1usize),
        12 => 2usize..=40,
        3 => 259usize..=261,
        1 => Just(MAX_WIDE_LEN),
    ]
}

/// A `WideString` of exactly a length drawn from `len`: a short random
/// pattern of chunks, cycled (and possibly cut mid-chunk) to that length.
pub fn wide_string_of(len: impl Strategy<Value = usize>) -> impl Strategy<Value = WideString> {
    (len, vec(chunk(), 1..12)).prop_map(|(len, chunks)| {
        let pattern: Vec<u16> = chunks.concat();
        WideString(pattern.iter().copied().cycle().take(len).collect())
    })
}

/// The default `WideString` strategy (lengths from [`wide_len`]).
pub fn wide_string() -> impl Strategy<Value = WideString> {
    wide_string_of(wide_len())
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

pub const ALL_VOLUMES: [Volume; 4] = [Volume::Fixed, Volume::Removable, Volume::Mapped, Volume::Unc];

pub const ALL_SHELL_KINDS: [ShellKind; 10] = [
    ShellKind::ThisPc,
    ShellKind::Home,
    ShellKind::Gallery,
    ShellKind::RecycleBin,
    ShellKind::Network,
    ShellKind::Library,
    ShellKind::ControlPanel,
    ShellKind::PortableDevice,
    ShellKind::SavedSearch,
    ShellKind::Other,
];

/// Every `LocationKind`, for exhaustive coverage checks.
pub fn all_location_kinds() -> Vec<LocationKind> {
    ALL_VOLUMES
        .iter()
        .map(|&v| LocationKind::FileSystem(v))
        .chain([LocationKind::ZipFolder])
        .chain(ALL_SHELL_KINDS.iter().map(|&k| LocationKind::Shell(k)))
        .chain([LocationKind::Search])
        .collect()
}

/// Every kind except `Search`.
pub fn plain_location_kind() -> impl Strategy<Value = LocationKind> {
    prop_oneof![
        5 => select(ALL_VOLUMES.to_vec()).prop_map(LocationKind::FileSystem),
        1 => Just(LocationKind::ZipFolder),
        3 => select(ALL_SHELL_KINDS.to_vec()).prop_map(LocationKind::Shell),
    ]
}

pub fn location_kind() -> impl Strategy<Value = LocationKind> {
    prop_oneof![9 => plain_location_kind(), 1 => Just(LocationKind::Search)]
}

fn shell_parsing_name(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::ThisPc => "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
        ShellKind::Home => "::{F874310E-B6B7-47DC-BC84-B9E6B38F5903}",
        ShellKind::Gallery => "::{E88865EA-0E1C-4E20-9AA6-EDCD0212C87C}",
        ShellKind::RecycleBin => "::{645FF040-5081-101B-9F08-00AA002F954E}",
        ShellKind::Network => "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}",
        ShellKind::Library => {
            "::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\\{7B0DB17D-9CD2-4A93-9733-46CC89022E7C}"
        }
        ShellKind::ControlPanel => "::{26EE0668-A00A-44D7-9371-BEB064C98683}\\0",
        ShellKind::PortableDevice => {
            "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\\\\\?\\usb#vid_04e8&pid_6860#0001#{6ac27878-a6fa-4155-ba85-f98f491d4f33}"
        }
        ShellKind::SavedSearch => "C:\\Users\\me\\Searches\\saved.search-ms",
        ShellKind::Other => "::{21EC2020-3AEA-1069-A2DD-08002B30309D}",
    }
}

fn join_path(prefix: &str, components: Vec<WideString>) -> WideString {
    let mut out = units(prefix);
    for (i, c) in components.into_iter().enumerate() {
        if i > 0 {
            out.push(u16::from(b'\\'));
        }
        out.extend(c.0);
    }
    WideString(out)
}

/// A realistic parsing name for `kind` (always well under `MAX_WIDE_LEN`).
fn realistic_parsing_name(kind: LocationKind) -> BoxedStrategy<WideString> {
    let comps = || vec(wide_string_of(1usize..=12), 0..6);
    match kind {
        LocationKind::FileSystem(v) => {
            let prefix = match v {
                Volume::Fixed => "C:\\",
                Volume::Removable => "E:\\",
                Volume::Mapped => "Z:\\",
                Volume::Unc => "\\\\server\\share\\",
            };
            comps().prop_map(move |c| join_path(prefix, c)).boxed()
        }
        LocationKind::ZipFolder => (comps(), comps())
            .prop_map(|(outer, inner)| {
                let mut p = join_path("C:\\", outer).0;
                p.extend(units("\\archive.zip"));
                for c in inner {
                    p.push(u16::from(b'\\'));
                    p.extend(c.0);
                }
                WideString(p)
            })
            .boxed(),
        LocationKind::Shell(k) => Just(WideString::from(shell_parsing_name(k))).boxed(),
        LocationKind::Search => wide_string_of(1usize..=40)
            .prop_map(|q| {
                let mut p = units("search-ms:displayname=");
                p.extend(q.0);
                p.extend(units("&crumb=location:C%3A%5C"));
                WideString(p)
            })
            .boxed(),
    }
}

fn parsing_name(kind: LocationKind) -> impl Strategy<Value = WideString> {
    prop_oneof![3 => realistic_parsing_name(kind), 2 => wide_string().boxed()]
}

fn pidl() -> impl Strategy<Value = Vec<u8>> {
    vec(any::<u8>(), 0..64)
}

/// A `LocationId` of exactly `kind`. For `Search`, the scope is a plain
/// (non-search) location.
pub fn location_of_kind(kind: LocationKind) -> BoxedStrategy<LocationId> {
    let search = if kind == LocationKind::Search {
        (wide_string(), plain_location_id())
            .prop_map(|(query, scope)| Some(SearchSpec { query, scope: Box::new(scope) }))
            .boxed()
    } else {
        Just(None).boxed()
    };
    (parsing_name(kind), wide_string(), pidl(), search)
        .prop_map(move |(parsing_name, display_name, pidl, search)| LocationId {
            case_sensitive: false,
            kind,
            parsing_name,
            display_name,
            pidl,
            search,
        })
        .boxed()
}

/// Any location except a search.
pub fn plain_location_id() -> BoxedStrategy<LocationId> {
    plain_location_kind().prop_flat_map(location_of_kind).boxed()
}

/// Any location, covering every `LocationKind`.
pub fn location_id() -> BoxedStrategy<LocationId> {
    location_kind().prop_flat_map(location_of_kind).boxed()
}

// ---------------------------------------------------------------------------
// Displays and placement
// ---------------------------------------------------------------------------

fn monitor_size() -> impl Strategy<Value = (i32, i32)> {
    prop_oneof![
        3 => select(vec![(1920, 1080), (2560, 1440), (3840, 2160), (1366, 768), (1080, 1920)]),
        1 => (640i32..=3840, 480i32..=2160),
    ]
}

/// (edge 0..4 = bottom/top/left/right, thickness in px).
fn taskbar() -> impl Strategy<Value = (u8, i32)> {
    (0u8..4, prop_oneof![Just(0i32), 30i32..=96])
}

fn work_area(m: Rect, (edge, t): (u8, i32)) -> Rect {
    let mut w = m;
    match edge {
        0 => w.bottom -= t,
        1 => w.top += t,
        2 => w.left += t,
        _ => w.right -= t,
    }
    w
}

/// 1–4 displays laid out in a row with no overlap. The primary is at the
/// origin; displays before it in the row have negative x, and non-primary
/// displays are shifted vertically (possibly to negative y).
pub fn displays() -> impl Strategy<Value = Vec<Display>> {
    (1usize..=4)
        .prop_flat_map(|n| {
            (vec(monitor_size(), n), 0..n, vec(-1200i32..=1200, n), vec(taskbar(), n))
        })
        .prop_map(|(sizes, primary, dys, bars)| {
            let n = sizes.len();
            let mut lefts = vec![0i32; n];
            let mut x = 0;
            for i in (0..primary).rev() {
                x -= sizes[i].0;
                lefts[i] = x;
            }
            let mut x = sizes[primary].0;
            for i in primary + 1..n {
                lefts[i] = x;
                x += sizes[i].0;
            }
            (0..n)
                .map(|i| {
                    let (w, h) = sizes[i];
                    let top = if i == primary { 0 } else { dys[i] };
                    let monitor = Rect { left: lefts[i], top, right: lefts[i] + w, bottom: top + h };
                    Display {
                        device: format!(r"\\.\DISPLAY{}", i + 1),
                        monitor,
                        work: work_area(monitor, bars[i]),
                        primary: i == primary,
                    }
                })
                .collect()
        })
}

pub fn show_state() -> impl Strategy<Value = ShowState> {
    prop_oneof![
        4 => Just(ShowState::Normal),
        2 => Just(ShowState::Maximized),
        1 => any::<bool>().prop_map(|restore_maximized| ShowState::Minimized { restore_maximized }),
    ]
}

/// A positive rect inside `area` (fractions in permille).
fn rect_inside(area: Rect) -> impl Strategy<Value = Rect> {
    (0i64..1000, 0i64..1000, 1i64..=1000, 1i64..=1000).prop_map(move |(fx, fy, fw, fh)| {
        let (w, h) = (area.width(), area.height());
        let left = area.left as i64 + w * fx / 1000;
        let top = area.top as i64 + h * fy / 1000;
        let width = ((area.right as i64 - left) * fw / 1000).max(1);
        let height = ((area.bottom as i64 - top) * fh / 1000).max(1);
        Rect {
            left: left as i32,
            top: top as i32,
            right: (left + width) as i32,
            bottom: (top + height) as i32,
        }
    })
}

/// Any positive rect, including far off-screen and negative positions.
pub fn any_rect() -> impl Strategy<Value = Rect> {
    (-8000i32..8000, -5000i32..5000, 1i32..=5000, 1i32..=4000).prop_map(|(left, top, w, h)| Rect {
        left,
        top,
        right: left + w,
        bottom: top + h,
    })
}

/// A placement whose display is one of `displays` (or, with low weight, a
/// display that is no longer attached).
pub fn placement_on(displays: Vec<Display>) -> impl Strategy<Value = Placement> {
    let current = select(displays)
        .prop_map(|d| DisplayRef { device: d.device, monitor: d.monitor })
        .boxed();
    let vanished = (5u32..=9, any_rect())
        .prop_map(|(i, monitor)| DisplayRef { device: format!(r"\\.\DISPLAY{i}"), monitor })
        .boxed();
    let display = prop_oneof![6 => current, 1 => vanished];
    (show_state(), display)
        .prop_flat_map(|(state, display)| {
            let restored = prop_oneof![3 => rect_inside(display.monitor).boxed(), 1 => any_rect().boxed()];
            (Just(state), restored, Just(display))
        })
        .prop_map(|(state, restored, display)| Placement { state, restored, display })
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Size bounds for [`snapshot_with`]. Must stay within the model caps.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotParams {
    pub max_windows: usize,
    pub max_tabs_per_window: usize,
    pub max_tabs_total: usize,
}

impl Default for SnapshotParams {
    /// Small snapshots for fast tests.
    fn default() -> Self {
        Self { max_windows: 6, max_tabs_per_window: 8, max_tabs_total: MAX_TABS_TOTAL }
    }
}

impl SnapshotParams {
    /// Up to the model caps (100 windows, 500 tabs).
    pub fn at_caps() -> Self {
        Self {
            max_windows: MAX_WINDOWS,
            max_tabs_per_window: MAX_TABS_PER_WINDOW,
            max_tabs_total: MAX_TABS_TOTAL,
        }
    }
}

/// Clamps per-window tab counts so the total fits `max_total` while every
/// window keeps at least one tab.
fn clamp_counts(mut counts: Vec<usize>, max_total: usize) -> Vec<usize> {
    let mut budget = max_total;
    let n = counts.len();
    for (i, c) in counts.iter_mut().enumerate() {
        let allowed = budget - (n - i - 1);
        *c = (*c).min(allowed).max(1);
        budget -= *c;
    }
    counts
}

/// A valid snapshot whose placements refer to `displays`.
pub fn snapshot_on(displays: Vec<Display>, p: SnapshotParams) -> impl Strategy<Value = Snapshot> {
    assert!(p.max_windows <= MAX_WINDOWS && p.max_tabs_total <= MAX_TABS_TOTAL);
    assert!(p.max_tabs_per_window >= 1 && p.max_tabs_per_window <= MAX_TABS_PER_WINDOW);
    assert!(p.max_windows <= p.max_tabs_total, "every window needs a tab");
    (0..=p.max_windows)
        .prop_flat_map(move |n| (vec(1..=p.max_tabs_per_window, n), btree_set(any::<u32>(), n)))
        .prop_flat_map(move |(counts, ids)| {
            let counts = clamp_counts(counts, p.max_tabs_total);
            let ids: Vec<WindowId> = ids.into_iter().map(WindowId).collect();
            let windows: Vec<_> = counts
                .iter()
                .map(|&c| (vec(location_id(), c), 0..c, placement_on(displays.clone())))
                .collect();
            (Just(ids.clone()), windows, Just(ids).prop_shuffle())
        })
        .prop_map(|(ids, windows, activation)| Snapshot {
            windows: ids
                .into_iter()
                .zip(windows)
                .map(|(id, (locations, active, placement))| WindowRec {
                    id,
                    tabs: locations.into_iter().map(|location| TabRec { location }).collect(),
                    active,
                    placement,
                })
                .collect(),
            activation,
        })
}

/// Displays together with a valid snapshot placed on them.
pub fn displays_and_snapshot_with(p: SnapshotParams) -> impl Strategy<Value = (Vec<Display>, Snapshot)> {
    displays().prop_flat_map(move |d| (Just(d.clone()), snapshot_on(d, p)))
}

pub fn displays_and_snapshot() -> impl Strategy<Value = (Vec<Display>, Snapshot)> {
    displays_and_snapshot_with(SnapshotParams::default())
}

pub fn snapshot_with(p: SnapshotParams) -> impl Strategy<Value = Snapshot> {
    displays_and_snapshot_with(p).prop_map(|(_, s)| s)
}

/// A small valid snapshot (see [`SnapshotParams::default`]).
pub fn snapshot() -> impl Strategy<Value = Snapshot> {
    snapshot_with(SnapshotParams::default())
}

// ---------------------------------------------------------------------------
// Session document
// ---------------------------------------------------------------------------

/// The Session_File_Format version the strategies produce.
pub const FORMAT_VERSION: u32 = 1;

fn days_in_month(year: u16, month: u8) -> u8 {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A valid timestamp; offsets are mostly whole quarter hours from −12:00 to
/// +14:00, sometimes any minute within ±23:59.
pub fn timestamp() -> impl Strategy<Value = Timestamp> {
    let year = prop_oneof![8 => 2000u16..=2099, 1 => 1u16..=9999];
    let offset = prop_oneof![4 => (-48i16..=56).prop_map(|q| q * 15), 1 => -1439i16..=1439];
    (year, 1u8..=12, offset)
        .prop_flat_map(|(year, month, offset)| {
            (
                Just((year, month, offset)),
                1..=days_in_month(year, month),
                (0u8..24, 0u8..60, 0u8..60, 0u16..1000),
            )
        })
        .prop_map(|((year, month, offset_minutes), day, (hour, minute, second, millis))| Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            millis,
            offset_minutes,
        })
}

pub fn status() -> impl Strategy<Value = Status> {
    prop_oneof![Just(Status::Pending), Just(Status::Consumed)]
}

pub const ALL_SAVE_CAUSES: [SaveCause; 8] = [
    SaveCause::Checkpoint,
    SaveCause::LastWindowClosed,
    SaveCause::LastTabClosed,
    SaveCause::ExplorerTerminated,
    SaveCause::SignOut,
    SaveCause::Shutdown,
    SaveCause::Restart,
    SaveCause::ExitThroughSettings,
];

pub fn save_cause() -> impl Strategy<Value = SaveCause> {
    select(ALL_SAVE_CAUSES.to_vec())
}

/// A valid document around snapshots drawn from `snapshot`.
pub fn session_doc_of(snapshot: impl Strategy<Value = Snapshot>) -> impl Strategy<Value = SessionDoc> {
    (status(), 0..=MAX_FAILED_ATTEMPTS, timestamp(), save_cause(), snapshot).prop_map(
        |(status, failed_attempts, saved_at, cause, snapshot)| SessionDoc {
            version: FORMAT_VERSION,
            status,
            failed_attempts,
            saved_at,
            cause,
            snapshot,
        },
    )
}

pub fn session_doc() -> impl Strategy<Value = SessionDoc> {
    session_doc_of(snapshot())
}

// ---------------------------------------------------------------------------
// Availability and probe latencies
// ---------------------------------------------------------------------------
//
// Test-side stand-ins for the core `ProbeOutcome`. Planner tests can replace
// `TestProbeOutcome` with the real type by changing `probe_result_for`.

/// Probe deadline (design.md, "Probe pool": 5 s).
pub const PROBE_DEADLINE_MS: u32 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TestProbeOutcome {
    Available,
    /// Does not exist; only for FileSystem and ZipFolder locations.
    Missing,
    AccessDenied,
    /// Latency is always [`PROBE_DEADLINE_MS`].
    Timeout,
    Unresolvable,
    /// Only for Shell and Search locations.
    ShellUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub outcome: TestProbeOutcome,
    /// Time from probe start to the result.
    pub latency_ms: u32,
}

/// Keyed by canonical location key (`LocationId`'s `Eq`/`Hash`).
pub type AvailabilityMap = HashMap<LocationId, ProbeResult>;

/// Network locations: UNC, mapped drives and the Network folder.
pub fn is_network(loc: &LocationId) -> bool {
    matches!(
        loc.kind,
        LocationKind::FileSystem(Volume::Unc | Volume::Mapped) | LocationKind::Shell(ShellKind::Network)
    )
}

/// An outcome and latency consistent with `loc`'s kind. Network locations
/// are slower and time out more often.
pub fn probe_result_for(loc: &LocationId) -> BoxedStrategy<ProbeResult> {
    use TestProbeOutcome::*;
    let network = is_network(loc);
    let timeout_weight = if network { 3 } else { 1 };
    let outcome = match loc.kind {
        LocationKind::FileSystem(_) | LocationKind::ZipFolder => prop_oneof![
            6 => Just(Available),
            2 => Just(Missing),
            1 => Just(AccessDenied),
            timeout_weight => Just(Timeout),
            1 => Just(Unresolvable),
        ]
        .boxed(),
        LocationKind::Shell(_) | LocationKind::Search => prop_oneof![
            6 => Just(Available),
            2 => Just(ShellUnavailable),
            timeout_weight => Just(Timeout),
            1 => Just(Unresolvable),
        ]
        .boxed(),
    };
    let latency = if network {
        prop_oneof![1 => 0u32..100, 2 => 100u32..PROBE_DEADLINE_MS].boxed()
    } else {
        prop_oneof![9 => 0u32..=50, 1 => 51u32..=1000].boxed()
    };
    (outcome, latency)
        .prop_map(|(outcome, latency_ms)| ProbeResult {
            outcome,
            latency_ms: if outcome == Timeout { PROBE_DEADLINE_MS } else { latency_ms },
        })
        .boxed()
}

/// Every distinct location a restore of `snapshot` probes: each tab location
/// and each search scope, in first-seen order.
pub fn probe_targets(snapshot: &Snapshot) -> Vec<LocationId> {
    fn push(loc: &LocationId, seen: &mut HashSet<LocationId>, out: &mut Vec<LocationId>) {
        if seen.insert(loc.clone()) {
            out.push(loc.clone());
        }
        if let Some(spec) = &loc.search {
            push(&spec.scope, seen, out);
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for w in &snapshot.windows {
        for t in &w.tabs {
            push(&t.location, &mut seen, &mut out);
        }
    }
    out
}

/// A probe result for every [`probe_targets`] entry of `snapshot`.
pub fn availability_for(snapshot: &Snapshot) -> impl Strategy<Value = AvailabilityMap> + use<> {
    let targets = probe_targets(snapshot);
    let results: Vec<_> = targets.iter().map(probe_result_for).collect();
    (Just(targets), results).prop_map(|(targets, results)| targets.into_iter().zip(results).collect())
}

/// Displays, a small valid snapshot, and availability for all its locations.
pub fn restore_scenario() -> impl Strategy<Value = (Vec<Display>, Snapshot, AvailabilityMap)> {
    displays_and_snapshot().prop_flat_map(|(d, s)| {
        let avail = availability_for(&s);
        (Just(d), Just(s), avail)
    })
}

// ---------------------------------------------------------------------------
// Tab folding: contexts for the pure `fold::select` (Req 14.20)
// ---------------------------------------------------------------------------
//
// `FoldCtx` and `FoldWindow` borrow their `LocationKey`s, so the strategies
// produce the owned [`FoldScenario`], which lends a `FoldCtx` through
// [`FoldScenario::with_ctx`].

/// Number of distinct Locations in the fold pool.
pub const FOLD_POOL_SIZE: usize = 6;

/// Number of spellings per pool Location (see [`pool_location`]).
pub const FOLD_SPELLINGS: u8 = 4;

/// Pool entry `idx` (taken modulo [`FOLD_POOL_SIZE`]) in spelling `spelling`
/// (taken modulo [`FOLD_SPELLINGS`]). Spellings of the same entry share one
/// `fold_key`; different entries never do.
///
/// Entries 0 to 2 are drive paths (spellings: exact, lower case, trailing
/// separator, `\\?\` prefix), entry 3 is a UNC share, entry 4 is This PC and
/// entry 5 is a search in entry 0's folder; the last three are spelled one way.
pub fn pool_location(idx: usize, spelling: u8) -> LocationId {
    let (idx, spelling) = (idx % FOLD_POOL_SIZE, spelling % FOLD_SPELLINGS);
    let plain = |kind: LocationKind, name: &str| LocationId {
        case_sensitive: false,
        kind,
        parsing_name: WideString::from(name),
        display_name: WideString::from(name),
        pidl: vec![idx as u8],
        search: None,
    };
    let drive = |name: &str| {
        let spelled = match spelling {
            0 => name.to_owned(),
            1 => name.to_lowercase(),
            2 => format!("{name}\\"),
            _ => format!(r"\\?\{name}"),
        };
        plain(LocationKind::FileSystem(Volume::Fixed), &spelled)
    };
    match idx {
        0 => drive(r"C:\Users"),
        1 => drive(r"C:\Users\Docs"),
        2 => drive(r"D:\Data"),
        3 => plain(LocationKind::FileSystem(Volume::Unc), r"\\srv\share"),
        4 => plain(LocationKind::Shell(ShellKind::ThisPc), shell_parsing_name(ShellKind::ThisPc)),
        _ => LocationId {
            search: Some(SearchSpec { query: WideString::from("report"), scope: Box::new(pool_location(0, 0)) }),
            ..plain(LocationKind::Search, "search-ms:displayname=report")
        },
    }
}

/// A Location from the small pool, in any spelling, so that equal keys occur
/// often.
pub fn fold_location() -> impl Strategy<Value = LocationId> {
    (0..FOLD_POOL_SIZE, 0..FOLD_SPELLINGS).prop_map(|(i, s)| pool_location(i, s))
}

/// The `n`-th tracked window of a scenario.
pub fn fold_win_key(n: usize) -> WinKey {
    WinKey { hwnd: 0x1000 + n as u64, seq: 1 + n as u64 }
}

/// The `n`-th window that is open but not tracked (an excluded window).
fn untracked_win_key(n: usize) -> WinKey {
    WinKey { hwnd: 0x2000 + n as u64, seq: 100 + n as u64 }
}

/// `None` or one of `keys`.
fn opt_key(keys: Vec<WinKey>) -> BoxedStrategy<Option<WinKey>> {
    if keys.is_empty() {
        Just(None).boxed()
    } else {
        prop_oneof![1 => Just(None), 3 => select(keys).prop_map(Some)].boxed()
    }
}

/// A `DesktopView`: `Unknown` or `Known` with each of `universe` in the set
/// with high probability (so that some windows are off the desktop).
pub fn desktop_view_of(universe: Vec<WinKey>) -> impl Strategy<Value = DesktopView> {
    let n = universe.len();
    prop_oneof![
        1 => Just(DesktopView::Unknown),
        4 => vec(prop_oneof![3 => Just(true), 1 => Just(false)], n).prop_map(move |keep| {
            DesktopView::Known(universe.iter().zip(keep).filter(|(_, k)| *k).map(|(w, _)| *w).collect())
        }),
    ]
}

/// A `DesktopView` over up to eight windows.
pub fn desktop_view() -> impl Strategy<Value = DesktopView> {
    (0usize..=8).prop_flat_map(|n| desktop_view_of((0..n).map(fold_win_key).collect()))
}

/// One tab of a [`FoldWindowData`].
#[derive(Clone, Debug)]
pub struct FoldTabData {
    /// The Location the tab shows; `None` when it is not known (it matches
    /// nothing). A Deferred_Tab always has one: the Location the tracker
    /// records for it (Req 5.13).
    pub location: Option<LocationId>,
    pub deferred: bool,
}

/// Owned form of a `FoldWindow`.
#[derive(Clone, Debug)]
pub struct FoldWindowData {
    pub win: WinKey,
    pub tabs: Vec<FoldTabData>,
    pub active: Option<usize>,
    pub closing: bool,
}

fn fold_tab() -> impl Strategy<Value = FoldTabData> {
    prop_oneof![
        6 => fold_location().prop_map(|l| FoldTabData { location: Some(l), deferred: false }),
        2 => fold_location().prop_map(|l| FoldTabData { location: Some(l), deferred: true }),
        1 => Just(FoldTabData { location: None, deferred: false }),
    ]
}

/// A tracked window `win` with 1–5 tabs, an Active_Tab (rarely unknown) and a
/// closing flag (about one in five).
pub fn fold_window_data(win: WinKey) -> impl Strategy<Value = FoldWindowData> {
    vec(fold_tab(), 1..=5)
        .prop_flat_map(|tabs| {
            let n = tabs.len();
            (Just(tabs), prop_oneof![9 => (0..n).prop_map(Some), 1 => Just(None)])
        })
        .prop_flat_map(move |(tabs, active)| {
            (Just(tabs), Just(active), prop_oneof![4 => Just(false), 1 => Just(true)])
        })
        .prop_map(move |(tabs, active, closing)| FoldWindowData { win, tabs, active, closing })
}

/// 0–6 tracked windows with distinct keys, in arbitrary order.
pub fn fold_windows() -> impl Strategy<Value = Vec<FoldWindowData>> {
    (0usize..=6)
        .prop_flat_map(|n| (0..n).map(|i| fold_window_data(fold_win_key(i))).collect::<Vec<_>>())
        .prop_shuffle()
}

/// Pending Folded_Tabs of earlier candidates: each targets one of `keys`.
pub fn pending_folds_of(keys: Vec<WinKey>) -> BoxedStrategy<Vec<(WinKey, LocationId)>> {
    if keys.is_empty() {
        Just(Vec::new()).boxed()
    } else {
        vec((select(keys), fold_location()), 0..=3).boxed()
    }
}

/// An Opening_Order: `keys` plus up to two untracked (excluded) windows, in
/// any order.
pub fn opening_of(keys: Vec<WinKey>) -> impl Strategy<Value = Vec<WinKey>> {
    (0usize..=2).prop_flat_map(move |extra| {
        Just(keys.iter().copied().chain((0..extra).map(untracked_win_key)).collect::<Vec<_>>()).prop_shuffle()
    })
}

/// A subset of `keys`: the windows that did not answer `WM_NULL` in time.
pub fn unresponsive_of(keys: Vec<WinKey>) -> impl Strategy<Value = BTreeSet<WinKey>> {
    let n = keys.len();
    subsequence(keys, 0..=n).prop_map(|v| v.into_iter().collect())
}

/// Everything a `FoldCtx` borrows, owned.
#[derive(Clone, Debug)]
pub struct FoldScenario {
    /// Location of the candidate (a candidate itself is never in `windows`).
    pub candidate: LocationId,
    pub foreground: Option<WinKey>,
    /// Opening_Order: every tracked window, plus excluded windows that are not
    /// tracked.
    pub opening: Vec<WinKey>,
    /// Activation_Order: every tracked window, most recent first.
    pub activation: Vec<WinKey>,
    pub windows: Vec<FoldWindowData>,
    pub pending_folds: Vec<(WinKey, LocationId)>,
    pub desktop: DesktopView,
    pub unresponsive: BTreeSet<WinKey>,
    pub exclude: Option<WinKey>,
}

impl FoldScenario {
    /// Builds the borrowing `FoldCtx` and passes it to `f`.
    pub fn with_ctx<R>(&self, f: impl FnOnce(&FoldCtx<'_>) -> R) -> R {
        let windows: Vec<FoldWindow<'_>> = self
            .windows
            .iter()
            .map(|w| FoldWindow {
                win: w.win,
                tabs: w.tabs.iter().map(|t| t.location.as_ref().map(LocationId::fold_key)).collect(),
                active: w.active,
                closing: w.closing,
            })
            .collect();
        let pending: Vec<(WinKey, LocationKey<'_>)> =
            self.pending_folds.iter().map(|(w, l)| (*w, l.fold_key())).collect();
        f(&FoldCtx {
            candidate_key: self.candidate.fold_key(),
            foreground: self.foreground,
            opening: &self.opening,
            activation: &self.activation,
            windows: &windows,
            pending_folds: &pending,
            desktop: &self.desktop,
            unresponsive: &self.unresponsive,
            exclude: self.exclude,
        })
    }

    /// The keys of the tracked windows, in list order.
    pub fn keys(&self) -> Vec<WinKey> {
        self.windows.iter().map(|w| w.win).collect()
    }
}

/// A complete fold context: tracked windows, Opening_Order and
/// Activation_Order over them, a desktop view, unresponsive windows,
/// `foreground`, `exclude` and pending Folded_Tabs.
pub fn fold_scenario() -> impl Strategy<Value = FoldScenario> {
    fold_windows().prop_flat_map(|windows| {
        let keys: Vec<WinKey> = windows.iter().map(|w| w.win).collect();
        (
            Just(windows),
            fold_location(),
            opt_key(keys.clone()),
            opening_of(keys.clone()),
            Just(keys.clone()).prop_shuffle(),
            pending_folds_of(keys.clone()),
            desktop_view_of(keys.iter().copied().chain((0..2).map(untracked_win_key)).collect()),
            unresponsive_of(keys.clone()),
            opt_key(keys),
        )
    })
    .prop_map(
        |(windows, candidate, foreground, opening, activation, pending_folds, desktop, unresponsive, exclude)| {
            FoldScenario {
                candidate,
                foreground,
                opening,
                activation,
                windows,
                pending_folds,
                desktop,
                unresponsive,
                exclude,
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Tab folding: bursts of candidates and failure schedules (Req 14.8, 14.19, 14.20)
// ---------------------------------------------------------------------------

/// One Folding_Candidate of a [`FoldBurst`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BurstCandidate {
    /// Which pool Location its first navigation shows (see [`pool_location`]).
    /// Candidates with the same `slot` show Locations with equal `fold_key`s.
    pub slot: usize,
    /// The spelling of that Location (see [`pool_location`]).
    pub spelling: u8,
    /// The items it was opened to select; empty if none.
    pub selection: Vec<ItemId>,
    /// Its Foreground_Explorer_Window, as a pick among the open windows.
    pub foreground: Option<u16>,
    /// Logical time from this candidate's first signal to the next one's.
    pub gap_ms: u64,
    /// Logical time from its first signal to its Location being known.
    pub first_navigation_ms: u64,
}

/// Candidates opened in a burst. The creation order is the order of the list;
/// the order in which the Locations become known is [`arrival_order`]
/// (which differs from it whenever a later candidate's first navigation is
/// quicker than an earlier one's).
///
/// [`arrival_order`]: FoldBurst::arrival_order
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldBurst {
    pub candidates: Vec<BurstCandidate>,
}

impl FoldBurst {
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// The Location candidate `i` shows (a pool Location in its spelling).
    pub fn location(&self, i: usize) -> LocationId {
        pool_location(self.candidates[i].slot, self.candidates[i].spelling)
    }

    /// Logical time of the first signal of each candidate, from the first one.
    pub fn first_signals(&self) -> Vec<u64> {
        let mut at = 0;
        self.candidates
            .iter()
            .map(|c| {
                let now = at;
                at += c.gap_ms;
                now
            })
            .collect()
    }

    /// The candidates' indices in the order their Locations become known:
    /// by arrival time, ties in creation order.
    pub fn arrival_order(&self) -> Vec<usize> {
        let arrival: Vec<u64> =
            self.first_signals().iter().zip(&self.candidates).map(|(s, c)| s + c.first_navigation_ms).collect();
        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_by_key(|&i| (arrival[i], i));
        order
    }

    /// Groups of candidates with equal `fold_key`s (singletons left out), each
    /// in creation order.
    pub fn equalities(&self) -> Vec<Vec<usize>> {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for i in 0..self.len() {
            match groups.iter_mut().find(|g| self.candidates[g[0]].slot == self.candidates[i].slot) {
                Some(g) => g.push(i),
                None => groups.push(vec![i]),
            }
        }
        groups.retain(|g| g.len() > 1);
        groups
    }

    /// The simulator operations that open the burst: for each candidate its
    /// first-navigation delay, the candidate itself (on the pool entry `slot`
    /// of the simulator's Locations, spelled one way only) and the gap to the
    /// next one. The delay of the last candidate stays in effect afterwards.
    pub fn sim_ops(&self) -> Vec<Op> {
        let mut ops = Vec::new();
        for c in &self.candidates {
            ops.push(Op::SetFirstNavigation { ms: Some(c.first_navigation_ms) });
            ops.push(Op::OpenCandidate {
                location: Pick(c.slot as u16),
                selection: c.selection.clone(),
                bypass_keys: false,
                desktop: DesktopSpec::Current,
                foreground: c.foreground.map(Pick),
                process: ProcessChoice::Main,
            });
            ops.push(Op::Tick { ms: c.gap_ms });
        }
        ops
    }
}

fn burst_candidate() -> impl Strategy<Value = BurstCandidate> {
    (
        // Mostly four pool entries, so that equal Locations are common.
        prop_oneof![5 => 0usize..4, 1 => 0..FOLD_POOL_SIZE],
        0..FOLD_SPELLINGS,
        selection_strategy(),
        proptest::option::weighted(0.4, any::<u16>()),
        prop_oneof![2 => Just(0u64), 3 => 1u64..=60, 1 => 61u64..=400],
        // Wide enough that a later candidate's Location can arrive first.
        20u64..=600,
    )
        .prop_map(|(slot, spelling, selection, foreground, gap_ms, first_navigation_ms)| BurstCandidate {
            slot,
            spelling,
            selection,
            foreground,
            gap_ms,
            first_navigation_ms,
        })
}

/// 1–6 candidates in creation order, with their Location arrival times and
/// equalities. See [`FoldBurst`].
pub fn fold_burst() -> impl Strategy<Value = FoldBurst> {
    vec(burst_candidate(), 1..=6).prop_map(|candidates| FoldBurst { candidates })
}

/// A burst and a failure schedule over the commands it can cause (a few
/// `CreateTab`, `ActivateTab` and `CloseWindow` per candidate).
pub fn fold_burst_with_faults() -> impl Strategy<Value = (FoldBurst, FoldFaults)> {
    fold_burst().prop_flat_map(|burst| {
        let commands = 3 * burst.len() as u64;
        (Just(burst), failure_schedule(commands))
    })
}

/// A failure schedule over the first eight commands of each kind: failed tab
/// creations (tab window late or never, delayed registration), unconfirmed
/// activations, targets that close, and candidates that ignore `CloseWindow`.
pub fn fold_failure_schedule() -> impl Strategy<Value = FoldFaults> {
    fold_faults_strategy()
}
