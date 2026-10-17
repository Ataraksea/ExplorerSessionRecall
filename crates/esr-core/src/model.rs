//! Session data model and validation: `Snapshot`, `WindowRec`, `TabRec`,
//! `LocationId`, `WideString`, `Placement`, `SessionDoc`, canonical location
//! keys, and `validate`.
//!
//! Requirements 2.1, 8.3, 8.4, 11.4 (see design.md, "Data Models").

use std::borrow::Cow;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Most Explorer windows a snapshot may hold (Req 2.1, 2.10).
pub const MAX_WINDOWS: usize = 100;
/// Most tabs a snapshot may hold in total (Req 2.1, 2.10).
pub const MAX_TABS_TOTAL: usize = 500;
/// Most tabs a single window may hold.
pub const MAX_TABS_PER_WINDOW: usize = 500;
/// Longest `WideString` in UTF-16 code units (Req 8.4).
pub const MAX_WIDE_LEN: usize = 32_767;
/// Highest permitted count of consecutive failed restoration attempts.
pub const MAX_FAILED_ATTEMPTS: u8 = 3;

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

/// UTF-16 code units preserved verbatim, including unpaired surrogates,
/// up to 32,767 units (Req 8.4). No case folding, no normalization.
/// Equality compares code units exactly.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WideString(pub Vec<u16>);

impl WideString {
    pub fn new(units: Vec<u16>) -> Self {
        Self(units)
    }

    pub fn units(&self) -> &[u16] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Lossy conversion for logs and diagnostics (unpaired surrogates → U+FFFD).
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
}

impl From<&str> for WideString {
    fn from(s: &str) -> Self {
        Self(s.encode_utf16().collect())
    }
}

impl fmt::Debug for WideString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "W{:?}", self.to_string_lossy())
    }
}

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

/// Volume type of a file-system location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Volume {
    Fixed,
    Removable,
    /// Mapped network drive; the path keeps the drive letter (Req 8.7).
    Mapped,
    Unc,
}

/// Kind of a non-file-system Shell_Location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShellKind {
    ThisPc,
    Home,
    Gallery,
    RecycleBin,
    Network,
    Library,
    ControlPanel,
    PortableDevice,
    /// A saved search: a `.search-ms` (or `.searchConnector-ms`) file that
    /// Explorer browses as a folder of results. Like a search it is never
    /// redirected to an ancestor folder; a missing file is omitted (Req 9.2).
    SavedSearch,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LocationKind {
    FileSystem(Volume),
    ZipFolder,
    Shell(ShellKind),
    Search,
}

/// Top-level discriminant of a [`LocationKind`], used by the canonical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KindTag {
    FileSystem,
    ZipFolder,
    Shell,
    Search,
}

impl LocationKind {
    pub fn tag(self) -> KindTag {
        match self {
            LocationKind::FileSystem(_) => KindTag::FileSystem,
            LocationKind::ZipFolder => KindTag::ZipFolder,
            LocationKind::Shell(_) => KindTag::Shell,
            LocationKind::Search => KindTag::Search,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SearchSpec {
    /// Exactly as entered (Req 8.5).
    pub query: WideString,
    pub scope: Box<LocationId>,
}

/// A Location_Identifier.
///
/// `PartialEq`/`Eq`/`Hash` use the [canonical key](LocationId::key): PIDL bytes
/// and the display name are not part of equality, because PIDLs embed
/// timestamps that change between captures. Use [`LocationId::identical`] for
/// a field-by-field comparison.
#[derive(Clone, Debug)]
pub struct LocationId {
    pub kind: LocationKind,
    /// `SHGDN_FORPARSING`, desktop-absolute.
    pub parsing_name: WideString,
    /// For UIA order matching and logs.
    pub display_name: WideString,
    /// Absolute ITEMIDLIST bytes (fallback binding).
    pub pidl: Vec<u8>,
    /// Present iff `kind == Search`.
    pub search: Option<SearchSpec>,
    /// Runtime only, never serialized and not part of equality: the folder
    /// lies on a case-sensitive directory, so [`LocationId::fold_key`] leaves
    /// its name unfolded (Req 14.20). Defaults to `false`.
    pub case_sensitive: bool,
}

/// Canonical location key (Req 8.3, 5.9, launch classification).
///
/// - `Search` with a spec → `(Search, query, key(scope))`
/// - otherwise → `(kind discriminant, parsing_name)`
///
/// [`LocationId::key`] borrows the exact code units; [`LocationId::fold_key`]
/// owns the normalized ones.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LocationKey<'a> {
    Plain { kind: KindTag, parsing_name: Cow<'a, [u16]> },
    Search { query: Cow<'a, [u16]>, scope: Box<LocationKey<'a>> },
}

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
/// The Win32 file namespace prefix `\\?\`.
const VERBATIM_PREFIX: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];

fn is_separator(unit: u16) -> bool {
    unit == BACKSLASH || unit == SLASH
}

/// Simple uppercase mapping of one UTF-16 unit: the single-character uppercase
/// of a BMP scalar value, otherwise the unit unchanged (surrogates, characters
/// whose uppercase is longer than one character or outside the BMP).
fn fold_unit(unit: u16) -> u16 {
    let Some(c) = char::from_u32(u32::from(unit)) else { return unit };
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u16::try_from(u32::from(u)).unwrap_or(unit),
        _ => unit,
    }
}

/// Normalizes a parsing name for [`LocationId::fold_key`]: removes a `\\?\`
/// prefix, removes trailing separators except a root's (a drive root `X:\` or
/// a share root `\\server\share\` keeps exactly one `\`), and, unless
/// `case_sensitive`, upper-cases it unit by unit.
fn fold_name(name: &[u16], case_sensitive: bool) -> Vec<u16> {
    let name = name.strip_prefix(&VERBATIM_PREFIX).unwrap_or(name);
    let trimmed_len = name.iter().rposition(|&u| !is_separator(u)).map_or(0, |i| i + 1);
    let (trimmed, had_separator) = (&name[..trimmed_len], trimmed_len < name.len());

    let is_drive = trimmed.len() == 2 && trimmed[1] == u16::from(b':') && {
        char::from_u32(u32::from(trimmed[0])).is_some_and(|c| c.is_ascii_alphabetic())
    };
    let is_share = trimmed.starts_with(&[BACKSLASH, BACKSLASH]) && {
        let mut parts = trimmed[2..].split(|&u| u == BACKSLASH);
        let named = |p: Option<&[u16]>| p.is_some_and(|p| !p.is_empty());
        named(parts.next()) && named(parts.next()) && parts.next().is_none()
    };

    let mut out: Vec<u16> =
        if case_sensitive { trimmed.to_vec() } else { trimmed.iter().map(|&u| fold_unit(u)).collect() };
    if (is_drive && had_separator) || is_share || (trimmed.is_empty() && had_separator) {
        out.push(BACKSLASH);
    }
    out
}

impl LocationId {
    pub fn key(&self) -> LocationKey<'_> {
        match (&self.kind, &self.search) {
            (LocationKind::Search, Some(spec)) => LocationKey::Search {
                query: Cow::Borrowed(spec.query.units()),
                scope: Box::new(spec.scope.key()),
            },
            // A Search kind without a spec is invalid; fall back to the plain key.
            _ => LocationKey::Plain {
                kind: self.kind.tag(),
                parsing_name: Cow::Borrowed(self.parsing_name.units()),
            },
        }
    }

    /// Canonical key for Tab_Folding comparisons (Req 14.20): [`key`](Self::key)
    /// with the parsing name (and a search's scope, recursively) case-folded
    /// per UTF-16 unit with the simple uppercase mapping, a `\\?\` prefix
    /// removed and trailing separators removed except a root's. A search's
    /// query text stays exact, and a name is left unfolded when
    /// `case_sensitive` is true.
    pub fn fold_key(&self) -> LocationKey<'_> {
        match (&self.kind, &self.search) {
            (LocationKind::Search, Some(spec)) => LocationKey::Search {
                query: Cow::Borrowed(spec.query.units()),
                scope: Box::new(spec.scope.fold_key()),
            },
            _ => LocationKey::Plain {
                kind: self.kind.tag(),
                parsing_name: Cow::Owned(fold_name(self.parsing_name.units(), self.case_sensitive)),
            },
        }
    }

    /// Exact field-by-field comparison, including PIDL bytes and display names
    /// (but not the runtime-only `case_sensitive` flag).
    pub fn identical(&self, other: &LocationId) -> bool {
        self.kind == other.kind
            && self.parsing_name == other.parsing_name
            && self.display_name == other.display_name
            && self.pidl == other.pidl
            && match (&self.search, &other.search) {
                (None, None) => true,
                (Some(a), Some(b)) => a.query == b.query && a.scope.identical(&b.scope),
                _ => false,
            }
    }
}

impl PartialEq for LocationId {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for LocationId {}

impl Hash for LocationId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

// ---------------------------------------------------------------------------
// Geometry and placement
// ---------------------------------------------------------------------------

/// Screen coordinates in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i64 {
        i64::from(self.right) - i64::from(self.left)
    }

    pub fn height(&self) -> i64 {
        i64::from(self.bottom) - i64::from(self.top)
    }

    /// `right > left` and `bottom > top`.
    pub fn is_positive(&self) -> bool {
        self.width() > 0 && self.height() > 0
    }

    pub fn area(&self) -> i64 {
        if self.is_positive() {
            self.width() * self.height()
        } else {
            0
        }
    }

    /// Intersection, or `None` when the rects do not overlap with positive area.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let r = Rect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        r.is_positive().then_some(r)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShowState {
    Normal,
    Maximized,
    Minimized { restore_maximized: bool },
}

/// The display a window was on at save time.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DisplayRef {
    pub device: String,
    pub monitor: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    pub state: ShowState,
    /// Screen coords, physical px; right > left, bottom > top.
    pub restored: Rect,
    pub display: DisplayRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Display {
    pub device: String,
    pub monitor: Rect,
    pub work: Rect,
    pub primary: bool,
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// File-local window id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TabRec {
    pub location: LocationId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WindowRec {
    pub id: WindowId,
    /// Left-to-right, len >= 1.
    pub tabs: Vec<TabRec>,
    /// Index into `tabs`.
    pub active: usize,
    pub placement: Placement,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Snapshot {
    /// Order irrelevant; ordering is `activation`.
    pub windows: Vec<WindowRec>,
    /// Most recent first; permutation of window ids.
    pub activation: Vec<WindowId>,
}

impl Snapshot {
    pub fn tab_count(&self) -> usize {
        self.windows.iter().map(|w| w.tabs.len()).sum()
    }

    pub fn window(&self, id: WindowId) -> Option<&WindowRec> {
        self.windows.iter().find(|w| w.id == id)
    }
}

// ---------------------------------------------------------------------------
// Session document
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Pending,
    Consumed,
}

/// Why a snapshot was written (Req 13.1). A `Checkpoint` found pending at the
/// next start is reported as "abrupt end or power loss".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SaveCause {
    Checkpoint,
    LastWindowClosed,
    LastTabClosed,
    ExplorerTerminated,
    SignOut,
    Shutdown,
    Restart,
    ExitThroughSettings,
}

/// Local wall-clock time with millisecond precision and UTC offset,
/// e.g. `2025-01-31T09:15:02.123+01:00`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub millis: u16,
    /// Offset from UTC in minutes (local = UTC + offset).
    pub offset_minutes: i16,
}

impl Timestamp {
    /// Calendar and range check (year 1..=9999, offset within ±23:59).
    pub fn is_valid(&self) -> bool {
        let leap = (self.year.is_multiple_of(4) && !self.year.is_multiple_of(100)) || self.year.is_multiple_of(400);
        let days_in_month = match self.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return false,
        };
        (1..=9999).contains(&self.year)
            && (1..=days_in_month).contains(&self.day)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
            && self.millis < 1000
            && self.offset_minutes.unsigned_abs() < 24 * 60
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionDoc {
    /// Session_File_Format version.
    pub version: u32,
    pub status: Status,
    /// 0..=3.
    pub failed_attempts: u8,
    pub saved_at: Timestamp,
    pub cause: SaveCause,
    pub snapshot: Snapshot,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Which string of a location is too long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringField {
    ParsingName,
    DisplayName,
    SearchQuery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    TooManyWindows { count: usize },
    TooManyTabs { count: usize },
    NoTabs { window: WindowId },
    TooManyTabsInWindow { window: WindowId, count: usize },
    ActiveOutOfRange { window: WindowId, active: usize, tabs: usize },
    DuplicateWindowId { window: WindowId },
    /// `activation` is not a permutation of the window ids.
    ActivationNotPermutation,
    NonPositiveRect { window: WindowId },
    StringTooLong { window: WindowId, tab: usize, field: StringField, len: usize },
    /// `kind == Search` does not match the presence of `search`.
    SearchMismatch { window: WindowId, tab: usize },
    TooManyFailedAttempts { count: u8 },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ValidationError::*;
        match self {
            TooManyWindows { count } => write!(f, "{count} windows exceed the limit of {MAX_WINDOWS}"),
            TooManyTabs { count } => write!(f, "{count} tabs exceed the limit of {MAX_TABS_TOTAL}"),
            NoTabs { window } => write!(f, "window {} has no tabs", window.0),
            TooManyTabsInWindow { window, count } => {
                write!(f, "window {} has {count} tabs (limit {MAX_TABS_PER_WINDOW})", window.0)
            }
            ActiveOutOfRange { window, active, tabs } => {
                write!(f, "window {} active tab {active} is out of range (0..{tabs})", window.0)
            }
            DuplicateWindowId { window } => write!(f, "window id {} is duplicated", window.0),
            ActivationNotPermutation => {
                write!(f, "activation order does not list every window exactly once")
            }
            NonPositiveRect { window } => {
                write!(f, "window {} restored rect has non-positive size", window.0)
            }
            StringTooLong { window, tab, field, len } => write!(
                f,
                "window {} tab {tab} {field:?} has {len} UTF-16 units (limit {MAX_WIDE_LEN})",
                window.0
            ),
            SearchMismatch { window, tab } => write!(
                f,
                "window {} tab {tab}: search spec must be present exactly for search locations",
                window.0
            ),
            TooManyFailedAttempts { count } => {
                write!(f, "failed attempts {count} exceed {MAX_FAILED_ATTEMPTS}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Checks every snapshot validity rule from design.md (Req 2.1, 11.4).
pub fn validate(snapshot: &Snapshot) -> Result<(), ValidationError> {
    let windows = &snapshot.windows;
    if windows.len() > MAX_WINDOWS {
        return Err(ValidationError::TooManyWindows { count: windows.len() });
    }
    let total = snapshot.tab_count();
    if total > MAX_TABS_TOTAL {
        return Err(ValidationError::TooManyTabs { count: total });
    }

    let mut ids = std::collections::BTreeSet::new();
    for w in windows {
        if !ids.insert(w.id) {
            return Err(ValidationError::DuplicateWindowId { window: w.id });
        }
        if w.tabs.is_empty() {
            return Err(ValidationError::NoTabs { window: w.id });
        }
        if w.tabs.len() > MAX_TABS_PER_WINDOW {
            return Err(ValidationError::TooManyTabsInWindow { window: w.id, count: w.tabs.len() });
        }
        if w.active >= w.tabs.len() {
            return Err(ValidationError::ActiveOutOfRange {
                window: w.id,
                active: w.active,
                tabs: w.tabs.len(),
            });
        }
        if !w.placement.restored.is_positive() {
            return Err(ValidationError::NonPositiveRect { window: w.id });
        }
        for (i, tab) in w.tabs.iter().enumerate() {
            validate_location(&tab.location, w.id, i)?;
        }
    }

    // Permutation: same length, every entry is a known id, no repeats.
    if snapshot.activation.len() != ids.len() {
        return Err(ValidationError::ActivationNotPermutation);
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in &snapshot.activation {
        if !ids.contains(id) || !seen.insert(*id) {
            return Err(ValidationError::ActivationNotPermutation);
        }
    }
    Ok(())
}

/// [`validate`] plus the document-level rule `failed_attempts ≤ 3`.
pub fn validate_doc(doc: &SessionDoc) -> Result<(), ValidationError> {
    if doc.failed_attempts > MAX_FAILED_ATTEMPTS {
        return Err(ValidationError::TooManyFailedAttempts { count: doc.failed_attempts });
    }
    validate(&doc.snapshot)
}

fn validate_location(loc: &LocationId, window: WindowId, tab: usize) -> Result<(), ValidationError> {
    let check = |s: &WideString, field| {
        if s.len() > MAX_WIDE_LEN {
            Err(ValidationError::StringTooLong { window, tab, field, len: s.len() })
        } else {
            Ok(())
        }
    };
    check(&loc.parsing_name, StringField::ParsingName)?;
    check(&loc.display_name, StringField::DisplayName)?;
    match (loc.kind == LocationKind::Search, &loc.search) {
        (true, Some(spec)) => {
            check(&spec.query, StringField::SearchQuery)?;
            validate_location(&spec.scope, window, tab)
        }
        (false, None) => Ok(()),
        _ => Err(ValidationError::SearchMismatch { window, tab }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(kind: LocationKind, path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: path.into(),
            display_name: path.into(),
            pidl: vec![1, 2, 3],
            search: None,
        }
    }

    fn fs(path: &str) -> LocationId {
        loc(LocationKind::FileSystem(Volume::Fixed), path)
    }

    fn search(query: &str, scope: LocationId) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::Search,
            parsing_name: "search-ms:x".into(),
            display_name: "Search".into(),
            pidl: vec![],
            search: Some(SearchSpec { query: query.into(), scope: Box::new(scope) }),
        }
    }

    fn placement() -> Placement {
        let r = Rect { left: -100, top: 0, right: 700, bottom: 500 };
        Placement {
            state: ShowState::Normal,
            restored: r,
            display: DisplayRef { device: r"\\.\DISPLAY1".into(), monitor: r },
        }
    }

    fn window(id: u32, tabs: Vec<LocationId>, active: usize) -> WindowRec {
        WindowRec {
            id: WindowId(id),
            tabs: tabs.into_iter().map(|location| TabRec { location }).collect(),
            active,
            placement: placement(),
        }
    }

    fn snap(windows: Vec<WindowRec>, activation: &[u32]) -> Snapshot {
        Snapshot { windows, activation: activation.iter().map(|&i| WindowId(i)).collect() }
    }

    fn valid() -> Snapshot {
        snap(
            vec![
                window(1, vec![fs(r"C:\a"), fs(r"C:\b")], 1),
                window(7, vec![search("q", fs(r"D:\"))], 0),
            ],
            &[7, 1],
        )
    }

    #[test]
    fn equality_ignores_pidl_and_display_name() {
        let a = fs(r"C:\Users");
        let mut b = a.clone();
        b.pidl = vec![9, 9];
        b.display_name = "Users".into();
        assert_eq!(a, b);
        assert!(!a.identical(&b));
    }

    #[test]
    fn equality_is_exact_on_code_units() {
        // Case and normalization form are significant (Req 8.4).
        assert_ne!(fs(r"C:\a"), fs(r"C:\A"));
        assert_ne!(fs("C:\\\u{e9}"), fs("C:\\e\u{301}"));
        let mut lone = fs("x");
        lone.parsing_name.0.push(0xD800);
        let mut other = fs("x");
        other.parsing_name.0.push(0xDC00);
        assert_ne!(lone, other);
    }

    #[test]
    fn equality_uses_kind_discriminant() {
        assert_ne!(fs(r"C:\a"), loc(LocationKind::ZipFolder, r"C:\a"));
        assert_eq!(fs(r"Z:\a"), loc(LocationKind::FileSystem(Volume::Mapped), r"Z:\a"));
    }

    #[test]
    fn search_equality_uses_query_and_scope() {
        let a = search("q", fs(r"C:\a"));
        let mut b = search("q", fs(r"C:\a"));
        b.parsing_name = "search-ms:different".into();
        assert_eq!(a, b);
        assert_ne!(a, search("Q", fs(r"C:\a")));
        assert_ne!(a, search("q", fs(r"C:\b")));
    }

    fn folded(name: &str) -> Vec<u16> {
        match fs(name).fold_key() {
            LocationKey::Plain { parsing_name, .. } => parsing_name.into_owned(),
            other => panic!("unexpected key {other:?}"),
        }
    }

    #[test]
    fn fold_key_ignores_letter_case_prefix_and_trailing_separators() {
        assert_eq!(fs(r"C:\Users\Ünï").fold_key(), fs(r"c:\users\ünï").fold_key());
        assert_eq!(fs(r"C:\Users").fold_key(), fs(r"\\?\C:\Users").fold_key());
        assert_eq!(fs(r"C:\Users").fold_key(), fs(r"C:\Users\\").fold_key());
        assert_eq!(fs(r"C:\Users").fold_key(), fs(r"c:\USERS/").fold_key());
        assert_eq!(folded(r"\\?\c:\a\b\"), r"C:\A\B".encode_utf16().collect::<Vec<_>>());
        // The plain key stays exact.
        assert_ne!(fs(r"C:\a").key(), fs(r"C:\A").key());
    }

    #[test]
    fn fold_key_keeps_a_roots_separator() {
        for root in [r"C:\", r"c:\\", r"\\?\C:\"] {
            assert_eq!(folded(root), r"C:\".encode_utf16().collect::<Vec<_>>(), "{root}");
        }
        assert_eq!(folded(r"\\srv\share"), r"\\SRV\SHARE\".encode_utf16().collect::<Vec<_>>());
        assert_eq!(folded(r"\\srv\share\"), folded(r"\\srv\share"));
        assert_eq!(folded(r"\\srv\share\dir\"), r"\\SRV\SHARE\DIR".encode_utf16().collect::<Vec<_>>());
        assert_eq!(folded(r"C:"), r"C:".encode_utf16().collect::<Vec<_>>());
        // Idempotent on a normalized form.
        for name in [r"C:\", r"C:\A\B", r"\\SRV\SHARE\", "::{20D04FE0}"] {
            assert_eq!(folded(name), name.encode_utf16().collect::<Vec<_>>());
        }
    }

    #[test]
    fn fold_key_folds_per_utf16_unit() {
        // Single-unit simple uppercase mapping only: ß has no single-character
        // uppercase, and astral characters are not folded.
        assert_eq!(folded("ß"), "ß".encode_utf16().collect::<Vec<_>>());
        assert_eq!(folded("\u{10428}"), "\u{10428}".encode_utf16().collect::<Vec<_>>());
        let mut lone = fs("x");
        lone.parsing_name.0.push(0xD800);
        assert_ne!(lone.fold_key(), fs("x").fold_key());
    }

    #[test]
    fn fold_key_respects_case_sensitive_kind_and_search_rules() {
        let mut a = fs(r"C:\a");
        let mut b = fs(r"C:\A");
        assert_eq!(a.fold_key(), b.fold_key());
        a.case_sensitive = true;
        b.case_sensitive = true;
        assert_ne!(a.fold_key(), b.fold_key());
        // The flag is not part of equality.
        assert_eq!(a, fs(r"C:\a"));
        // The prefix and trailing separators go even when case is kept.
        let mut prefixed = fs(r"\\?\C:\a\");
        prefixed.case_sensitive = true;
        assert_eq!(a.fold_key(), prefixed.fold_key());
        assert_ne!(fs(r"C:\a").fold_key(), loc(LocationKind::ZipFolder, r"C:\a").fold_key());

        // A search folds its scope and keeps its query exact.
        let s = search("Query", fs(r"C:\Dir\"));
        assert_eq!(s.fold_key(), search("Query", fs(r"\\?\c:\dir")).fold_key());
        assert_ne!(s.fold_key(), search("query", fs(r"C:\Dir")).fold_key());
        assert_ne!(s.fold_key(), search("Query", fs(r"C:\Other")).fold_key());
        let mut cs_scope = fs(r"C:\Dir");
        cs_scope.case_sensitive = true;
        assert_ne!(s.fold_key(), search("Query", cs_scope).fold_key());
    }

    #[test]
    fn valid_snapshot_passes() {
        assert_eq!(validate(&valid()), Ok(()));
        assert_eq!(validate(&Snapshot::default()), Ok(()));
    }

    #[test]
    fn window_and_tab_rules() {
        let mut s = valid();
        s.windows[0].tabs.clear();
        s.windows[0].active = 0;
        assert_eq!(validate(&s), Err(ValidationError::NoTabs { window: WindowId(1) }));

        let mut s = valid();
        s.windows[0].active = 2;
        assert!(matches!(validate(&s), Err(ValidationError::ActiveOutOfRange { .. })));

        let mut s = valid();
        s.windows[1].id = WindowId(1);
        s.activation = vec![WindowId(1), WindowId(1)];
        assert_eq!(validate(&s), Err(ValidationError::DuplicateWindowId { window: WindowId(1) }));

        let mut s = valid();
        s.windows[0].placement.restored.right = s.windows[0].placement.restored.left;
        assert_eq!(validate(&s), Err(ValidationError::NonPositiveRect { window: WindowId(1) }));
    }

    #[test]
    fn activation_must_be_permutation() {
        for act in [&[7][..], &[7, 1, 1], &[7, 7], &[1, 2], &[]] {
            let mut s = valid();
            s.activation = act.iter().map(|&i| WindowId(i)).collect();
            assert_eq!(validate(&s), Err(ValidationError::ActivationNotPermutation), "{act:?}");
        }
    }

    #[test]
    fn caps() {
        let ws: Vec<_> = (0..101).map(|i| window(i, vec![fs("C:\\")], 0)).collect();
        let ids: Vec<u32> = (0..101).collect();
        assert_eq!(validate(&snap(ws, &ids)), Err(ValidationError::TooManyWindows { count: 101 }));

        let ws = vec![window(0, vec![fs("C:\\"); 500], 0)];
        assert_eq!(validate(&snap(ws, &[0])), Ok(()));
        let ws = vec![window(0, vec![fs("C:\\"); 300], 0), window(1, vec![fs("C:\\"); 201], 0)];
        assert_eq!(validate(&snap(ws, &[0, 1])), Err(ValidationError::TooManyTabs { count: 501 }));
    }

    #[test]
    fn string_length_limit() {
        let mut ok = fs("");
        ok.parsing_name = WideString(vec![b'a' as u16; MAX_WIDE_LEN]);
        assert_eq!(validate(&snap(vec![window(0, vec![ok.clone()], 0)], &[0])), Ok(()));

        let mut long = ok.clone();
        long.parsing_name.0.push(b'a' as u16);
        assert!(matches!(
            validate(&snap(vec![window(0, vec![long.clone()], 0)], &[0])),
            Err(ValidationError::StringTooLong { field: StringField::ParsingName, .. })
        ));
        // Nested in a search scope.
        assert!(matches!(
            validate(&snap(vec![window(0, vec![search("q", long)], 0)], &[0])),
            Err(ValidationError::StringTooLong { .. })
        ));
        let mut q = search("q", fs("C:\\"));
        q.search.as_mut().unwrap().query = WideString(vec![0xD800; MAX_WIDE_LEN + 1]);
        assert!(matches!(
            validate(&snap(vec![window(0, vec![q], 0)], &[0])),
            Err(ValidationError::StringTooLong { field: StringField::SearchQuery, .. })
        ));
    }

    #[test]
    fn search_iff_spec() {
        let mut a = fs("C:\\");
        a.search = search("q", fs("C:\\")).search;
        let mut b = search("q", fs("C:\\"));
        b.search = None;
        for l in [a, b] {
            assert_eq!(
                validate(&snap(vec![window(0, vec![l], 0)], &[0])),
                Err(ValidationError::SearchMismatch { window: WindowId(0), tab: 0 })
            );
        }
    }

    #[test]
    fn failed_attempts_limit() {
        let mut doc = SessionDoc {
            version: 1,
            status: Status::Pending,
            failed_attempts: 3,
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
            cause: SaveCause::Checkpoint,
            snapshot: valid(),
        };
        assert!(doc.saved_at.is_valid());
        assert_eq!(validate_doc(&doc), Ok(()));
        doc.failed_attempts = 4;
        assert_eq!(validate_doc(&doc), Err(ValidationError::TooManyFailedAttempts { count: 4 }));
    }
}
