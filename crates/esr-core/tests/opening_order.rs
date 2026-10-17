//! Unit tests for `fold::OpeningOrder` (Req 14.8, 14.9, 14.11, 14.12).

use std::collections::BTreeSet;

use esr_core::engine::{DesktopView, WinKey};
use esr_core::fold::OpeningOrder;

fn w(n: u64) -> WinKey {
    WinKey { hwnd: n, seq: n }
}

fn known(wins: &[u64]) -> DesktopView {
    DesktopView::Known(wins.iter().map(|&n| w(n)).collect::<BTreeSet<_>>())
}

fn ids(order: &OpeningOrder) -> Vec<u64> {
    order.windows().map(|k| k.hwnd).collect()
}

fn order_of(wins: &[u64]) -> OpeningOrder {
    let mut o = OpeningOrder::new();
    for &n in wins {
        o.append(w(n));
    }
    o
}

// --- appends (Req 14.8) ----------------------------------------------------

#[test]
fn new_order_is_empty() {
    let o = OpeningOrder::new();
    assert!(o.is_empty());
    assert_eq!(o.len(), 0);
    assert!(o.to_vec().is_empty());
}

#[test]
fn append_keeps_opening_time_order() {
    let o = order_of(&[3, 1, 2]);
    assert_eq!(ids(&o), vec![3, 1, 2]);
    assert_eq!(o.len(), 3);
    assert!(o.contains(w(1)));
    assert!(!o.contains(w(9)));
}

#[test]
fn append_of_known_window_keeps_its_position() {
    let mut o = order_of(&[1, 2, 3]);
    o.append(w(1));
    assert_eq!(ids(&o), vec![1, 2, 3]);
}

#[test]
fn same_hwnd_with_new_seq_is_a_different_window() {
    let mut o = OpeningOrder::new();
    o.append(WinKey { hwnd: 5, seq: 1 });
    o.append(WinKey { hwnd: 5, seq: 2 });
    assert_eq!(o.len(), 2);
}

// --- left-open candidate (Req 14.15) ----------------------------------------

#[test]
fn reserved_candidate_is_not_visible_in_the_order() {
    let mut o = order_of(&[1]);
    o.reserve(w(2));
    assert_eq!(ids(&o), vec![1]);
    assert!(!o.contains(w(2)));
    assert_eq!(o.len(), 1);
}

#[test]
fn revealed_candidate_takes_its_first_signal_position() {
    let mut o = order_of(&[1]);
    o.reserve(w(2)); // candidate's first signal
    o.append(w(3)); // a later window appears while it is a candidate
    o.append(w(4));
    o.reveal(w(2)); // left open
    assert_eq!(ids(&o), vec![1, 2, 3, 4]);
    assert!(o.contains(w(2)));
}

#[test]
fn reveal_without_reservation_appends() {
    let mut o = order_of(&[1]);
    o.reveal(w(2));
    assert_eq!(ids(&o), vec![1, 2]);
}

#[test]
fn reserve_of_known_window_keeps_it_as_it_was() {
    let mut o = order_of(&[1, 2]);
    o.reserve(w(1));
    assert_eq!(ids(&o), vec![1, 2]);
}

#[test]
fn folded_candidate_gives_up_its_position() {
    let mut o = order_of(&[1]);
    o.reserve(w(2));
    o.remove(w(2));
    o.reveal(w(2)); // no reservation any more: appended at the end
    o.append(w(3));
    assert_eq!(ids(&o), vec![1, 2, 3]);

    let mut o = order_of(&[1]);
    o.reserve(w(2));
    o.remove(w(2));
    o.append(w(3));
    assert_eq!(ids(&o), vec![1, 3]);
}

// --- removal (Req 14.9) ------------------------------------------------------

#[test]
fn remove_drops_a_destroyed_window_and_keeps_the_rest_in_order() {
    let mut o = order_of(&[1, 2, 3, 4]);
    o.remove(w(2));
    assert_eq!(ids(&o), vec![1, 3, 4]);
    assert!(!o.contains(w(2)));
}

#[test]
fn remove_of_unknown_window_is_ignored() {
    let mut o = order_of(&[1, 2]);
    o.remove(w(9));
    assert_eq!(ids(&o), vec![1, 2]);
}

// --- start and resume (Req 14.8) ---------------------------------------------

#[test]
fn reset_ranks_open_windows_in_activation_order() {
    let mut o = OpeningOrder::new();
    o.reset_to_activation(&[w(7), w(3), w(5)]);
    assert_eq!(ids(&o), vec![7, 3, 5]);
}

#[test]
fn reset_discards_earlier_positions_including_reserved_ones() {
    let mut o = order_of(&[1, 2]);
    o.reserve(w(3));
    o.reset_to_activation(&[w(2), w(9)]);
    assert_eq!(ids(&o), vec![2, 9]);
    // The reserved position of 3 is gone: revealing appends at the end.
    o.reveal(w(3));
    assert_eq!(ids(&o), vec![2, 9, 3]);
}

#[test]
fn windows_opened_after_reset_follow_the_activation_ranked_ones() {
    let mut o = OpeningOrder::new();
    o.reset_to_activation(&[w(4), w(2)]);
    o.append(w(8));
    assert_eq!(ids(&o), vec![4, 2, 8]);
}

#[test]
fn reset_with_no_windows_empties_the_order() {
    let mut o = order_of(&[1]);
    o.reset_to_activation(&[]);
    assert!(o.is_empty());
}

// --- Restoration_Completion (Req 14.8) ---------------------------------------

#[test]
fn completion_moves_trigger_then_restored_ahead_of_windows_opened_meanwhile() {
    // T was open first, N1/N2 opened during the restoration, R1/R2 were
    // restored (and so opened after N1 here).
    let mut o = order_of(&[10, 21, 22, 31, 32]);
    // 10 = trigger, 21/22 = opened meanwhile, 31/32 = restored.
    o.restoration_completed(Some(w(10)), &[w(32), w(31)]);
    assert_eq!(ids(&o), vec![10, 32, 31, 21, 22]);
}

#[test]
fn completion_without_trigger_moves_only_restored_windows() {
    let mut o = order_of(&[31, 21, 32]);
    o.restoration_completed(None, &[w(31), w(32)]);
    assert_eq!(ids(&o), vec![31, 32, 21]);
}

#[test]
fn completion_keeps_windows_that_ranked_before_all_moved_windows() {
    // P opened before the restoration began and ranks before every moved one.
    let mut o = order_of(&[1, 2, 3, 4, 5]);
    // 1 = P, 2 = trigger, 3 = N1, 4 = R1, 5 = N2
    o.restoration_completed(Some(w(2)), &[w(4)]);
    assert_eq!(ids(&o), vec![1, 2, 4, 3, 5]);
}

#[test]
fn completion_trigger_comes_before_a_restored_window_listed_first() {
    let mut o = order_of(&[1, 2, 3]);
    o.restoration_completed(Some(w(3)), &[w(2), w(3)]);
    // The trigger is listed once, in front, then the other restored window.
    assert_eq!(ids(&o), vec![1, 3, 2]);
}

#[test]
fn completion_skips_windows_not_in_the_order() {
    let mut o = order_of(&[1, 2]);
    o.restoration_completed(Some(w(9)), &[w(8), w(2)]);
    assert_eq!(ids(&o), vec![1, 2]);

    let mut o = order_of(&[1, 2]);
    o.restoration_completed(Some(w(9)), &[w(8)]);
    assert_eq!(ids(&o), vec![1, 2]);
}

#[test]
fn completion_keeps_reserved_candidate_positions() {
    let mut o = order_of(&[1]); // 1 is the trigger window
    o.reserve(w(2)); // candidate during the restoration
    o.append(w(3)); // opened meanwhile
    o.append(w(4)); // restored
    o.restoration_completed(Some(w(1)), &[w(4)]);
    // Trigger first, then the restored window, ahead of the window opened
    // meanwhile.
    assert_eq!(ids(&o), vec![1, 4, 3]);
    o.reveal(w(2));
    // The candidate's position is still ahead of 3 and behind the moved windows.
    assert_eq!(ids(&o), vec![1, 4, 2, 3]);
}

#[test]
fn completion_is_idempotent() {
    let mut o = order_of(&[1, 2, 3, 4]);
    o.restoration_completed(Some(w(3)), &[w(4)]);
    let once = o.clone();
    o.restoration_completed(Some(w(3)), &[w(4)]);
    assert_eq!(o, once);
}

// --- Primary_Window (Req 14.9, 14.11, 14.12) ----------------------------------

#[test]
fn primary_is_first_window_of_the_order_on_the_desktop() {
    let o = order_of(&[1, 2, 3, 4]);
    assert_eq!(o.primary(&known(&[3, 4])), Some(w(3)));
    assert_eq!(o.primary(&known(&[2, 4])), Some(w(2)));
    assert_eq!(o.primary(&known(&[1, 2, 3, 4])), Some(w(1)));
}

#[test]
fn primary_ignores_view_windows_that_are_not_in_the_order() {
    let o = order_of(&[1, 2]);
    assert_eq!(o.primary(&known(&[9, 2])), Some(w(2)));
    assert_eq!(o.primary(&known(&[9])), None);
}

#[test]
fn primary_is_none_for_an_empty_known_view_or_empty_order() {
    assert_eq!(order_of(&[1, 2]).primary(&known(&[])), None);
    assert_eq!(OpeningOrder::new().primary(&known(&[1])), None);
    assert_eq!(OpeningOrder::new().primary(&DesktopView::Unknown), None);
}

#[test]
fn primary_with_unknown_view_is_the_first_window_overall() {
    let o = order_of(&[5, 1, 2]);
    assert_eq!(o.primary(&DesktopView::Unknown), Some(w(5)));
}

#[test]
fn all_desktop_window_is_primary_on_every_desktop_it_leads() {
    // Window 1 is shown on all desktops, so it is in the Known view of both.
    let o = order_of(&[1, 2, 3]);
    let desktop_a = known(&[1, 2]);
    let desktop_b = known(&[1, 3]);
    assert_eq!(o.primary(&desktop_a), Some(w(1)));
    assert_eq!(o.primary(&desktop_b), Some(w(1)));
}

#[test]
fn all_desktop_window_later_in_order_does_not_displace_earlier_local_window() {
    let o = order_of(&[2, 1, 3]);
    // 1 is on all desktops; 2 is only on desktop A.
    assert_eq!(o.primary(&known(&[1, 2])), Some(w(2)));
    assert_eq!(o.primary(&known(&[1, 3])), Some(w(1)));
}

#[test]
fn primary_moves_to_the_next_window_when_it_closes() {
    let mut o = order_of(&[1, 2, 3]);
    let view = known(&[1, 2, 3]);
    assert_eq!(o.primary(&view), Some(w(1)));
    o.remove(w(1));
    assert_eq!(o.primary(&view), Some(w(2)));
}

#[test]
fn primary_follows_a_window_moved_to_another_desktop() {
    let o = order_of(&[1, 2, 3]);
    assert_eq!(o.primary(&known(&[1, 2, 3])), Some(w(1)));
    // 1 moved away: the view no longer lists it.
    assert_eq!(o.primary(&known(&[2, 3])), Some(w(2)));
}

#[test]
fn candidates_are_never_primary() {
    let mut o = order_of(&[2]);
    o.reserve(w(1)); // first signal earlier in the order than 2? it is last
    assert_eq!(o.primary(&known(&[1])), None);
    assert_eq!(o.primary(&DesktopView::Unknown), Some(w(2)));
    o.reveal(w(1));
    assert_eq!(o.primary(&known(&[1])), Some(w(1)));
}

#[test]
fn on_desktop_lists_windows_in_order() {
    let o = order_of(&[4, 1, 3, 2]);
    let view = known(&[1, 2, 4]);
    assert_eq!(o.on_desktop(&view).map(|k| k.hwnd).collect::<Vec<_>>(), vec![4, 1, 2]);
    assert_eq!(
        o.on_desktop(&DesktopView::Unknown).map(|k| k.hwnd).collect::<Vec<_>>(),
        vec![4, 1, 3, 2]
    );
}
