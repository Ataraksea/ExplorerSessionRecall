//! Snapshot_Serializer: `serialize(&SessionDoc) -> String` following the
//! version 1 EBNF in design.md ("Session_File_Format (version 1)").
//!
//! Requirement 11.3.
//!
//! The output is UTF-8 with LF line endings and no BOM. Every line is a
//! keyword followed by `key=value` fields in a fixed order, separated by single
//! spaces, so the output is deterministic. Enum values use the tokens in
//! [`super::tokens`]; `WideString`s are escaped with [`escape_into`]; PIDLs
//! are padded base64; integers are decimal (negative values with a leading
//! `-`). Example:
//!
//! ```text
//! ESR-SESSION version=1
//! status pending
//! failed-attempts 0
//! saved-at 2025-01-31T09:15:02.123+01:00
//! cause shutdown
//! window id=7 state=normal restored=0,0,800,600 display=\\.\DISPLAY1 monitor=0,0,1920,1080
//! tab kind=fs-fixed parse=C:\Users display=Users pidl=AQID
//! active 0
//! end-window
//! activation 7
//! end-session
//! ```
//!
//! # Details beyond the EBNF
//!
//! * The header always declares [`FORMAT_VERSION`]: the serializer only
//!   writes the current grammar, whatever `doc.version` says. A parsed
//!   document therefore round-trips exactly when `doc.version ==
//!   FORMAT_VERSION`.
//! * `display=` on the `window` line is `DisplayRef::device`, escaped like
//!   every other value; `monitor=` is `DisplayRef::monitor`.
//! * `saved-at` is `YYYY-MM-DDTHH:MM:SS.mmm±HH:MM`: a four-digit zero-padded
//!   year, milliseconds in three digits, and the UTC offset with an explicit
//!   sign (`+00:00` for UTC, `-` only for negative offsets).
//! * Windows are written in `Snapshot::windows` order and tabs left to right.
//! * The optional search group of a `tab` line is written exactly when the
//!   location has a `SearchSpec` (validity requires that to coincide with
//!   `kind=search`). The scope is written as a full location with every key
//!   prefixed `scope-`. If the scope is itself a search, its own search group
//!   follows with the prefix applied again (`scope-query=`, then
//!   `scope-scope-kind=` …), so arbitrarily nested scopes are representable.
//!   For a plain scope this is exactly the EBNF's `query= scope-kind=
//!   scope-parse= scope-display= scope-pidl=` group.

use std::fmt::Write as _;

use super::escape::{base64_encode, escape_into};
use super::tokens::{cause_token, kind_token, show_state_token, status_token};
use crate::model::{LocationId, Placement, Rect, SessionDoc, Timestamp, WindowRec};

/// The Session_File_Format version this serializer writes and the newest
/// version the parser accepts.
pub const FORMAT_VERSION: u32 = 1;

/// Serializes `doc` into Session_File_Format version [`FORMAT_VERSION`].
///
/// The document is written as-is; validity (Req 11.4) is the caller's
/// responsibility and is re-checked by the parser.
pub fn serialize(doc: &SessionDoc) -> String {
    let mut out = String::with_capacity(256 + doc.snapshot.tab_count() * 160);

    // `write!` into a `String` cannot fail.
    let _ = writeln!(out, "ESR-SESSION version={FORMAT_VERSION}");
    let _ = writeln!(out, "status {}", status_token(doc.status));
    let _ = writeln!(out, "failed-attempts {}", doc.failed_attempts);
    out.push_str("saved-at ");
    write_timestamp(&mut out, &doc.saved_at);
    out.push('\n');
    let _ = writeln!(out, "cause {}", cause_token(doc.cause));

    for window in &doc.snapshot.windows {
        write_window(&mut out, window);
    }

    out.push_str("activation");
    for id in &doc.snapshot.activation {
        let _ = write!(out, " {}", id.0);
    }
    out.push('\n');
    out.push_str("end-session\n");
    out
}

fn write_window(out: &mut String, window: &WindowRec) {
    let _ = write!(out, "window id={} ", window.id.0);
    write_placement(out, &window.placement);
    out.push('\n');
    for tab in &window.tabs {
        out.push_str("tab ");
        write_location(out, &tab.location, "");
        out.push('\n');
    }
    let _ = writeln!(out, "active {}", window.active);
    out.push_str("end-window\n");
}

fn write_placement(out: &mut String, p: &Placement) {
    let _ = write!(out, "state={} restored=", show_state_token(p.state));
    write_rect(out, &p.restored);
    out.push_str(" display=");
    let device: Vec<u16> = p.display.device.encode_utf16().collect();
    escape_into(out, &device);
    out.push_str(" monitor=");
    write_rect(out, &p.display.monitor);
}

fn write_rect(out: &mut String, r: &Rect) {
    let _ = write!(out, "{},{},{},{}", r.left, r.top, r.right, r.bottom);
}

/// Writes the fields of `loc` with every key prefixed by `prefix`; the
/// first field has no leading space.
fn write_location(out: &mut String, loc: &LocationId, prefix: &str) {
    let _ = write!(out, "{prefix}kind={} {prefix}parse=", kind_token(loc.kind));
    escape_into(out, loc.parsing_name.units());
    let _ = write!(out, " {prefix}display=");
    escape_into(out, loc.display_name.units());
    let _ = write!(out, " {prefix}pidl={}", base64_encode(&loc.pidl));
    if let Some(spec) = &loc.search {
        let _ = write!(out, " {prefix}query=");
        escape_into(out, spec.query.units());
        out.push(' ');
        let scope_prefix = format!("{prefix}scope-");
        write_location(out, &spec.scope, &scope_prefix);
    }
}

/// `2025-01-31T09:15:02.123+01:00`.
fn write_timestamp(out: &mut String, t: &Timestamp) {
    let sign = if t.offset_minutes < 0 { '-' } else { '+' };
    let off = t.offset_minutes.unsigned_abs();
    let _ = write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{sign}{:02}:{:02}",
        t.year,
        t.month,
        t.day,
        t.hour,
        t.minute,
        t.second,
        t.millis,
        off / 60,
        off % 60,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn loc(kind: LocationKind, parse: &str, display: &str, pidl: &[u8]) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind,
            parsing_name: parse.into(),
            display_name: display.into(),
            pidl: pidl.to_vec(),
            search: None,
        }
    }

    fn ts(offset_minutes: i16) -> Timestamp {
        Timestamp {
            year: 2025,
            month: 1,
            day: 31,
            hour: 9,
            minute: 15,
            second: 2,
            millis: 123,
            offset_minutes,
        }
    }

    fn doc(snapshot: Snapshot) -> SessionDoc {
        SessionDoc {
            version: FORMAT_VERSION,
            status: Status::Pending,
            failed_attempts: 2,
            saved_at: ts(60),
            cause: SaveCause::LastWindowClosed,
            snapshot,
        }
    }

    fn small_doc() -> SessionDoc {
        let fs = loc(
            LocationKind::FileSystem(Volume::Fixed),
            r"C:\Program Files",
            "Program Files",
            &[1, 2, 3],
        );
        let mut search = loc(LocationKind::Search, "search-ms:x", "Search Results", &[]);
        search.search = Some(SearchSpec {
            query: "a=b".into(),
            scope: Box::new(loc(
                LocationKind::FileSystem(Volume::Unc),
                r"\\server\share",
                "share",
                &[0xFF],
            )),
        });
        let window = WindowRec {
            id: WindowId(7),
            tabs: vec![TabRec { location: fs }, TabRec { location: search }],
            active: 1,
            placement: Placement {
                state: ShowState::Minimized { restore_maximized: true },
                restored: Rect { left: -100, top: 20, right: 700, bottom: 520 },
                display: DisplayRef {
                    device: r"\\.\DISPLAY2".into(),
                    monitor: Rect { left: -1920, top: 0, right: 0, bottom: 1080 },
                },
            },
        };
        doc(Snapshot { windows: vec![window], activation: vec![WindowId(7)] })
    }

    #[test]
    fn golden_small_document() {
        let expected = concat!(
            "ESR-SESSION version=1\n",
            "status pending\n",
            "failed-attempts 2\n",
            "saved-at 2025-01-31T09:15:02.123+01:00\n",
            "cause last-window-closed\n",
            r"window id=7 state=minimized-max restored=-100,20,700,520 display=\\.\DISPLAY2 monitor=-1920,0,0,1080",
            "\n",
            r"tab kind=fs-fixed parse=C:\Program%20Files display=Program%20Files pidl=AQID",
            "\n",
            r"tab kind=search parse=search-ms:x display=Search%20Results pidl= query=a%3Db scope-kind=fs-unc scope-parse=\\server\share scope-display=share scope-pidl=/w==",
            "\n",
            "active 1\n",
            "end-window\n",
            "activation 7\n",
            "end-session\n",
        );
        let text = serialize(&small_doc());
        assert_eq!(text, expected);
        assert!(!text.starts_with('\u{FEFF}'));
        assert!(!text.contains('\r'));
    }

    #[test]
    fn empty_snapshot() {
        let mut d = doc(Snapshot::default());
        d.status = Status::Consumed;
        d.failed_attempts = 0;
        d.cause = SaveCause::Checkpoint;
        d.saved_at = ts(0);
        assert_eq!(
            serialize(&d),
            concat!(
                "ESR-SESSION version=1\n",
                "status consumed\n",
                "failed-attempts 0\n",
                "saved-at 2025-01-31T09:15:02.123+00:00\n",
                "cause checkpoint\n",
                "activation\n",
                "end-session\n",
            )
        );
    }

    #[test]
    fn timestamp_padding_and_negative_offset() {
        let t = Timestamp {
            year: 7,
            month: 2,
            day: 3,
            hour: 4,
            minute: 5,
            second: 6,
            millis: 7,
            offset_minutes: -(9 * 60 + 30),
        };
        let mut s = String::new();
        write_timestamp(&mut s, &t);
        assert_eq!(s, "0007-02-03T04:05:06.007-09:30");
        let mut s = String::new();
        write_timestamp(&mut s, &Timestamp { offset_minutes: 1439, ..t });
        assert_eq!(s, "0007-02-03T04:05:06.007+23:59");
    }

    #[test]
    fn window_order_activation_and_states() {
        let w = |id: u32, state: ShowState| WindowRec {
            id: WindowId(id),
            tabs: vec![TabRec {
                location: loc(LocationKind::Shell(ShellKind::ThisPc), "::{X}", "This PC", &[]),
            }],
            active: 0,
            placement: Placement {
                state,
                restored: Rect { left: 0, top: 0, right: 10, bottom: 10 },
                display: DisplayRef { device: "D 1".into(), monitor: Rect::default() },
            },
        };
        let d = doc(Snapshot {
            windows: vec![w(3, ShowState::Normal), w(1, ShowState::Maximized), w(2, ShowState::Minimized { restore_maximized: false })],
            activation: vec![WindowId(2), WindowId(3), WindowId(1)],
        });
        let text = serialize(&d);
        let window_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("window ")).collect();
        assert_eq!(
            window_lines,
            [
                "window id=3 state=normal restored=0,0,10,10 display=D%201 monitor=0,0,0,0",
                "window id=1 state=maximized restored=0,0,10,10 display=D%201 monitor=0,0,0,0",
                "window id=2 state=minimized restored=0,0,10,10 display=D%201 monitor=0,0,0,0",
            ]
        );
        assert!(text.contains("\nactivation 2 3 1\nend-session\n"));
        assert!(text.contains("\ntab kind=shell-this-pc parse=::{X} display=This%20PC pidl=\n"));
    }

    #[test]
    fn nested_search_scope_repeats_prefix() {
        let inner = {
            let mut l = loc(LocationKind::Search, "s2", "S2", &[]);
            l.search = Some(SearchSpec {
                query: "q2".into(),
                scope: Box::new(loc(LocationKind::ZipFolder, "z", "Z", &[0])),
            });
            l
        };
        let mut outer = loc(LocationKind::Search, "s1", "S1", &[]);
        outer.search = Some(SearchSpec { query: "q1".into(), scope: Box::new(inner) });
        let mut s = String::new();
        write_location(&mut s, &outer, "");
        assert_eq!(
            s,
            "kind=search parse=s1 display=S1 pidl= query=q1 \
             scope-kind=search scope-parse=s2 scope-display=S2 scope-pidl= scope-query=q2 \
             scope-scope-kind=zip scope-scope-parse=z scope-scope-display=Z scope-scope-pidl=AA=="
        );
    }

    #[test]
    fn lone_surrogates_and_controls_are_escaped() {
        let mut l = loc(LocationKind::FileSystem(Volume::Fixed), "", "", &[]);
        l.parsing_name = WideString(vec![u16::from(b'a'), 0xD800, u16::from(b'\n')]);
        let mut s = String::new();
        write_location(&mut s, &l, "");
        assert_eq!(s, "kind=fs-fixed parse=a%uD800%0A display= pidl=");
    }
}
