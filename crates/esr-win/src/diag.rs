//! Diagnostics for the timing tests (task 16.3): an in-process trace of
//! timestamped notes, and switches that turn individual subsystems off to
//! measure their cost.
//!
//! Everything here is off by default. The integration tests run the runtime
//! in-process and turn the trace and the switches on; the product never does.
//! A disabled trace costs one relaxed atomic load per note.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

static TRACE: AtomicBool = AtomicBool::new(false);
static NOTES: Mutex<Vec<(Instant, String)>> = Mutex::new(Vec::new());

/// Subsystems the timing tests can switch off ([`set_disabled`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Switch {
    /// The per-Explorer-PID WinEvent hooks (`LOCATIONCHANGE`, minimize,
    /// move/size end).
    PidHooks = 1,
    /// The global WinEvent hooks (create/destroy/show/hide, foreground).
    GlobalHooks = 2,
    /// The per-tab `DWebBrowserEvents2` sinks.
    BrowserSinks = 4,
    /// The UIA client of the tab strip reader (reads, watches, events).
    UiaReader = 8,
    /// Window scans (the scanner ignores change signals).
    Scanner = 16,
    /// Only the tab strip reader's `StructureChanged` subscriptions.
    UiaWatch = 32,
}

static DISABLED: AtomicU32 = AtomicU32::new(0);

/// Turns the trace on or off; turning it on clears earlier notes.
pub fn set_trace(on: bool) {
    if on {
        NOTES.lock().unwrap_or_else(PoisonError::into_inner).clear();
        COUNTS.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
    TRACE.store(on, Ordering::SeqCst);
}

/// Whether the trace is on.
#[inline]
pub fn tracing() -> bool {
    TRACE.load(Ordering::Relaxed)
}

/// Records a note (formatted only while the trace is on).
#[inline]
pub fn note(f: impl FnOnce() -> String) {
    if tracing() {
        let at = Instant::now();
        let text = f();
        NOTES.lock().unwrap_or_else(PoisonError::into_inner).push((at, text));
    }
}

/// Removes and returns the notes recorded so far.
pub fn take_notes() -> Vec<(Instant, String)> {
    std::mem::take(&mut *NOTES.lock().unwrap_or_else(PoisonError::into_inner))
}

static COUNTS: Mutex<Vec<(i32, u64)>> = Mutex::new(Vec::new());

/// Counts one occurrence of `key` (while the trace is on).
#[inline]
pub fn count(key: i32) {
    if tracing() {
        let mut c = COUNTS.lock().unwrap_or_else(PoisonError::into_inner);
        match c.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => c.push((key, 1)),
        }
    }
}

/// Removes and returns the counts recorded so far.
pub fn take_counts() -> Vec<(i32, u64)> {
    std::mem::take(&mut *COUNTS.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Disables (or re-enables) `switch`.
pub fn set_disabled(switch: Switch, disabled: bool) {
    if disabled {
        DISABLED.fetch_or(switch as u32, Ordering::SeqCst);
    } else {
        DISABLED.fetch_and(!(switch as u32), Ordering::SeqCst);
    }
}

/// Whether `switch` is disabled.
#[inline]
pub fn disabled(switch: Switch) -> bool {
    DISABLED.load(Ordering::Relaxed) & switch as u32 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_recorded_only_while_tracing_and_switches_toggle() {
        set_trace(false);
        note(|| "dropped".into());
        set_trace(true);
        note(|| "kept".into());
        set_trace(false);
        let notes = take_notes();
        assert!(notes.iter().any(|(_, n)| n == "kept"));
        assert!(!notes.iter().any(|(_, n)| n == "dropped"));

        assert!(!disabled(Switch::Scanner));
        set_disabled(Switch::Scanner, true);
        assert!(disabled(Switch::Scanner) && !disabled(Switch::PidHooks));
        set_disabled(Switch::Scanner, false);
        assert!(!disabled(Switch::Scanner));
    }
}
