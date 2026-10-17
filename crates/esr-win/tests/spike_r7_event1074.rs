//! Spike R7: can a standard user read the newest System-log event 1074 (User32)
//! within 300 ms, and does it tell restart vs. power-off? (Req 13.1)
//!
//! Read-only: this queries the System channel and never writes to any event log
//! or to the registry.
//!
//! Run manually:
//! `cargo test -p esr-win --test spike_r7_event1074 -- --ignored --nocapture`
//!
//! The pure parsing helpers below (`parse_event1074_xml`, `classify_shutdown_type`)
//! are what the later `SessionEndListener` should reuse; they are covered by
//! normal (non-ignored) unit tests.

/// Kind of shutdown recorded by a User32 1074 event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownKind {
    Restart,
    PowerOff,
    /// The shutdown-type field was missing or not recognised (e.g. an unknown
    /// UI language). Callers fall back to "Windows shutdown".
    Unknown,
}

/// Fields of interest from a rendered 1074 event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event1074 {
    /// `System/TimeCreated/@SystemTime` (UTC, ISO 8601), used by the caller to
    /// reject a stale event left over from an earlier shutdown.
    pub time_created: Option<String>,
    /// Raw `param5` (shutdown type) text as logged; it is localized.
    pub shutdown_type: Option<String>,
    pub kind: ShutdownKind,
}

/// Maps the (localized) `param5` text of event 1074 to a kind. Matching is
/// case-insensitive; unknown languages yield `Unknown`.
pub fn classify_shutdown_type(text: &str) -> ShutdownKind {
    let t = text.trim().to_lowercase();
    const RESTART: &[&str] = &[
        "restart",
        "neu starten",
        "neustart",
        "redémarrer",
        "reiniciar",
        "riavvia",
    ];
    const POWER_OFF: &[&str] = &[
        "power off",
        "shutdown",
        "shut down",
        "ausschalten",
        "herunterfahren",
        "arrêter",
        "apagar",
        "arresta",
    ];
    if RESTART.iter().any(|k| t.contains(k)) {
        ShutdownKind::Restart
    } else if POWER_OFF.iter().any(|k| t.contains(k)) {
        ShutdownKind::PowerOff
    } else {
        ShutdownKind::Unknown
    }
}

/// Parses the XML produced by `EvtRender(EvtRenderEventXml)` for a 1074 event.
/// Returns `None` if the XML is not a User32 1074 event.
pub fn parse_event1074_xml(xml: &str) -> Option<Event1074> {
    let event_id = element_text(xml, "EventID")?;
    if event_id.trim() != "1074" {
        return None;
    }
    let time_created = find_tag(xml, "TimeCreated").and_then(|tag| attr_value(tag, "SystemTime"));
    // Prefer the named field; fall back to the fifth positional <Data>.
    let shutdown_type = data_named(xml, "param5").or_else(|| data_nth(xml, 4));
    let kind = shutdown_type
        .as_deref()
        .map_or(ShutdownKind::Unknown, classify_shutdown_type);
    Some(Event1074 {
        time_created,
        shutdown_type,
        kind,
    })
}

/// Returns the opening tag text (`<Name ...>` or `<Name .../>`) of the first `<Name` element.
fn find_tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = xml;
    loop {
        let start = rest.find(&format!("<{name}"))?;
        let after = &rest[start + 1 + name.len()..];
        // Make sure the match is the whole element name (not a prefix).
        if after.starts_with([' ', '>', '/', '\t', '\r', '\n']) {
            let end = after.find('>')?;
            return Some(&rest[start..start + 1 + name.len() + end + 1]);
        }
        rest = after;
    }
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        let needle = format!("{attr}={quote}");
        if let Some(i) = tag.find(&needle) {
            let v = &tag[i + needle.len()..];
            let end = v.find(quote)?;
            return Some(unescape(&v[..end]));
        }
    }
    None
}

fn element_text(xml: &str, name: &str) -> Option<String> {
    let tag = find_tag(xml, name)?;
    if tag.ends_with("/>") {
        return Some(String::new());
    }
    let start = xml.find(tag)? + tag.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(unescape(&xml[start..start + end]))
}

/// Iterates over `(name attribute, text)` of every `<Data>` element.
fn data_elements(xml: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(tag) = find_tag(rest, "Data") {
        let pos = rest.find(tag).unwrap_or(0) + tag.len();
        let name = attr_value(tag, "Name");
        if tag.ends_with("/>") {
            out.push((name, String::new()));
            rest = &rest[pos..];
            continue;
        }
        let Some(end) = rest[pos..].find("</Data>") else {
            break;
        };
        out.push((name, unescape(&rest[pos..pos + end])));
        rest = &rest[pos + end + "</Data>".len()..];
    }
    out
}

fn data_named(xml: &str, name: &str) -> Option<String> {
    data_elements(xml)
        .into_iter()
        .find(|(n, _)| n.as_deref() == Some(name))
        .map(|(_, v)| v)
}

fn data_nth(xml: &str, index: usize) -> Option<String> {
    data_elements(xml).into_iter().nth(index).map(|(_, v)| v)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    const SAMPLE_RESTART: &str = r#"<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='User32' Guid='{b0aa8734-56f7-41cc-b2f4-de228e98b946}' EventSourceName='User32'/><EventID Qualifiers='32768'>1074</EventID><Version>0</Version><Level>4</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8080000000000000</Keywords><TimeCreated SystemTime='2025-01-02T03:04:05.6789012Z'/><EventRecordID>123</EventRecordID><Channel>System</Channel><Computer>HOST</Computer></System><EventData><Data Name='param1'>C:\Windows\System32\RuntimeBroker.exe (HOST)</Data><Data Name='param2'>HOST</Data><Data Name='param3'>Other (Unplanned)</Data><Data Name='param4'>0x0</Data><Data Name='param5'>restart</Data><Data Name='param6'></Data><Data Name='param7'>HOST\user</Data></EventData></Event>"#;

    #[test]
    fn parses_restart_and_time() {
        let e = parse_event1074_xml(SAMPLE_RESTART).expect("1074 event");
        assert_eq!(e.kind, ShutdownKind::Restart);
        assert_eq!(e.shutdown_type.as_deref(), Some("restart"));
        assert_eq!(
            e.time_created.as_deref(),
            Some("2025-01-02T03:04:05.6789012Z")
        );
    }

    #[test]
    fn parses_power_off_and_positional_data() {
        let named = SAMPLE_RESTART.replace(">restart<", ">power off<");
        assert_eq!(
            parse_event1074_xml(&named).unwrap().kind,
            ShutdownKind::PowerOff
        );
        // Unnamed <Data> elements: the fifth one is the shutdown type.
        let positional = "<Event><System><EventID>1074</EventID></System><EventData>\
            <Data>a</Data><Data>b</Data><Data>c</Data><Data>0x0</Data><Data>Ausschalten</Data></EventData></Event>";
        assert_eq!(
            parse_event1074_xml(positional).unwrap().kind,
            ShutdownKind::PowerOff
        );
    }

    #[test]
    fn unknown_and_non_1074() {
        assert_eq!(classify_shutdown_type("xyz"), ShutdownKind::Unknown);
        assert_eq!(
            classify_shutdown_type(" Neu starten "),
            ShutdownKind::Restart
        );
        let other = SAMPLE_RESTART.replace(">1074<", ">6006<");
        assert_eq!(parse_event1074_xml(&other), None);
        let empty = SAMPLE_RESTART.replace(
            "<Data Name='param5'>restart</Data>",
            "<Data Name='param5'/>",
        );
        assert_eq!(
            parse_event1074_xml(&empty).unwrap().kind,
            ShutdownKind::Unknown
        );
    }
}

#[cfg(windows)]
mod spike {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::EventLog::{
        EVT_HANDLE, EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection,
        EvtRender, EvtRenderEventXml,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::w;

    const BUDGET: Duration = Duration::from_millis(300);

    fn is_elevated() -> Option<bool> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
            let mut elevation = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let r = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            let _ = CloseHandle(token);
            r.ok()?;
            Some(elevation.TokenIsElevated != 0)
        }
    }

    fn render_xml(event: EVT_HANDLE) -> windows::core::Result<String> {
        unsafe {
            let mut used = 0u32;
            let mut props = 0u32;
            // First call sizes the buffer (fails with ERROR_INSUFFICIENT_BUFFER).
            if let Err(e) = EvtRender(
                None,
                event,
                EvtRenderEventXml.0,
                0,
                None,
                &mut used,
                &mut props,
            ) && e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
            {
                return Err(e);
            }
            let mut buf = vec![0u16; (used as usize).div_ceil(2)];
            EvtRender(
                None,
                event,
                EvtRenderEventXml.0,
                (buf.len() * 2) as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut used,
                &mut props,
            )?;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok(String::from_utf16_lossy(&buf[..len]))
        }
    }

    /// Queries the newest User32 1074 event from the System channel within `BUDGET`.
    fn newest_1074() -> windows::core::Result<Option<String>> {
        let start = Instant::now();
        unsafe {
            let query = EvtQuery(
                None,
                w!("System"),
                w!("*[System[Provider[@Name='User32'] and (EventID=1074)]]"),
                EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
            )?;
            let remaining = BUDGET.saturating_sub(start.elapsed()).as_millis() as u32;
            let mut events = [0isize; 1];
            let mut returned = 0u32;
            let next = EvtNext(query, &mut events, remaining.max(1), 0, &mut returned);
            let result = match next {
                Ok(()) if returned > 0 => {
                    let ev = EVT_HANDLE(events[0]);
                    let xml = render_xml(ev);
                    let _ = EvtClose(ev);
                    xml.map(Some)
                }
                Ok(()) => Ok(None),
                // ERROR_NO_MORE_ITEMS (259): no matching event.
                Err(e) if e.code().0 as u32 == 0x8007_0103 => Ok(None),
                Err(e) => Err(e),
            };
            let _ = EvtClose(query);
            result
        }
    }

    #[test]
    #[ignore = "spike: reads the local System event log"]
    fn spike_r7_read_newest_event1074_within_budget() {
        println!("R7: elevated = {:?}", is_elevated());
        let start = Instant::now();
        let result = newest_1074();
        let elapsed = start.elapsed();
        println!(
            "R7: elapsed = {:.1} ms (budget {} ms)",
            elapsed.as_secs_f64() * 1000.0,
            BUDGET.as_millis()
        );
        match &result {
            Ok(Some(xml)) => {
                let parsed = parse_event1074_xml(xml);
                println!("R7: event found; parsed = {parsed:?}");
            }
            Ok(None) => println!("R7: no User32 1074 event in the System log"),
            Err(e) => println!("R7: query failed: {e} ({:#x})", e.code().0),
        }
        let xml = result.expect("System log must be readable");
        assert!(
            elapsed <= BUDGET,
            "query took {elapsed:?}, over the {BUDGET:?} budget"
        );
        if let Some(xml) = xml {
            assert!(
                parse_event1074_xml(&xml).is_some(),
                "rendered XML is not a 1074 event: {xml}"
            );
        }
    }
}
