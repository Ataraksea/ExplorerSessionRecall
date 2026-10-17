//! Error messages shown on the UI thread (Requirement 10.5).
//!
//! Each message is an ownerless task dialog with an error icon and an OK
//! button (Esc and the title-bar X also close it). It runs on its own short
//! modal thread started by the UI thread, so it never blocks tracking or the
//! Settings dialog. Without comctl32 v6 it falls back to `MessageBoxW`.
//!
//! The unsupported-build message (Req 1.3) is shown before the Engine and
//! the UI thread exist, by `crate::version::show_unsupported_message`.

use esr_core::engine::MessageKind;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW};
use windows::core::HSTRING;

use super::prompt::{CAPTION, Icon, ModalList, TaskDialogSpec, run_task_dialog};

/// The copy of one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageText {
    pub main: &'static str,
    pub content: &'static str,
}

/// User-facing copy for `kind`.
pub fn message_text(kind: MessageKind) -> MessageText {
    match kind {
        MessageKind::BuiltinRestoreDisableFailed => MessageText {
            main: "Couldn\u{2019}t turn off Windows\u{2019} folder restore",
            content: "Explorer Session Recall couldn\u{2019}t turn off Windows\u{2019} \u{201C}Restore previous \
folder windows at logon\u{201D} option, so you may get duplicate windows when you sign in. You\u{2019}ll be asked \
again the next time Explorer Session Recall starts.",
        },
    }
}

/// Shows the message for `kind` and blocks this thread until it is closed.
/// `open` tracks the dialog window while it is displayed.
pub fn show_message(kind: MessageKind, open: &ModalList) {
    let text = message_text(kind);
    let spec = TaskDialogSpec {
        main: text.main,
        content: text.content,
        icon: Icon::Error,
        links: &[],
        default_button: 0,
    };
    if run_task_dialog(&spec, open).is_some() {
        return;
    }
    let body = HSTRING::from(format!("{}\n\n{}", text.main, text.content));
    let caption = HSTRING::from(CAPTION);
    // SAFETY: both strings outlive the call.
    unsafe {
        MessageBoxW(
            None,
            &body,
            &caption,
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_failed_message_says_what_happened() {
        let t = message_text(MessageKind::BuiltinRestoreDisableFailed);
        assert!(t.main.contains("turn off"));
        assert!(
            t.content
                .contains("Restore previous folder windows at logon")
        );
        assert!(t.content.contains("asked again"));
    }

    /// Manual: shows the error message.
    #[test]
    #[ignore = "manual: shows the error message"]
    fn manual_disable_failed_message() {
        let open = ModalList::default();
        show_message(MessageKind::BuiltinRestoreDisableFailed, &open);
        assert!(open.lock().unwrap().is_empty());
    }
}
