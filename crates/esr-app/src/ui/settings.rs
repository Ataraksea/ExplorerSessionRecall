//! Settings dialog (Requirements 12.1–12.5, 12.7).
//!
//! # Template choice
//!
//! The dialog is a standard Win32 dialog built from an **in-memory
//! `DLGTEMPLATEEX`** ([`encode_template`]) and created modeless with
//! `CreateDialogIndirectParamW`. It behaves exactly like a resource-template
//! dialog (dialog manager, `IsDialogMessageW` keyboard handling, standard
//! `Button`/`Static` controls, so UIA, mnemonics, and high contrast work by
//! default), but needs no `.rc` file, resource compiler, or build dependency,
//! and the layout stays a plain, unit-tested value ([`settings_layout`]).
//!
//! * **Font:** the system message font from `NONCLIENTMETRICSW` (read at
//!   96 DPI and converted to points), written into the template with
//!   `DS_SETFONT`. Dialog units derive from that font.
//! * **DPI:** the UI thread runs per-monitor-v2 aware. For dialogs created with
//!   `CreateDialog*` in that context Windows scales the template font and all
//!   dialog-unit coordinates to the monitor's DPI at creation and again on
//!   `WM_DPICHANGED`, so no manual `GetDpiForWindow` layout pass is needed.
//! * **Keyboard:** controls are listed in tab order; every actionable control
//!   has a unique mnemonic except Close, which is the default button and also
//!   answers Esc and the title-bar X (`IDCANCEL`).
//!
//! # Behaviour
//!
//! Every checkbox toggle and the "ask again" button post
//! `Event::SettingsChanged` immediately (Req 12.4); there is no OK/Apply
//! step. "Exit Explorer Session Restore" posts `Event::ExitRequested`
//! (Req 12.5) and closes the dialog. The UI thread keeps at most one Settings
//! dialog and brings an open one to the foreground (Req 12.7).

use std::cell::Cell;

use esr_core::engine::Event;
use esr_core::settings::Settings;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, IsDlgButtonChecked,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateDialogIndirectParamW, DLGTEMPLATE, DestroyWindow, GetDlgItem, GetWindowLongPtrW,
    IsIconic, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SW_RESTORE, SW_SHOW, SendMessageW,
    SetDlgItemTextW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, WINDOW_LONG_PTR_INDEX,
    WM_COMMAND, WM_DESTROY, WM_INITDIALOG, WM_NEXTDLGCTL,
};
use windows::core::HSTRING;

use crate::ports::SharedSink;

// ---------------------------------------------------------------------------
// Control ids and copy
// ---------------------------------------------------------------------------

/// `IDOK`; only reachable through Enter if no default button exists.
pub const ID_OK: u16 = 1;
/// `IDCANCEL`: the Close button, Esc, and the title-bar X.
pub const ID_CLOSE: u16 = 2;
/// "Restore my File Explorer session" (`restoration_enabled`).
pub const IDC_RESTORE: u16 = 1001;
/// "Start when I sign in" (`start_at_sign_in`).
pub const IDC_START_AT_SIGN_IN: u16 = 1002;
/// Explanation of the Builtin_Restore_Option prompt state.
pub const IDC_PROMPT_STATUS: u16 = 1003;
/// Re-enables the Builtin_Restore_Option prompt (Req 12.3).
pub const IDC_ASK_AGAIN: u16 = 1004;
/// "Exit Explorer Session Restore" (Req 12.5).
pub const IDC_EXIT: u16 = 1005;
/// "Open folders as tabs in my File Explorer window" (`tab_folding_enabled`).
pub const IDC_TAB_FOLDING: u16 = 1006;

pub const TITLE: &str = "Explorer Session Restore Settings";
pub const RESTORE_LABEL: &str = "&Restore my File Explorer session";
pub const START_LABEL: &str = "&Start when I sign in";
pub const FOLDING_LABEL: &str = "&Open folders as tabs in my File Explorer window";
pub const ASK_AGAIN_LABEL: &str = "&Ask me about it again";
pub const EXIT_LABEL: &str = "E&xit Explorer Session Restore";
pub const CLOSE_LABEL: &str = "Close";
/// Status text while the prompt is declined (the ask-again button is enabled).
pub const STATUS_DECLINED: &str = "You chose to keep Windows\u{2019} \u{201C}Restore previous folder windows \
at logon\u{201D} option on. It can open duplicate windows when you sign in.";
/// Status text while the prompt is active (the ask-again button is disabled).
pub const STATUS_ACTIVE: &str = "If Windows\u{2019} \u{201C}Restore previous folder windows at logon\u{201D} \
option is on, you\u{2019}ll be asked at startup whether to turn it off.";

// ---------------------------------------------------------------------------
// Window and control styles (values from WinUser.h; checked in tests)
// ---------------------------------------------------------------------------

/// Style bits used by the template. Plain `u32`s keep the layout pure.
pub mod style {
    pub const WS_POPUP: u32 = 0x8000_0000;
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_CAPTION: u32 = 0x00C0_0000;
    pub const WS_SYSMENU: u32 = 0x0008_0000;
    /// For child controls: starts a group.
    pub const WS_GROUP: u32 = 0x0002_0000;
    /// For top-level windows: minimize box (same bit as `WS_GROUP`).
    pub const WS_MINIMIZEBOX: u32 = 0x0002_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
    pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;
    pub const DS_SETFONT: u32 = 0x40;
    pub const DS_MODALFRAME: u32 = 0x80;
    pub const DS_CENTER: u32 = 0x800;
    pub const BS_PUSHBUTTON: u32 = 0x0;
    pub const BS_DEFPUSHBUTTON: u32 = 0x1;
    pub const BS_AUTOCHECKBOX: u32 = 0x3;
    pub const SS_LEFT: u32 = 0x0;
    pub const SS_NOPREFIX: u32 = 0x80;
}

// ---------------------------------------------------------------------------
// Pure layout model
// ---------------------------------------------------------------------------

/// Predefined control classes (ordinals in a dialog template).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlClass {
    Button,
    Static,
}

impl ControlClass {
    /// Class ordinal (`0x0080` Button, `0x0082` Static).
    pub fn atom(self) -> u16 {
        match self {
            ControlClass::Button => 0x0080,
            ControlClass::Static => 0x0082,
        }
    }
}

/// One control, in dialog units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlSpec {
    pub id: u16,
    pub class: ControlClass,
    pub text: &'static str,
    pub style: u32,
    pub x: i16,
    pub y: i16,
    pub cx: i16,
    pub cy: i16,
}

/// A dialog, in dialog units. `controls` is in tab order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogLayout {
    pub title: &'static str,
    /// Window style without `DS_SETFONT` (the encoder adds it).
    pub style: u32,
    pub ex_style: u32,
    pub cx: i16,
    pub cy: i16,
    pub controls: Vec<ControlSpec>,
}

/// The template font (`DS_SETFONT` fields of `DLGTEMPLATEEX`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogFont {
    pub point_size: u16,
    pub weight: u16,
    pub italic: bool,
    pub charset: u8,
    pub face: String,
}

impl DialogFont {
    /// Used when the system font cannot be read.
    pub fn fallback() -> Self {
        DialogFont {
            point_size: 9,
            weight: 400,
            italic: false,
            charset: 1,
            face: "Segoe UI".into(),
        }
    }

    /// Converts a `LOGFONTW` read at 96 DPI into template fields.
    ///
    /// A negative height is the character height in pixels (points =
    /// pixels × 72 / 96); a positive height is the cell height and is used the
    /// same way (a slight overestimate). The result is clamped to 6–24 pt;
    /// 0 and an empty face fall back to 9 pt Segoe UI. Weight 0
    /// (`FW_DONTCARE`) becomes 400.
    pub fn from_logfont(height: i32, weight: i32, italic: bool, charset: u8, face: &[u16]) -> Self {
        let fallback = Self::fallback();
        let pixels = height.unsigned_abs();
        let point_size = if pixels == 0 {
            fallback.point_size
        } else {
            ((pixels * 72 + 48) / 96).clamp(6, 24) as u16
        };
        let weight = if weight <= 0 {
            400
        } else {
            weight.min(1000) as u16
        };
        let len = face.iter().position(|&c| c == 0).unwrap_or(face.len());
        let face = String::from_utf16_lossy(&face[..len]);
        let face = if face.trim().is_empty() {
            fallback.face
        } else {
            face
        };
        DialogFont {
            point_size,
            weight,
            italic,
            charset,
            face,
        }
    }
}

/// The Settings dialog layout (dialog units, tab order).
pub fn settings_layout() -> DialogLayout {
    use style::*;
    let child = WS_CHILD | WS_VISIBLE;
    let control = |id, class, text, style, x, y, cx, cy| ControlSpec {
        id,
        class,
        text,
        style,
        x,
        y,
        cx,
        cy,
    };
    DialogLayout {
        title: TITLE,
        style: WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | DS_MODALFRAME | DS_CENTER,
        // Taskbar button, so the user can find the window again.
        ex_style: WS_EX_APPWINDOW,
        cx: 230,
        cy: 128,
        controls: vec![
            // The three checkboxes form one group (arrow keys move between them).
            control(
                IDC_RESTORE,
                ControlClass::Button,
                RESTORE_LABEL,
                child | WS_GROUP | WS_TABSTOP | BS_AUTOCHECKBOX,
                7,
                7,
                216,
                10,
            ),
            control(
                IDC_START_AT_SIGN_IN,
                ControlClass::Button,
                START_LABEL,
                child | WS_TABSTOP | BS_AUTOCHECKBOX,
                7,
                21,
                216,
                10,
            ),
            control(
                IDC_TAB_FOLDING,
                ControlClass::Button,
                FOLDING_LABEL,
                child | WS_TABSTOP | BS_AUTOCHECKBOX,
                7,
                35,
                216,
                10,
            ),
            control(
                IDC_PROMPT_STATUS,
                ControlClass::Static,
                STATUS_ACTIVE,
                child | WS_GROUP | SS_LEFT | SS_NOPREFIX,
                7,
                53,
                216,
                26,
            ),
            control(
                IDC_ASK_AGAIN,
                ControlClass::Button,
                ASK_AGAIN_LABEL,
                child | WS_GROUP | WS_TABSTOP | BS_PUSHBUTTON,
                7,
                81,
                120,
                14,
            ),
            control(
                IDC_EXIT,
                ControlClass::Button,
                EXIT_LABEL,
                child | WS_GROUP | WS_TABSTOP | BS_PUSHBUTTON,
                7,
                107,
                120,
                14,
            ),
            control(
                ID_CLOSE,
                ControlClass::Button,
                CLOSE_LABEL,
                child | WS_GROUP | WS_TABSTOP | BS_DEFPUSHBUTTON,
                173,
                107,
                50,
                14,
            ),
        ],
    }
}

/// The mnemonic of a control text (`&x`, ignoring `&&`), lower-cased.
pub fn mnemonic(text: &str) -> Option<char> {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.next() {
                Some('&') => continue,
                Some(m) => return Some(m.to_ascii_lowercase()),
                None => return None,
            }
        }
    }
    None
}

/// What the controls show for a given `Settings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsView {
    pub restore_checked: bool,
    pub start_checked: bool,
    pub folding_checked: bool,
    /// The ask-again button is only useful after the prompt was declined.
    pub ask_again_enabled: bool,
    pub status_text: &'static str,
}

/// Maps `Settings` to control states.
pub fn view(settings: &Settings) -> SettingsView {
    SettingsView {
        restore_checked: settings.restoration_enabled,
        start_checked: settings.start_at_sign_in,
        folding_checked: settings.tab_folding_enabled,
        ask_again_enabled: settings.builtin_prompt_declined,
        status_text: if settings.builtin_prompt_declined {
            STATUS_DECLINED
        } else {
            STATUS_ACTIVE
        },
    }
}

/// What a `WM_COMMAND` means for the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    /// Not a command the dialog acts on.
    None,
    /// Apply and post these Settings now (Req 12.4).
    Changed(Settings),
    /// Post `ExitRequested` and close (Req 12.5).
    Exit,
    /// Close the dialog only.
    Close,
}

/// `BN_CLICKED`.
pub const BN_CLICKED: u16 = 0;

/// Maps a `WM_COMMAND` (control `id`, `notify` code, and, for checkboxes,
/// the check state after the click) to an action. A toggle that leaves the
/// value unchanged yields `None`.
pub fn on_command(current: &Settings, id: u16, notify: u16, checked: bool) -> SettingsAction {
    if notify != BN_CLICKED {
        return SettingsAction::None;
    }
    let mut next = current.clone();
    match id {
        IDC_RESTORE => next.restoration_enabled = checked,
        IDC_START_AT_SIGN_IN => next.start_at_sign_in = checked,
        IDC_TAB_FOLDING => next.tab_folding_enabled = checked,
        // Req 12.3: clear the recorded decline so the prompt shows again.
        IDC_ASK_AGAIN => next.builtin_prompt_declined = false,
        IDC_EXIT => return SettingsAction::Exit,
        ID_CLOSE | ID_OK => return SettingsAction::Close,
        _ => return SettingsAction::None,
    }
    if &next == current {
        SettingsAction::None
    } else {
        SettingsAction::Changed(next)
    }
}

// ---------------------------------------------------------------------------
// DLGTEMPLATEEX encoding (pure)
// ---------------------------------------------------------------------------

fn push_u32(w: &mut Vec<u16>, v: u32) {
    w.push(v as u16);
    w.push((v >> 16) as u16);
}

fn push_str(w: &mut Vec<u16>, s: &str) {
    w.extend(s.encode_utf16());
    w.push(0);
}

/// Encodes `layout` as a `DLGTEMPLATEEX` with `DS_SETFONT` and `font`.
///
/// The result is in 16-bit words from the template start; every
/// `DLGITEMTEMPLATEEX` starts on a DWORD boundary relative to that start.
/// Pass it to Windows through [`aligned`], which provides the required DWORD
/// alignment of the buffer itself.
pub fn encode_template(layout: &DialogLayout, font: &DialogFont) -> Vec<u16> {
    let mut w = Vec::with_capacity(512);
    w.push(1); // dlgVer
    w.push(0xFFFF); // signature: DLGTEMPLATEEX
    push_u32(&mut w, 0); // helpID
    push_u32(&mut w, layout.ex_style);
    push_u32(&mut w, layout.style | style::DS_SETFONT);
    w.push(layout.controls.len() as u16);
    for v in [0, 0, layout.cx, layout.cy] {
        w.push(v as u16);
    }
    w.push(0); // no menu
    w.push(0); // default dialog class
    push_str(&mut w, layout.title);
    w.push(font.point_size);
    w.push(font.weight);
    w.push(u16::from(font.italic) | (u16::from(font.charset) << 8));
    push_str(&mut w, &font.face);
    for c in &layout.controls {
        if !w.len().is_multiple_of(2) {
            w.push(0);
        }
        push_u32(&mut w, 0); // helpID
        push_u32(&mut w, 0); // exStyle
        push_u32(&mut w, c.style);
        for v in [c.x, c.y, c.cx, c.cy] {
            w.push(v as u16);
        }
        push_u32(&mut w, u32::from(c.id));
        w.push(0xFFFF);
        w.push(c.class.atom());
        push_str(&mut w, c.text);
        w.push(0); // no creation data
    }
    w
}

/// Copies template words into a DWORD-aligned buffer (same byte layout).
pub fn aligned(words: &[u16]) -> Vec<u32> {
    words
        .chunks(2)
        .map(|p| u32::from(p[0]) | (u32::from(*p.get(1).unwrap_or(&0)) << 16))
        .collect()
}

// ---------------------------------------------------------------------------
// Native dialog
// ---------------------------------------------------------------------------

/// `DWLP_USER` = `DWLP_MSGRESULT` + sizeof(LRESULT) + sizeof(DLGPROC).
const DWLP_USER: WINDOW_LONG_PTR_INDEX =
    WINDOW_LONG_PTR_INDEX((2 * std::mem::size_of::<isize>()) as i32);

/// Per-dialog state, owned by the dialog from `WM_INITDIALOG` to `WM_DESTROY`.
struct DialogState {
    settings: Settings,
    sink: SharedSink,
    on_destroyed: fn(),
}

/// Hands the state to `WM_INITDIALOG`. If the dialog never adopts it (creation
/// failed early), it is dropped with this value.
struct InitParam {
    state: Cell<Option<Box<DialogState>>>,
}

/// Reads the system message font at 96 DPI (per-monitor scaling is applied
/// by Windows when the dialog is created).
pub fn system_dialog_font() -> DialogFont {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `ncm` is a writable NONCLIENTMETRICSW with its size set.
    let read = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut NONCLIENTMETRICSW as *mut core::ffi::c_void),
            0,
            96,
        )
    };
    match read {
        Ok(()) => {
            let lf = &ncm.lfMessageFont;
            DialogFont::from_logfont(
                lf.lfHeight,
                lf.lfWeight,
                lf.lfItalic != 0,
                lf.lfCharSet.0,
                &lf.lfFaceName,
            )
        }
        Err(_) => DialogFont::fallback(),
    }
}

/// Creates the Settings dialog without showing it. `on_destroyed` runs on
/// this thread when the dialog is destroyed (by Close, Exit, or the caller).
/// The calling thread must pump messages through `IsDialogMessageW`.
pub(crate) fn create_hidden(
    settings: Settings,
    sink: SharedSink,
    on_destroyed: fn(),
) -> windows::core::Result<HWND> {
    let words = encode_template(&settings_layout(), &system_dialog_font());
    let template = aligned(&words);
    let init = InitParam {
        state: Cell::new(Some(Box::new(DialogState {
            settings,
            sink,
            on_destroyed,
        }))),
    };
    // SAFETY: the template buffer is a valid, DWORD-aligned DLGTEMPLATEEX that
    // outlives the call; `init` outlives the call and WM_INITDIALOG (sent
    // during it) is the only reader of the pointer.
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        CreateDialogIndirectParamW(
            Some(hinstance.into()),
            template.as_ptr() as *const DLGTEMPLATE,
            None,
            Some(dialog_proc),
            LPARAM(&init as *const InitParam as isize),
        )
    }
}

/// Creates, shows, and activates the Settings dialog.
pub(crate) fn open(
    settings: Settings,
    sink: SharedSink,
    on_destroyed: fn(),
) -> windows::core::Result<HWND> {
    let hwnd = create_hidden(settings, sink, on_destroyed)?;
    // SAFETY: `hwnd` was just created on this thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    bring_to_front(hwnd);
    Ok(hwnd)
}

/// Restores a minimized dialog and brings it to the foreground (Req 12.7).
/// The second instance grants the foreground with `AllowSetForegroundWindow`.
pub(crate) fn bring_to_front(hwnd: HWND) {
    // SAFETY: plain window calls; they fail harmlessly on a stale handle.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Replaces the dialog's Settings (changes made elsewhere) and updates the controls.
pub(crate) fn refresh(hwnd: HWND, settings: Settings) {
    if !set_settings(hwnd, |s| *s = settings.clone()) {
        return;
    }
    apply_view(hwnd, &view(&settings));
}

/// Records that the prompt was just declined, so a later toggle in this
/// dialog does not undo the decision.
pub(crate) fn mark_prompt_declined(hwnd: HWND) {
    let mut now = None;
    set_settings(hwnd, |s| {
        s.builtin_prompt_declined = true;
        now = Some(s.clone());
    });
    if let Some(s) = now {
        apply_dynamic_view(hwnd, &view(&s));
    }
}

/// Destroys the dialog (`on_destroyed` runs).
pub(crate) fn close(hwnd: HWND) {
    // SAFETY: destroying a window owned by this thread.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn state_ptr(hwnd: HWND) -> *mut DialogState {
    // SAFETY: reading this dialog's own DWLP_USER slot.
    unsafe { GetWindowLongPtrW(hwnd, DWLP_USER) as *mut DialogState }
}

/// Runs `f` on the dialog's Settings; `false` if the dialog has no state.
/// The borrow ends before any window call.
fn set_settings(hwnd: HWND, f: impl FnOnce(&mut Settings)) -> bool {
    let ptr = state_ptr(hwnd);
    if ptr.is_null() {
        return false;
    }
    // SAFETY: the pointer is the live Box adopted in WM_INITDIALOG; it is only
    // freed in WM_DESTROY, which cannot run during `f` (no window calls).
    unsafe { f(&mut (*ptr).settings) };
    true
}

fn settings_of(hwnd: HWND) -> Option<(Settings, SharedSink)> {
    let ptr = state_ptr(hwnd);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: see `set_settings`.
    unsafe { Some(((*ptr).settings.clone(), (*ptr).sink.clone())) }
}

fn check(hwnd: HWND, id: u16, on: bool) {
    // SAFETY: controls of this thread's dialog.
    unsafe {
        let _ = CheckDlgButton(
            hwnd,
            i32::from(id),
            if on { BST_CHECKED } else { BST_UNCHECKED },
        );
    }
}

fn apply_view(hwnd: HWND, v: &SettingsView) {
    check(hwnd, IDC_RESTORE, v.restore_checked);
    check(hwnd, IDC_START_AT_SIGN_IN, v.start_checked);
    check(hwnd, IDC_TAB_FOLDING, v.folding_checked);
    apply_dynamic_view(hwnd, v);
}

/// Updates the ask-again button and the status text.
fn apply_dynamic_view(hwnd: HWND, v: &SettingsView) {
    // SAFETY: controls of this thread's dialog.
    unsafe {
        if let Ok(button) = GetDlgItem(Some(hwnd), i32::from(IDC_ASK_AGAIN)) {
            if !v.ask_again_enabled && GetFocus() == button {
                // Never leave keyboard focus on a disabled control.
                SendMessageW(hwnd, WM_NEXTDLGCTL, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            let _ = EnableWindow(button, v.ask_again_enabled);
        }
        let _ = SetDlgItemTextW(
            hwnd,
            i32::from(IDC_PROMPT_STATUS),
            &HSTRING::from(v.status_text),
        );
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            // SAFETY: lparam is the InitParam passed by `create_hidden`, alive
            // for the duration of CreateDialogIndirectParamW.
            let init = unsafe { &*(lparam.0 as *const InitParam) };
            if let Some(state) = init.state.take() {
                let settings = state.settings.clone();
                let raw = Box::into_raw(state);
                // SAFETY: storing the adopted pointer in this dialog's slot.
                unsafe { SetWindowLongPtrW(hwnd, DWLP_USER, raw as isize) };
                apply_view(hwnd, &view(&settings));
            }
            1 // let the dialog manager focus the first tab stop
        }
        WM_COMMAND => {
            let Some((current, sink)) = settings_of(hwnd) else {
                return 0;
            };
            let id = (wparam.0 & 0xFFFF) as u16;
            let notify = ((wparam.0 >> 16) & 0xFFFF) as u16;
            // SAFETY: control of this dialog (0 for non-checkbox ids).
            let checked = unsafe { IsDlgButtonChecked(hwnd, i32::from(id)) } == BST_CHECKED.0;
            match on_command(&current, id, notify, checked) {
                SettingsAction::None => 0,
                SettingsAction::Changed(next) => {
                    set_settings(hwnd, |s| *s = next.clone());
                    apply_dynamic_view(hwnd, &view(&next));
                    sink.post(Event::SettingsChanged(next));
                    1
                }
                SettingsAction::Exit => {
                    sink.post(Event::ExitRequested);
                    close(hwnd);
                    1
                }
                SettingsAction::Close => {
                    close(hwnd);
                    1
                }
            }
        }
        WM_DESTROY => {
            // SAFETY: take back ownership exactly once; the slot is cleared first.
            let raw = unsafe { SetWindowLongPtrW(hwnd, DWLP_USER, 0) } as *mut DialogState;
            if !raw.is_null() {
                // SAFETY: `raw` came from Box::into_raw in WM_INITDIALOG.
                let state = unsafe { Box::from_raw(raw) };
                (state.on_destroyed)();
            }
            0
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn s(restore: bool, start: bool, declined: bool) -> Settings {
        Settings {
            restoration_enabled: restore,
            start_at_sign_in: start,
            builtin_prompt_declined: declined,
            ..Settings::default()
        }
    }

    #[test]
    fn checkbox_toggles_map_to_settings_changes() {
        let cur = Settings::default();
        assert_eq!(
            on_command(&cur, IDC_RESTORE, BN_CLICKED, false),
            SettingsAction::Changed(s(false, true, false))
        );
        assert_eq!(
            on_command(&cur, IDC_START_AT_SIGN_IN, BN_CLICKED, false),
            SettingsAction::Changed(s(true, false, false))
        );
        let off = s(false, false, true);
        assert_eq!(
            on_command(&off, IDC_RESTORE, BN_CLICKED, true),
            SettingsAction::Changed(s(true, false, true))
        );
        // No change, no event.
        assert_eq!(
            on_command(&cur, IDC_RESTORE, BN_CLICKED, true),
            SettingsAction::None
        );
    }

    #[test]
    fn folding_toggle_maps_to_settings_change() {
        let cur = Settings::default();
        assert!(cur.tab_folding_enabled, "Req 12.10: on by default");
        let off = Settings {
            tab_folding_enabled: false,
            ..cur.clone()
        };
        assert_eq!(
            on_command(&cur, IDC_TAB_FOLDING, BN_CLICKED, false),
            SettingsAction::Changed(off.clone())
        );
        assert_eq!(
            on_command(&off, IDC_TAB_FOLDING, BN_CLICKED, true),
            SettingsAction::Changed(cur.clone())
        );
        // No change, no event; non-click notifications are ignored.
        assert_eq!(
            on_command(&cur, IDC_TAB_FOLDING, BN_CLICKED, true),
            SettingsAction::None
        );
        assert_eq!(
            on_command(&cur, IDC_TAB_FOLDING, 6, false),
            SettingsAction::None
        );
        // Other options are untouched.
        let declined = s(false, false, true);
        let toggled = Settings {
            tab_folding_enabled: false,
            ..declined.clone()
        };
        assert_eq!(
            on_command(&declined, IDC_TAB_FOLDING, BN_CLICKED, false),
            SettingsAction::Changed(toggled)
        );
    }

    #[test]
    fn ask_again_clears_the_decline_only_when_declined() {
        assert_eq!(
            on_command(&s(true, false, true), IDC_ASK_AGAIN, BN_CLICKED, false),
            SettingsAction::Changed(s(true, false, false))
        );
        assert_eq!(
            on_command(&s(true, true, false), IDC_ASK_AGAIN, BN_CLICKED, false),
            SettingsAction::None
        );
    }

    #[test]
    fn exit_close_and_other_commands() {
        let cur = Settings::default();
        assert_eq!(
            on_command(&cur, IDC_EXIT, BN_CLICKED, false),
            SettingsAction::Exit
        );
        assert_eq!(
            on_command(&cur, ID_CLOSE, BN_CLICKED, false),
            SettingsAction::Close
        );
        assert_eq!(
            on_command(&cur, ID_OK, BN_CLICKED, false),
            SettingsAction::Close
        );
        assert_eq!(
            on_command(&cur, IDC_PROMPT_STATUS, BN_CLICKED, false),
            SettingsAction::None
        );
        assert_eq!(
            on_command(&cur, 4242, BN_CLICKED, false),
            SettingsAction::None
        );
        // Non-click notifications (e.g. BN_SETFOCUS = 6) are ignored.
        assert_eq!(on_command(&cur, IDC_EXIT, 6, false), SettingsAction::None);
        assert_eq!(
            on_command(&cur, IDC_RESTORE, 6, false),
            SettingsAction::None
        );
    }

    #[test]
    fn view_follows_settings() {
        let v = view(&s(true, false, true));
        assert!(v.restore_checked && !v.start_checked && v.ask_again_enabled);
        assert_eq!(v.status_text, STATUS_DECLINED);
        let v = view(&Settings::default());
        assert!(v.restore_checked && v.start_checked && !v.ask_again_enabled);
        assert_eq!(v.status_text, STATUS_ACTIVE);
        assert!(v.folding_checked);
        let v = view(&Settings {
            tab_folding_enabled: false,
            ..Settings::default()
        });
        assert!(!v.folding_checked);
    }

    #[test]
    fn layout_has_required_controls_in_tab_order() {
        let l = settings_layout();
        let ids: Vec<u16> = l.controls.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                IDC_RESTORE,
                IDC_START_AT_SIGN_IN,
                IDC_TAB_FOLDING,
                IDC_PROMPT_STATUS,
                IDC_ASK_AGAIN,
                IDC_EXIT,
                ID_CLOSE
            ]
        );
        let unique: HashSet<u16> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());
        // Every actionable control is a tab stop; the static text is not.
        for c in &l.controls {
            let tab = c.style & style::WS_TABSTOP != 0;
            assert_eq!(tab, c.class == ControlClass::Button, "{}", c.text);
            assert!(c.style & style::WS_CHILD != 0 && c.style & style::WS_VISIBLE != 0);
        }
        // Exactly one default button: Close (also Esc / X).
        let defaults: Vec<u16> = l
            .controls
            .iter()
            .filter(|c| c.class == ControlClass::Button && c.style & 0xF == style::BS_DEFPUSHBUTTON)
            .map(|c| c.id)
            .collect();
        assert_eq!(defaults, [ID_CLOSE]);
    }

    #[test]
    fn layout_controls_fit_and_do_not_overlap() {
        let l = settings_layout();
        for c in &l.controls {
            assert!(c.x >= 7 && c.y >= 7, "{} inside the margin", c.text);
            assert!(
                c.x + c.cx <= l.cx - 7 && c.y + c.cy <= l.cy - 7,
                "{} fits",
                c.text
            );
        }
        for (i, a) in l.controls.iter().enumerate() {
            for b in &l.controls[i + 1..] {
                let apart = a.x + a.cx <= b.x
                    || b.x + b.cx <= a.x
                    || a.y + a.cy <= b.y
                    || b.y + b.cy <= a.y;
                assert!(apart, "{} overlaps {}", a.text, b.text);
            }
        }
    }

    #[test]
    fn mnemonics_are_unique_and_present() {
        assert_eq!(mnemonic("&Restore"), Some('r'));
        assert_eq!(mnemonic("E&xit"), Some('x'));
        assert_eq!(mnemonic("A && B"), None);
        assert_eq!(mnemonic("Close"), None);
        assert_eq!(mnemonic(FOLDING_LABEL), Some('o'));
        let l = settings_layout();
        let mut seen = HashSet::new();
        for c in l
            .controls
            .iter()
            .filter(|c| c.class == ControlClass::Button && c.id != ID_CLOSE)
        {
            let m = mnemonic(c.text).unwrap_or_else(|| panic!("{} has a mnemonic", c.text));
            assert!(seen.insert(m), "duplicate mnemonic {m}");
        }
        // The static text never shows an accidental mnemonic.
        let status = l
            .controls
            .iter()
            .find(|c| c.id == IDC_PROMPT_STATUS)
            .unwrap();
        assert!(status.style & style::SS_NOPREFIX != 0);
    }

    #[test]
    fn style_constants_match_the_sdk() {
        use windows::Win32::UI::WindowsAndMessaging as wm;
        assert_eq!(style::WS_POPUP, wm::WS_POPUP.0);
        assert_eq!(style::WS_CHILD, wm::WS_CHILD.0);
        assert_eq!(style::WS_VISIBLE, wm::WS_VISIBLE.0);
        assert_eq!(style::WS_CAPTION, wm::WS_CAPTION.0);
        assert_eq!(style::WS_SYSMENU, wm::WS_SYSMENU.0);
        assert_eq!(style::WS_GROUP, wm::WS_GROUP.0);
        assert_eq!(style::WS_MINIMIZEBOX, wm::WS_MINIMIZEBOX.0);
        assert_eq!(style::WS_TABSTOP, wm::WS_TABSTOP.0);
        assert_eq!(style::WS_EX_APPWINDOW, wm::WS_EX_APPWINDOW.0);
        assert_eq!(style::DS_SETFONT, wm::DS_SETFONT as u32);
        assert_eq!(style::DS_MODALFRAME, wm::DS_MODALFRAME as u32);
        assert_eq!(style::DS_CENTER, wm::DS_CENTER as u32);
        assert_eq!(style::BS_PUSHBUTTON, wm::BS_PUSHBUTTON as u32);
        assert_eq!(style::BS_DEFPUSHBUTTON, wm::BS_DEFPUSHBUTTON as u32);
        assert_eq!(style::BS_AUTOCHECKBOX, wm::BS_AUTOCHECKBOX as u32);
        assert_eq!(u32::from(BN_CLICKED), wm::BN_CLICKED);
        assert_eq!(i32::from(ID_CLOSE), wm::IDCANCEL.0);
        assert_eq!(i32::from(ID_OK), wm::IDOK.0);
    }

    #[test]
    fn font_from_logfont() {
        let face: Vec<u16> = "Segoe UI\0garbage".encode_utf16().collect();
        assert_eq!(
            DialogFont::from_logfont(-12, 400, false, 1, &face),
            DialogFont {
                point_size: 9,
                weight: 400,
                italic: false,
                charset: 1,
                face: "Segoe UI".into()
            }
        );
        let f = DialogFont::from_logfont(
            -16,
            0,
            true,
            0,
            &"Tahoma".encode_utf16().collect::<Vec<_>>(),
        );
        assert_eq!(
            (f.point_size, f.weight, f.italic, f.face.as_str()),
            (12, 400, true, "Tahoma")
        );
        // Unreadable values fall back.
        let f = DialogFont::from_logfont(0, 400, false, 1, &[0; 32]);
        assert_eq!((f.point_size, f.face.as_str()), (9, "Segoe UI"));
        assert_eq!(
            DialogFont::from_logfont(-200, 2000, false, 1, &face).point_size,
            24
        );
        assert_eq!(
            DialogFont::from_logfont(-200, 2000, false, 1, &face).weight,
            1000
        );
    }

    /// Minimal DLGTEMPLATEEX reader used to check the encoder.
    struct Reader<'a> {
        w: &'a [u16],
        i: usize,
    }

    impl Reader<'_> {
        fn u16(&mut self) -> u16 {
            self.i += 1;
            self.w[self.i - 1]
        }
        fn u32(&mut self) -> u32 {
            u32::from(self.u16()) | (u32::from(self.u16()) << 16)
        }
        fn string(&mut self) -> String {
            let start = self.i;
            while self.w[self.i] != 0 {
                self.i += 1;
            }
            self.i += 1;
            String::from_utf16(&self.w[start..self.i - 1]).unwrap()
        }
    }

    #[test]
    fn template_round_trips_through_a_reader() {
        let layout = settings_layout();
        let font = DialogFont {
            point_size: 9,
            weight: 400,
            italic: true,
            charset: 1,
            face: "Segoe UI".into(),
        };
        let words = encode_template(&layout, &font);
        let mut r = Reader { w: &words, i: 0 };
        assert_eq!((r.u16(), r.u16()), (1, 0xFFFF));
        assert_eq!(r.u32(), 0);
        assert_eq!(r.u32(), layout.ex_style);
        assert_eq!(r.u32(), layout.style | style::DS_SETFONT);
        assert_eq!(usize::from(r.u16()), layout.controls.len());
        assert_eq!([r.u16(), r.u16(), r.u16(), r.u16()], [0, 0, 230, 128]);
        assert_eq!((r.u16(), r.u16()), (0, 0));
        assert_eq!(r.string(), TITLE);
        assert_eq!((r.u16(), r.u16(), r.u16()), (9, 400, 1 | (1 << 8)));
        assert_eq!(r.string(), "Segoe UI");
        for c in &layout.controls {
            if !r.i.is_multiple_of(2) {
                assert_eq!(r.u16(), 0, "alignment padding is zero");
            }
            assert!(
                r.i.is_multiple_of(2),
                "{} starts on a DWORD boundary",
                c.text
            );
            assert_eq!((r.u32(), r.u32(), r.u32()), (0, 0, c.style));
            assert_eq!(
                [r.u16(), r.u16(), r.u16(), r.u16()],
                [c.x, c.y, c.cx, c.cy].map(|v| v as u16)
            );
            assert_eq!(r.u32(), u32::from(c.id));
            assert_eq!((r.u16(), r.u16()), (0xFFFF, c.class.atom()));
            assert_eq!(r.string(), c.text);
            assert_eq!(r.u16(), 0);
        }
        assert_eq!(r.i, words.len());
    }

    #[test]
    fn aligned_preserves_the_byte_layout() {
        let words = [0x1234u16, 0xABCD, 0x0001];
        let buf = aligned(&words);
        assert_eq!(buf, vec![0xABCD_1234, 0x0000_0001]);
        let bytes: Vec<u8> = buf.iter().flat_map(|d| d.to_le_bytes()).collect();
        let from_words: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(&bytes[..from_words.len()], &from_words[..]);
    }

    #[test]
    fn system_font_is_plausible() {
        let f = system_dialog_font();
        assert!((6..=24).contains(&f.point_size), "{f:?}");
        assert!(!f.face.is_empty());
    }

    /// Creates the real dialog from the in-memory template but never shows
    /// it (no `WS_VISIBLE`), then drives it with `WM_COMMAND` as a click would.
    #[test]
    fn hidden_dialog_posts_changes_immediately_and_exit() {
        use crate::ui::test_support::Recorder;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible};

        static DESTROYED: AtomicUsize = AtomicUsize::new(0);
        fn on_destroyed() {
            DESTROYED.fetch_add(1, Ordering::SeqCst);
        }
        fn click(dialog: HWND, id: u16) {
            // SAFETY: test-thread dialog; WM_COMMAND(BN_CLICKED) as sent by a button.
            unsafe {
                let ctrl = GetDlgItem(Some(dialog), i32::from(id)).unwrap();
                SendMessageW(
                    dialog,
                    WM_COMMAND,
                    Some(WPARAM(usize::from(id))),
                    Some(LPARAM(ctrl.0 as isize)),
                );
            }
        }

        let rec = Arc::new(Recorder::default());
        let dialog = create_hidden(s(true, true, true), rec.clone(), on_destroyed).expect("dialog");
        // SAFETY: plain queries on the test thread's dialog.
        unsafe {
            assert!(!IsWindowVisible(dialog).as_bool());
            assert_eq!(
                IsDlgButtonChecked(dialog, i32::from(IDC_RESTORE)),
                BST_CHECKED.0
            );
            assert_eq!(
                IsDlgButtonChecked(dialog, i32::from(IDC_START_AT_SIGN_IN)),
                BST_CHECKED.0
            );
            assert_eq!(
                IsDlgButtonChecked(dialog, i32::from(IDC_TAB_FOLDING)),
                BST_CHECKED.0
            );
            let ask = GetDlgItem(Some(dialog), i32::from(IDC_ASK_AGAIN)).unwrap();
            assert!(IsWindowEnabled(ask).as_bool());

            // Toggle "Open folders as tabs" off and back on; each posts at once.
            CheckDlgButton(dialog, i32::from(IDC_TAB_FOLDING), BST_UNCHECKED).unwrap();
            click(dialog, IDC_TAB_FOLDING);
            let folding_off = Settings {
                tab_folding_enabled: false,
                ..s(true, true, true)
            };
            assert_eq!(
                rec.events(),
                vec![Event::SettingsChanged(folding_off.clone())]
            );
            CheckDlgButton(dialog, i32::from(IDC_TAB_FOLDING), BST_CHECKED).unwrap();
            click(dialog, IDC_TAB_FOLDING);
            assert_eq!(
                rec.events().last(),
                Some(&Event::SettingsChanged(s(true, true, true)))
            );

            // Toggle "Restore my File Explorer session" off.
            CheckDlgButton(dialog, i32::from(IDC_RESTORE), BST_UNCHECKED).unwrap();
            click(dialog, IDC_RESTORE);
            assert_eq!(
                rec.events().last(),
                Some(&Event::SettingsChanged(s(false, true, true)))
            );
            assert_eq!(rec.events().len(), 3);

            // Re-enable the prompt: the decline is cleared and the button disabled.
            click(dialog, IDC_ASK_AGAIN);
            assert_eq!(
                rec.events().last(),
                Some(&Event::SettingsChanged(s(false, true, false)))
            );
            assert!(!IsWindowEnabled(ask).as_bool());

            // A refresh from elsewhere updates the controls without posting.
            refresh(dialog, s(true, false, true));
            assert_eq!(
                IsDlgButtonChecked(dialog, i32::from(IDC_RESTORE)),
                BST_CHECKED.0
            );
            assert_eq!(
                IsDlgButtonChecked(dialog, i32::from(IDC_START_AT_SIGN_IN)),
                BST_UNCHECKED.0
            );
            assert!(IsWindowEnabled(ask).as_bool());
            assert_eq!(rec.events().len(), 4);

            // Exit posts ExitRequested and destroys the dialog.
            click(dialog, IDC_EXIT);
            assert_eq!(rec.events().last(), Some(&Event::ExitRequested));
            assert!(!IsWindow(Some(dialog)).as_bool());
        }
        assert_eq!(DESTROYED.load(Ordering::SeqCst), 1);
    }
}
