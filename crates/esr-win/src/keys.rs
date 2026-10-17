//! Stable keys for Explorer window and tab HWNDs.
//!
//! The core identifies a live window by [`WinKey`] and a live tab by
//! [`TabKey`]: the HWND value plus a creation sequence number, so that a
//! reused HWND value (Windows recycles them) never aliases a window or tab the
//! core still remembers. Every adapter that turns an HWND into a key (the
//! `WindowEventSource`, the window scanner, the shell window source) goes
//! through one shared [`KeyRegistry`] per kind, so they all agree on the key
//! of a given window:
//!
//! - [`KeyRegistry::key_of`] returns the current key of a live HWND, minting a
//!   new one (with a fresh sequence number) the first time the HWND is seen
//!   or the first time after it was [destroyed](KeyRegistry::destroyed).
//! - [`KeyRegistry::destroyed`] retires the HWND's key. Seeing the same HWND
//!   value again (`EVENT_OBJECT_CREATE` of a new window that reuses it, or a
//!   scan) therefore yields a new sequence number.
//!
//! Sequence numbers come from one counter per registry, starting at 1, so a
//! key is never issued twice.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use esr_core::engine::{TabKey, WinKey};
use windows::Win32::Foundation::HWND;

/// A key made of an HWND value and a creation sequence number.
pub trait HwndKey: Copy + Eq + Debug {
    fn from_parts(hwnd: u64, seq: u64) -> Self;
    fn hwnd(&self) -> u64;
    fn seq(&self) -> u64;
}

impl HwndKey for WinKey {
    fn from_parts(hwnd: u64, seq: u64) -> Self {
        WinKey { hwnd, seq }
    }
    fn hwnd(&self) -> u64 {
        self.hwnd
    }
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl HwndKey for TabKey {
    fn from_parts(hwnd: u64, seq: u64) -> Self {
        TabKey { hwnd, seq }
    }
    fn hwnd(&self) -> u64 {
        self.hwnd
    }
    fn seq(&self) -> u64 {
        self.seq
    }
}

/// The HWND value stored in keys. Every adapter must use this conversion.
pub fn hwnd_value(hwnd: HWND) -> u64 {
    hwnd.0 as usize as u64
}

/// The HWND for a key's HWND value (inverse of [`hwnd_value`]).
pub fn hwnd_of(value: u64) -> HWND {
    HWND(value as usize as *mut c_void)
}

/// HWND value → current key, for one kind of key (windows or tabs).
#[derive(Debug)]
pub struct KeyRegistry<K> {
    live: HashMap<u64, K>,
    last_seq: u64,
}

impl<K: HwndKey> Default for KeyRegistry<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: HwndKey> KeyRegistry<K> {
    pub fn new() -> Self {
        Self {
            live: HashMap::new(),
            last_seq: 0,
        }
    }

    /// The current key of `hwnd`, if it is live.
    pub fn get(&self, hwnd: u64) -> Option<K> {
        self.live.get(&hwnd).copied()
    }

    /// The current key of `hwnd`, minting one if the HWND is not live.
    pub fn key_of(&mut self, hwnd: u64) -> K {
        if let Some(key) = self.live.get(&hwnd) {
            return *key;
        }
        self.last_seq += 1;
        let key = K::from_parts(hwnd, self.last_seq);
        self.live.insert(hwnd, key);
        key
    }

    /// Retires `hwnd`'s key and returns it (`None` if the HWND was not live).
    pub fn destroyed(&mut self, hwnd: u64) -> Option<K> {
        self.live.remove(&hwnd)
    }

    /// Every live key, oldest first.
    pub fn live(&self) -> Vec<K> {
        let mut keys: Vec<K> = self.live.values().copied().collect();
        keys.sort_by_key(HwndKey::seq);
        keys
    }

    /// Number of live keys.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

/// A [`KeyRegistry`] shared between adapters (cheap to clone).
#[derive(Debug)]
pub struct SharedKeys<K>(Arc<Mutex<KeyRegistry<K>>>);

impl<K> Clone for SharedKeys<K> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<K: HwndKey> Default for SharedKeys<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: HwndKey> SharedKeys<K> {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(KeyRegistry::new())))
    }

    fn lock(&self) -> MutexGuard<'_, KeyRegistry<K>> {
        // The registry stays consistent even if a holder panicked.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// See [`KeyRegistry::get`].
    pub fn get(&self, hwnd: u64) -> Option<K> {
        self.lock().get(hwnd)
    }

    /// See [`KeyRegistry::key_of`].
    pub fn key_of(&self, hwnd: u64) -> K {
        self.lock().key_of(hwnd)
    }

    /// See [`KeyRegistry::destroyed`].
    pub fn destroyed(&self, hwnd: u64) -> Option<K> {
        self.lock().destroyed(hwnd)
    }

    /// See [`KeyRegistry::live`].
    pub fn live(&self) -> Vec<K> {
        self.lock().live()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// Keys of top-level `CabinetWClass` windows.
pub type WindowKeys = SharedKeys<WinKey>;
/// Keys of `ShellTabWindowClass` tab windows.
pub type TabKeys = SharedKeys<TabKey>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keys_are_stable_while_live() {
        let mut reg = KeyRegistry::<WinKey>::new();
        let a = reg.key_of(0x10);
        assert_eq!(reg.key_of(0x10), a);
        assert_eq!(reg.get(0x10), Some(a));
        assert_eq!(a.hwnd, 0x10);
        let b = reg.key_of(0x20);
        assert_ne!(a, b);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn hwnd_reuse_after_destroy_gets_a_new_seq() {
        let mut reg = KeyRegistry::<WinKey>::new();
        let first = reg.key_of(0x10);
        assert_eq!(reg.destroyed(0x10), Some(first));
        assert_eq!(reg.get(0x10), None);
        assert!(reg.is_empty());
        let second = reg.key_of(0x10);
        assert_eq!(second.hwnd, first.hwnd);
        assert!(second.seq > first.seq, "{first:?} {second:?}");
    }

    #[test]
    fn live_lists_the_live_keys_oldest_first() {
        let keys = WindowKeys::new();
        let a = keys.key_of(0x30);
        let b = keys.key_of(0x10);
        let c = keys.key_of(0x20);
        keys.destroyed(0x10);
        assert_eq!(keys.live(), vec![a, c]);
        assert_ne!(b, keys.key_of(0x10));
    }

    #[test]
    fn destroying_an_unknown_hwnd_is_none() {
        let mut reg = KeyRegistry::<TabKey>::new();
        assert_eq!(reg.destroyed(0x30), None);
    }

    #[test]
    fn no_key_is_ever_issued_twice() {
        let mut reg = KeyRegistry::<TabKey>::new();
        let mut minted = HashSet::new();
        for round in 0..50u64 {
            let hwnd = round % 3;
            let was_live = reg.get(hwnd).is_some();
            let key = reg.key_of(hwnd);
            if !was_live {
                assert!(minted.insert(key), "re-issued {key:?}");
            }
            if round % 2 == 0 {
                reg.destroyed(hwnd);
            }
        }
    }

    #[test]
    fn window_and_tab_registries_are_independent() {
        let wins = WindowKeys::new();
        let tabs = TabKeys::new();
        let w = wins.key_of(0x40);
        let t = tabs.key_of(0x40);
        assert_eq!((w.hwnd, w.seq), (0x40, 1));
        assert_eq!((t.hwnd, t.seq), (0x40, 1));
        assert_eq!(tabs.destroyed(0x40), Some(t));
        assert_eq!(wins.get(0x40), Some(w));
    }

    #[test]
    fn shared_keys_agree_across_clones_and_threads() {
        let keys = WindowKeys::new();
        let other = keys.clone();
        let k = std::thread::spawn(move || other.key_of(0x50))
            .join()
            .unwrap();
        assert_eq!(keys.key_of(0x50), k);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn hwnd_value_round_trips() {
        for v in [1u64, 0x1234, 0x7FFF_FFFF, 0xFFFF_FFFF] {
            assert_eq!(hwnd_value(hwnd_of(v)), v);
        }
    }
}
