//! Stable lowercase tokens for the enums written to the Session_File_Format.
//!
//! Each enum has one table of `(value, token)` pairs that both the serializer
//! and the parser use, so the two directions cannot drift apart. Tokens are
//! part of the on-disk format: never rename one without a format migration.

use crate::model::{LocationKind, SaveCause, ShellKind, ShowState, Status, Volume};

/// `kind=` / `scope-kind=` tokens for every [`LocationKind`].
pub const LOCATION_KINDS: &[(LocationKind, &str)] = &[
    (LocationKind::FileSystem(Volume::Fixed), "fs-fixed"),
    (LocationKind::FileSystem(Volume::Removable), "fs-removable"),
    (LocationKind::FileSystem(Volume::Mapped), "fs-mapped"),
    (LocationKind::FileSystem(Volume::Unc), "fs-unc"),
    (LocationKind::ZipFolder, "zip"),
    (LocationKind::Shell(ShellKind::ThisPc), "shell-this-pc"),
    (LocationKind::Shell(ShellKind::Home), "shell-home"),
    (LocationKind::Shell(ShellKind::Gallery), "shell-gallery"),
    (LocationKind::Shell(ShellKind::RecycleBin), "shell-recycle-bin"),
    (LocationKind::Shell(ShellKind::Network), "shell-network"),
    (LocationKind::Shell(ShellKind::Library), "shell-library"),
    (LocationKind::Shell(ShellKind::ControlPanel), "shell-control-panel"),
    (LocationKind::Shell(ShellKind::PortableDevice), "shell-portable-device"),
    // Added within format version 1: older files never contain it, so they
    // parse unchanged.
    (LocationKind::Shell(ShellKind::SavedSearch), "shell-saved-search"),
    (LocationKind::Shell(ShellKind::Other), "shell-other"),
    (LocationKind::Search, "search"),
];

/// `status` line tokens.
pub const STATUSES: &[(Status, &str)] = &[(Status::Pending, "pending"), (Status::Consumed, "consumed")];

/// `cause` line tokens (Req 13.1). Unlike the Activity_Log, a checkpoint is
/// written as `checkpoint`; the log reinterprets it at the next start.
pub const SAVE_CAUSES: &[(SaveCause, &str)] = &[
    (SaveCause::Checkpoint, "checkpoint"),
    (SaveCause::LastWindowClosed, "last-window-closed"),
    (SaveCause::LastTabClosed, "last-tab-closed"),
    (SaveCause::ExplorerTerminated, "explorer-terminated"),
    (SaveCause::SignOut, "sign-out"),
    (SaveCause::Shutdown, "shutdown"),
    (SaveCause::Restart, "restart"),
    (SaveCause::ExitThroughSettings, "exit-through-settings"),
];

/// `state=` tokens. `minimized-max` is a minimized window that restores to
/// maximized (`WPF_RESTORETOMAXIMIZED`).
pub const SHOW_STATES: &[(ShowState, &str)] = &[
    (ShowState::Normal, "normal"),
    (ShowState::Maximized, "maximized"),
    (ShowState::Minimized { restore_maximized: false }, "minimized"),
    (ShowState::Minimized { restore_maximized: true }, "minimized-max"),
];

fn to_token<T: PartialEq + Copy>(table: &[(T, &'static str)], value: T) -> &'static str {
    table
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, t)| *t)
        // Every table above lists every value of its enum (checked by tests).
        .expect("token table is exhaustive")
}

fn from_token<T: Copy>(table: &[(T, &'static str)], token: &str) -> Option<T> {
    table.iter().find(|(_, t)| *t == token).map(|(v, _)| *v)
}

pub fn kind_token(kind: LocationKind) -> &'static str {
    to_token(LOCATION_KINDS, kind)
}

pub fn kind_from_token(token: &str) -> Option<LocationKind> {
    from_token(LOCATION_KINDS, token)
}

pub fn status_token(status: Status) -> &'static str {
    to_token(STATUSES, status)
}

pub fn status_from_token(token: &str) -> Option<Status> {
    from_token(STATUSES, token)
}

pub fn cause_token(cause: SaveCause) -> &'static str {
    to_token(SAVE_CAUSES, cause)
}

pub fn cause_from_token(token: &str) -> Option<SaveCause> {
    from_token(SAVE_CAUSES, token)
}

pub fn show_state_token(state: ShowState) -> &'static str {
    to_token(SHOW_STATES, state)
}

pub fn show_state_from_token(token: &str) -> Option<ShowState> {
    from_token(SHOW_STATES, token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn check_table<T: Copy + PartialEq + std::fmt::Debug + std::hash::Hash + Eq>(
        table: &[(T, &'static str)],
        expected_len: usize,
    ) {
        assert_eq!(table.len(), expected_len);
        let values: HashSet<T> = table.iter().map(|(v, _)| *v).collect();
        let tokens: HashSet<&str> = table.iter().map(|(_, t)| *t).collect();
        assert_eq!(values.len(), expected_len, "duplicate value");
        assert_eq!(tokens.len(), expected_len, "duplicate token");
        for (v, t) in table {
            assert_eq!(to_token(table, *v), *t);
            assert_eq!(from_token(table, t), Some(*v));
            // Tokens need no escaping and are lowercase.
            assert!(t.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{t}");
        }
    }

    #[test]
    fn tables_are_exhaustive_and_bijective() {
        // 4 volumes + zip + 10 shell kinds + search.
        check_table(LOCATION_KINDS, 16);
        check_table(STATUSES, 2);
        check_table(SAVE_CAUSES, 8);
        check_table(SHOW_STATES, 4);
        assert_eq!(kind_from_token("FS-FIXED"), None);
        assert_eq!(cause_from_token(""), None);
    }
}
