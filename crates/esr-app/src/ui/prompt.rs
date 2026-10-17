//! Builtin_Restore_Option prompt (Requirements 10.1, 10.3, 10.6).
//!
//! A `TaskDialogIndirect` with two command links, "Turn it off
//! (recommended)" → [`PromptAnswer::Accepted`] and "Keep it on" →
//! [`PromptAnswer::Declined`], and `TDF_ALLOW_DIALOG_CANCELLATION`, so Esc,
//! Alt+F4 and the title-bar X return `IDCANCEL` →
//! [`PromptAnswer::Dismissed`] (Req 10.6).
//!
//! # Common Controls 6
//!
//! `TaskDialogIndirect` exists only in comctl32 v6. esr.exe (and the esr-app
//! test executables) embed a manifest that selects v6 (see `build.rs`), but
//! the function is resolved at run time with `GetProcAddress` rather than
//! imported: a static import would stop every *other* executable that links
//! this crate without that manifest (e.g. `esr-sim` tests) from loading at
//! all. If the function is unavailable, the prompt falls back to a
//! Yes/No/Cancel `MessageBoxW` with the same meaning.

use std::sync::{Mutex, OnceLock};

use esr_core::engine::PromptAnswer;
use windows::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_COMMON_BUTTON_FLAGS, TASKDIALOG_FLAGS, TASKDIALOG_NOTIFICATIONS,
    TASKDIALOGCONFIG, TD_ERROR_ICON, TD_INFORMATION_ICON, TDCBF_OK_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_USE_COMMAND_LINKS, TDN_CREATED, TDN_DESTROYED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IDCANCEL, IDNO, IDYES, MB_ICONINFORMATION, MB_SETFOREGROUND, MB_YESNOCANCEL, MessageBoxW,
};
use windows::core::{BOOL, HRESULT, HSTRING, PCWSTR, s, w};

/// Window title of the prompt and the messages.
pub const CAPTION: &str = "Explorer Session Restore";
/// Command-link id: turn the Builtin_Restore_Option off.
pub const ID_TURN_OFF: i32 = 100;
/// Command-link id: keep the Builtin_Restore_Option on.
pub const ID_KEEP_ON: i32 = 101;

pub const MAIN_INSTRUCTION: &str = "Turn off Windows\u{2019} folder restore at sign-in?";
pub const CONTENT: &str = "Windows\u{2019} \u{201C}Restore previous folder windows at logon\u{201D} option \
reopens your folders when you sign in. Explorer Session Restore reopens them too, so you get duplicate windows.";
/// Command-link text; the part after `\n` is shown as the link's note.
pub const TURN_OFF_TEXT: &str = "Turn it off (recommended)\nExplorer Session Restore reopens your windows and \
tabs. No other File Explorer setting is changed.";
pub const KEEP_ON_TEXT: &str = "Keep it on\nYou may get duplicate windows. You won\u{2019}t be asked again unless you choose that in Settings.";
/// Extra line of the `MessageBoxW` fallback, which has no command links.
pub const FALLBACK_CHOICES: &str = "Select Yes to turn it off (recommended) or No to keep it on.";

/// Maps the `TaskDialogIndirect` button result to the answer. Anything other
/// than the two command links (`IDCANCEL`, or 0 on failure) records no
/// decision (Req 10.6).
pub fn answer_from_task_dialog(button: i32) -> PromptAnswer {
    match button {
        ID_TURN_OFF => PromptAnswer::Accepted,
        ID_KEEP_ON => PromptAnswer::Declined,
        _ => PromptAnswer::Dismissed,
    }
}

/// Maps the `MessageBoxW` fallback result (`MB_YESNOCANCEL`) to the answer.
pub fn answer_from_message_box(result: i32) -> PromptAnswer {
    if result == IDYES.0 {
        PromptAnswer::Accepted
    } else if result == IDNO.0 {
        PromptAnswer::Declined
    } else {
        PromptAnswer::Dismissed
    }
}

/// Open task dialog windows (as `usize` HWNDs), so the UI thread can close
/// them on shutdown.
pub type ModalList = Mutex<Vec<usize>>;

/// Icon of a task dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Icon {
    Information,
    Error,
}

/// A task dialog to show.
pub(crate) struct TaskDialogSpec<'a> {
    pub main: &'a str,
    pub content: &'a str,
    pub icon: Icon,
    /// Command links `(id, text)`; empty for an OK-only dialog.
    pub links: &'a [(i32, &'a str)],
    pub default_button: i32,
}

type TaskDialogIndirectFn =
    unsafe extern "system" fn(*const TASKDIALOGCONFIG, *mut i32, *mut i32, *mut BOOL) -> HRESULT;

/// `TaskDialogIndirect` from comctl32 v6, if the process uses it.
fn task_dialog_indirect() -> Option<TaskDialogIndirectFn> {
    static FN: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *FN.get_or_init(|| {
        // SAFETY: loading a system DLL by name (resolved through the process
        // activation context, i.e. v6 with the manifest) and looking up an
        // export; the module is never freed.
        unsafe {
            let module = LoadLibraryW(w!("comctl32.dll")).ok()?;
            GetProcAddress(module, s!("TaskDialogIndirect")).map(|f| f as usize)
        }
    });
    // SAFETY: the export has exactly this signature (CommCtrl.h).
    addr.map(|a| unsafe { std::mem::transmute::<usize, TaskDialogIndirectFn>(a) })
}

/// Whether `TaskDialogIndirect` is available in this process.
pub fn task_dialog_available() -> bool {
    task_dialog_indirect().is_some()
}

unsafe extern "system" fn track_callback(
    hwnd: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    _lparam: LPARAM,
    data: isize,
) -> HRESULT {
    if data != 0 {
        // SAFETY: `data` is the `&ModalList` passed by `run_task_dialog`,
        // alive for the whole TaskDialogIndirect call.
        let list = unsafe { &*(data as *const ModalList) };
        if let Ok(mut open) = list.lock() {
            if msg == TDN_CREATED {
                open.push(hwnd.0 as usize);
            } else if msg == TDN_DESTROYED {
                open.retain(|&h| h != hwnd.0 as usize);
            }
        }
    }
    S_OK
}

/// Shows a modal, ownerless task dialog and returns the pressed button, or
/// `None` if task dialogs are unavailable or the call failed.
pub(crate) fn run_task_dialog(spec: &TaskDialogSpec<'_>, open: &ModalList) -> Option<i32> {
    let show = task_dialog_indirect()?;
    let caption = HSTRING::from(CAPTION);
    let main = HSTRING::from(spec.main);
    let content = HSTRING::from(spec.content);
    let texts: Vec<HSTRING> = spec.links.iter().map(|(_, t)| HSTRING::from(*t)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = spec
        .links
        .iter()
        .zip(&texts)
        .map(|((id, _), t)| TASKDIALOG_BUTTON {
            nButtonID: *id,
            pszButtonText: PCWSTR(t.as_ptr()),
        })
        .collect();

    let mut flags = TDF_ALLOW_DIALOG_CANCELLATION;
    let mut common = TASKDIALOG_COMMON_BUTTON_FLAGS(0);
    if buttons.is_empty() {
        common = TDCBF_OK_BUTTON;
    } else {
        flags = TASKDIALOG_FLAGS(flags.0 | TDF_USE_COMMAND_LINKS.0);
    }
    let mut config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: flags,
        dwCommonButtons: common,
        pszWindowTitle: PCWSTR(caption.as_ptr()),
        pszMainInstruction: PCWSTR(main.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: if buttons.is_empty() {
            std::ptr::null()
        } else {
            buttons.as_ptr()
        },
        nDefaultButton: spec.default_button,
        pfCallback: Some(track_callback),
        lpCallbackData: open as *const ModalList as isize,
        ..Default::default()
    };
    config.Anonymous1.pszMainIcon = match spec.icon {
        Icon::Information => TD_INFORMATION_ICON,
        Icon::Error => TD_ERROR_ICON,
    };
    let mut button = 0i32;
    // SAFETY: every pointer in `config` refers to locals that outlive the
    // call; `open` outlives it as well (read by the callback).
    let hr = unsafe {
        show(
            &config,
            &mut button,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    hr.is_ok().then_some(button)
}

/// Shows the prompt (blocking this thread until answered) and returns the
/// answer. `open` tracks the dialog window while it is displayed.
pub fn show_prompt(open: &ModalList) -> PromptAnswer {
    let spec = TaskDialogSpec {
        main: MAIN_INSTRUCTION,
        content: CONTENT,
        icon: Icon::Information,
        links: &[(ID_TURN_OFF, TURN_OFF_TEXT), (ID_KEEP_ON, KEEP_ON_TEXT)],
        default_button: ID_TURN_OFF,
    };
    if let Some(button) = run_task_dialog(&spec, open) {
        return answer_from_task_dialog(button);
    }
    let text = HSTRING::from(fallback_text());
    let caption = HSTRING::from(CAPTION);
    // SAFETY: both strings outlive the call.
    let result = unsafe {
        MessageBoxW(
            None,
            &text,
            &caption,
            MB_YESNOCANCEL | MB_ICONINFORMATION | MB_SETFOREGROUND,
        )
    };
    answer_from_message_box(result.0)
}

/// Body of the `MessageBoxW` fallback.
pub fn fallback_text() -> String {
    format!("{MAIN_INSTRUCTION}\n\n{CONTENT}\n\n{FALLBACK_CHOICES}")
}

/// `IDCANCEL` as returned for a dismissed task dialog.
pub const ID_DISMISSED: i32 = IDCANCEL.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_dialog_buttons_map_to_answers() {
        assert_eq!(answer_from_task_dialog(ID_TURN_OFF), PromptAnswer::Accepted);
        assert_eq!(answer_from_task_dialog(ID_KEEP_ON), PromptAnswer::Declined);
        assert_eq!(
            answer_from_task_dialog(ID_DISMISSED),
            PromptAnswer::Dismissed
        );
        assert_eq!(answer_from_task_dialog(0), PromptAnswer::Dismissed);
        assert_eq!(answer_from_task_dialog(1), PromptAnswer::Dismissed); // IDOK
    }

    #[test]
    fn message_box_fallback_maps_to_answers() {
        assert_eq!(answer_from_message_box(IDYES.0), PromptAnswer::Accepted);
        assert_eq!(answer_from_message_box(IDNO.0), PromptAnswer::Declined);
        assert_eq!(answer_from_message_box(IDCANCEL.0), PromptAnswer::Dismissed);
        assert_eq!(answer_from_message_box(0), PromptAnswer::Dismissed);
    }

    #[test]
    fn copy_explains_the_duplicate_windows() {
        assert!(CONTENT.contains("Restore previous folder windows at logon"));
        assert!(CONTENT.contains("duplicate windows"));
        assert!(TURN_OFF_TEXT.starts_with("Turn it off (recommended)\n"));
        assert!(KEEP_ON_TEXT.starts_with("Keep it on\n"));
        let fallback = fallback_text();
        assert!(fallback.contains(CONTENT) && fallback.contains("Yes") && fallback.contains("No"));
    }

    // IDOK..IDCONTINUE are 1..=11; the command-link ids must not collide.
    const _: () = assert!(ID_TURN_OFF > 11 && ID_KEEP_ON > 11 && ID_TURN_OFF != ID_KEEP_ON);

    /// The esr-app test executables embed the Common Controls 6 manifest, so
    /// the runtime lookup must succeed here (as it does in esr.exe).
    #[test]
    fn task_dialog_is_available_with_the_embedded_manifest() {
        assert!(task_dialog_available());
    }

    /// Manual: shows the prompt and prints the answer.
    #[test]
    #[ignore = "manual: shows the prompt"]
    fn manual_prompt() {
        let open = ModalList::default();
        let answer = show_prompt(&open);
        println!("answer: {answer:?}");
        assert!(open.lock().unwrap().is_empty());
    }
}
