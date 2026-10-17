//! UIA helpers for the XAML tab strip, shared by the `TabStripReader` and the
//! navigator's UIA worker (task 16.3 findings).
//!
//! * **Scoped strip search.** The strip lives in the window's XAML island,
//!   a direct child HWND of the `CabinetWClass` window
//!   (`Microsoft.UI.Content.DesktopChildSiteBridge` on 22621). A descendant
//!   search from that child takes about 1 ms, while one from the top-level
//!   window also walks every tab's file view (80 ms for one tab, 0.8–2.4 s
//!   with 10–30 tabs, R9). [`search_roots`] lists the island first and the
//!   top-level window last as the fallback.
//! * **Every item, realized or not.** Right after tabs are created the strip
//!   exposes fewer child items than there are tabs (1 of 10 for ~150 ms),
//!   and an overflowing strip virtualises its items (17–24 of 30 exposed).
//!   The strip's `ItemContainerPattern` (`FindItemByProperty` iteration)
//!   returns every item in visual order in both cases, measured on 22621:
//!   10 of 10 while only 1 was exposed, 30 of 30 on an overflowing strip
//!   (unrealized items have empty names). [`all_items`] uses it.
//! * **Realize before use.** An unrealized item is realized
//!   (`VirtualizedItemPattern.Realize`) and scrolled into view
//!   (`ScrollItemPattern`) before it is selected or its close button is
//!   looked up ([`realize`]); `SelectionItemPattern.Select` then works on any
//!   position (checked at positions 1, 15, 28 of 30).
//! * **Explorer's tab count** is the number of `ShellTabWindowClass` children
//!   ([`tab_window_count`]); a strip "has caught up" when [`all_items`]
//!   returns exactly that many items.

use std::time::Duration;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationCondition, IUIAutomationElement, IUIAutomationItemContainerPattern,
    IUIAutomationScrollItemPattern, IUIAutomationSelectionItemPattern, IUIAutomationTreeWalker,
    IUIAutomationVirtualizedItemPattern, TreeScope_Descendants, UIA_ItemContainerPatternId,
    UIA_PROPERTY_ID, UIA_ScrollItemPatternId, UIA_SelectionItemPatternId,
    UIA_VirtualizedItemPatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{GW_CHILD, GW_HWNDNEXT, GetWindow};

use crate::shell_util::{SHELL_TAB_WINDOW_CLASS, window_class_name};

/// Class-name fragments of the child window hosting the XAML island (the
/// tab strip): WinAppSDK (`Microsoft.UI.Content.DesktopChildSiteBridge`),
/// system XAML (`Windows.UI.Composition.DesktopWindowContentBridge`), and
/// `DesktopWindowXamlSource` hosts.
pub const STRIP_HOST_CLASS_HINTS: &[&str] = &["DesktopChildSiteBridge", "ContentBridge", "Xaml"];

/// `UIA_E_ELEMENTNOTAVAILABLE`, returned transiently by `ElementFromHandle`.
pub const UIA_E_ELEMENTNOTAVAILABLE: i32 = 0x8004_0201_u32 as i32;
const ELEMENT_ATTEMPTS: u32 = 10;
const ELEMENT_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Upper bound on items iterated through `ItemContainerPattern`.
const MAX_ITEMS: usize = 4096;
/// Upper bound on child windows examined.
const MAX_CHILDREN: usize = 4096;

/// Whether a child window class looks like the XAML island host.
pub fn is_strip_host_class(class: &str) -> bool {
    STRIP_HOST_CLASS_HINTS.iter().any(|h| class.contains(h))
}

/// Direct child windows of `top` with their class names.
fn direct_children(top: HWND) -> Vec<(HWND, String)> {
    let mut out = Vec::new();
    // SAFETY: GetWindow tolerates stale HWNDs (it then fails and ends the walk).
    let mut child = unsafe { GetWindow(top, GW_CHILD) }.ok();
    while let Some(c) = child.filter(|c| !c.is_invalid()) {
        if out.len() >= MAX_CHILDREN {
            break;
        }
        out.push((c, window_class_name(c)));
        // SAFETY: as above.
        child = unsafe { GetWindow(c, GW_HWNDNEXT) }.ok();
    }
    out
}

/// Number of tabs Explorer hosts in `top` (its `ShellTabWindowClass` children).
pub fn tab_window_count(top: HWND) -> usize {
    direct_children(top).iter().filter(|(_, c)| c == SHELL_TAB_WINDOW_CLASS).count()
}

/// Where to search for the strip, cheapest first: the XAML island host
/// child(ren), then the top-level window itself (the slow fallback).
pub fn search_roots(top: HWND) -> Vec<HWND> {
    let mut roots: Vec<HWND> =
        direct_children(top).into_iter().filter(|(_, c)| is_strip_host_class(c)).map(|(h, _)| h).collect();
    roots.push(top);
    roots
}

/// `ElementFromHandle`, retried while UIA reports the element as not available.
pub fn element_from_handle(automation: &IUIAutomation, hwnd: HWND) -> windows::core::Result<IUIAutomationElement> {
    let mut attempt = 0;
    loop {
        // SAFETY: plain UIA call; a stale HWND just fails.
        match unsafe { automation.ElementFromHandle(hwnd) } {
            Ok(el) => return Ok(el),
            Err(err) if err.code().0 == UIA_E_ELEMENTNOTAVAILABLE && attempt + 1 < ELEMENT_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(ELEMENT_RETRY_DELAY);
            }
            Err(err) => return Err(err),
        }
    }
}

/// The tab strip of `top`: the parent of the first element matching
/// `strip_item` (a `ListViewItem` tab item), searched under each of
/// [`search_roots`] in turn. The last error is returned when none has one.
///
/// The direct route ([`find_strip_direct`]) is tried first: a handful of
/// UIA calls instead of a descendant search, which walks the island
/// element by element in this process and made dozens of calls into
/// Explorer's busy thread (task 16.3: 1–1.8 s, once 10 s, for a strip
/// searched while Explorer was still building tabs).
pub fn find_strip(
    automation: &IUIAutomation,
    walker: &IUIAutomationTreeWalker,
    strip_item: &IUIAutomationCondition,
    top: HWND,
) -> windows::core::Result<IUIAutomationElement> {
    // The island's tree is sometimes momentarily unavailable right after a
    // navigation (the route fails at once); the descendant search from the
    // top-level window then took ~0.9 s (task 16.3). Retry briefly first.
    for attempt in 0..DIRECT_ATTEMPTS {
        if let Some(strip) = find_strip_direct(automation, top) {
            return Ok(strip);
        }
        if attempt + 1 < DIRECT_ATTEMPTS {
            std::thread::sleep(backoff(attempt));
        }
    }
    crate::diag::note(|| format!("uia: direct strip route failed for {:#x}; searching", top.0 as usize));
    let mut last = None;
    for root in search_roots(top) {
        let found = element_from_handle(automation, root).and_then(|el| {
            // SAFETY: plain UIA calls on live interfaces.
            unsafe {
                let first = el.FindFirst(TreeScope_Descendants, strip_item)?;
                walker.GetParentElement(&first)
            }
        });
        match found {
            Ok(strip) => return Ok(strip),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL)))
}

/// AutomationId of the XAML `TabView`, a child of the island root (22621).
pub const TAB_VIEW_AUTOMATION_ID: &str = "TabView";
/// AutomationId of the tab strip (`ListView`), a child of the `TabView`.
pub const TAB_LIST_AUTOMATION_ID: &str = "TabListView";
/// Most siblings examined per level by [`find_strip_direct`].
const DIRECT_SIBLINGS: usize = 16;
/// Attempts of the direct route before the descendant search (backoff
/// 10, 20, 40, 80 ms in between: at most 150 ms).
const DIRECT_ATTEMPTS: u32 = 5;

/// The strip through its known place (measured on 22621): island root →
/// child with AutomationId `TabView` (child 0 of 8) → child with
/// AutomationId `TabListView` (child 0 of 2), walked in the raw view.
/// `None` when that structure is not found (the caller then searches).
pub fn find_strip_direct(automation: &IUIAutomation, top: HWND) -> Option<IUIAutomationElement> {
    // SAFETY: plain UIA calls on live interfaces.
    unsafe {
        let raw = automation.RawViewWalker().ok()?;
        let child_with_id = |parent: &IUIAutomationElement, id: &str| {
            let mut child = raw.GetFirstChildElement(parent).ok();
            for _ in 0..DIRECT_SIBLINGS {
                let c = child?;
                if c.CurrentAutomationId().is_ok_and(|a| a == id) {
                    return Some(c);
                }
                child = raw.GetNextSiblingElement(&c).ok();
            }
            None
        };
        search_roots(top).into_iter().filter(|r| *r != top).find_map(|root| {
            let el = element_from_handle(automation, root).ok()?;
            let tab_view = child_with_id(&el, TAB_VIEW_AUTOMATION_ID)?;
            child_with_id(&tab_view, TAB_LIST_AUTOMATION_ID)
        })
    }
}

/// Every item of `strip` in visual order, realized or not, through its
/// `ItemContainerPattern`. `None` when the strip has no such pattern (the
/// caller then falls back to the exposed child items).
pub fn all_items(strip: &IUIAutomationElement) -> Option<Vec<IUIAutomationElement>> {
    // SAFETY: plain UIA calls on live interfaces.
    unsafe {
        let pattern: IUIAutomationItemContainerPattern =
            strip.GetCurrentPatternAs(UIA_ItemContainerPatternId).ok()?;
        let any = VARIANT::default();
        let mut items: Vec<IUIAutomationElement> = Vec::new();
        while items.len() < MAX_ITEMS {
            // Property 0 matches every item; the iteration ends with an error
            // (no element) after the last one.
            match pattern.FindItemByProperty(items.last(), UIA_PROPERTY_ID(0), &any) {
                Ok(item) => items.push(item),
                Err(_) => break,
            }
        }
        Some(items)
    }
}

/// Realizes a virtualized item and scrolls it into view (best effort: items
/// without the patterns are already realized).
pub fn realize(item: &IUIAutomationElement) {
    // SAFETY: plain UIA calls on a live element.
    unsafe {
        if let Ok(v) =
            item.GetCurrentPatternAs::<IUIAutomationVirtualizedItemPattern>(UIA_VirtualizedItemPatternId)
        {
            let _ = v.Realize();
        }
        if let Ok(s) = item.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId) {
            let _ = s.ScrollIntoView();
        }
    }
}

/// `SelectionItemPattern.Select` on `item`.
pub fn select_item(item: &IUIAutomationElement) -> windows::core::Result<()> {
    // SAFETY: plain UIA calls on a live element.
    unsafe {
        item.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .and_then(|p| p.Select())
    }
}

/// `SelectionItemPattern.CurrentIsSelected` of `item`.
pub fn is_selected(item: &IUIAutomationElement) -> windows::core::Result<bool> {
    // SAFETY: plain UIA calls on a live element.
    unsafe {
        item.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .and_then(|p| p.CurrentIsSelected())
            .map(|b| b.as_bool())
    }
}

/// Backoff delays for waiting on a strip that lags behind tab creation:
/// 10, 20, 40, 80, then 100 ms.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((10u64 << attempt.min(4)).min(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_host_classes() {
        assert!(is_strip_host_class("Microsoft.UI.Content.DesktopChildSiteBridge"));
        assert!(is_strip_host_class("Windows.UI.Composition.DesktopWindowContentBridge"));
        assert!(!is_strip_host_class("ShellTabWindowClass"));
        assert!(!is_strip_host_class("TITLE_BAR_SCAFFOLDING_WINDOW_CLASS"));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let d: Vec<u64> = (0..7).map(|a| backoff(a).as_millis() as u64).collect();
        assert_eq!(d, vec![10, 20, 40, 80, 100, 100, 100]);
    }
}
