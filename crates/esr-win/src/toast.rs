//! `ToastNotifier`: non-activating omitted-tab toast (Requirement 9.8;
//! design.md, "Notification").
//!
//! When a restoration completes with at least one Tab omitted, the engine
//! emits one `Command::Notify { omitted_tabs }`. The runtime hands it to
//! [`ToastNotifier::notify_omitted`], which shows a single WinRT toast:
//!
//! - title: `"1 tab couldn't be restored"` / `"N tabs couldn't be restored"`;
//! - body: a short pointer to the Activity_Log (`activity.log`).
//!
//! # Non-modal, never takes focus
//!
//! Toasts are drawn by the shell's notification UI, which never takes keyboard
//! focus from the foreground window. The template uses the default scenario
//! and `duration="short"` (no `reminder`/`alarm`, which stay on screen), and
//! carries no `launch` argument, so it starts nothing when clicked.
//!
//! # Replacing instead of stacking
//!
//! Every toast carries the same [`TOAST_TAG`] / [`TOAST_GROUP`], so a later
//! restoration replaces the previous toast in Action Center instead of adding a
//! second one. Each toast also expires from Action Center after
//! [`TOAST_EXPIRATION`]; the Activity_Log stays the durable record.
//!
//! # AUMID dependency (task 14.5)
//!
//! A desktop (unpackaged) app can only show toasts under an AppUserModelID that
//! is registered on a Start-menu shortcut. `esr.exe --install` (task 14.5)
//! creates the per-user shortcut carrying `System.AppUserModel.ID =`
//! [`AUMID`]; this module only consumes it and never creates shortcuts or
//! registry entries.
//!
//! Without that shortcut, Windows either returns an error (typically from
//! `ToastNotifier.Setting` or `Show`) or accepts the toast and silently drops
//! it. Both are handled without panicking: errors become a [`ShellError`],
//! and a failing `Setting` query is reported as an error even when `Show`
//! succeeds, because the toast was then most likely dropped. Callers only log
//! the error (task 13.12: "Failures are logged only").

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use esr_core::engine::ShellError;
use esr_core::logfmt::ACTIVITY_LOG_FILE_NAME;
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::{DateTime, IReference, PropertyValue};
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager,
};
use windows::core::{HSTRING, Interface};

/// AppUserModelID the toasts are shown under. The installer (task 14.5) must
/// put exactly this value on the per-user Start-menu shortcut.
pub const AUMID: &str = "ExplorerSessionRestore";

/// Tag shared by every omitted-tab toast, so a new one replaces the old one.
pub const TOAST_TAG: &str = "omitted-tabs";

/// Group shared by every omitted-tab toast.
pub const TOAST_GROUP: &str = "restore";

/// How long a toast stays in Action Center.
pub const TOAST_EXPIRATION: Duration = Duration::from_secs(60 * 60);

/// 100-ns intervals between 1601-01-01 (WinRT `DateTime` epoch) and 1970-01-01.
const UNIX_EPOCH_AS_WINRT_TICKS: i64 = 116_444_736_000_000_000;

/// Escape `text` for use as XML character data or an attribute value.
pub fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Title line: the number of omitted Tabs, with singular/plural wording.
pub fn toast_title(omitted: u32) -> String {
    if omitted == 1 {
        "1 tab couldn't be restored".to_owned()
    } else {
        format!("{omitted} tabs couldn't be restored")
    }
}

/// Secondary line pointing to the Activity_Log.
pub fn toast_body() -> String {
    format!("See the activity log ({ACTIVITY_LOG_FILE_NAME}) for details.")
}

/// ToastGeneric XML for the omitted-tab toast (all text escaped).
pub fn toast_xml(omitted: u32) -> String {
    format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text>\
         </binding></visual></toast>",
        escape_xml(&toast_title(omitted)),
        escape_xml(&toast_body()),
    )
}

/// `now + after` as a WinRT `DateTime` (100-ns ticks since 1601-01-01 UTC).
fn winrt_time_after(now: SystemTime, after: Duration) -> DateTime {
    let since_unix = now.duration_since(UNIX_EPOCH).unwrap_or_default() + after;
    let ticks =
        i64::try_from(since_unix.as_nanos() / 100).unwrap_or(i64::MAX - UNIX_EPOCH_AS_WINRT_TICKS);
    DateTime {
        UniversalTime: UNIX_EPOCH_AS_WINRT_TICKS + ticks,
    }
}

fn shell_error(step: &str, e: &windows::core::Error) -> ShellError {
    ShellError {
        code: e.code().0,
        message: format!("toast: {step}: {}", e.message()),
    }
}

/// Build (but do not show) the toast: XML, tag, group and expiration.
fn build_notification(omitted: u32) -> Result<ToastNotification, ShellError> {
    let doc = XmlDocument::new().map_err(|e| shell_error("XmlDocument", &e))?;
    doc.LoadXml(&HSTRING::from(toast_xml(omitted)))
        .map_err(|e| shell_error("LoadXml", &e))?;
    let toast = ToastNotification::CreateToastNotification(&doc)
        .map_err(|e| shell_error("CreateToastNotification", &e))?;
    toast
        .SetTag(&HSTRING::from(TOAST_TAG))
        .map_err(|e| shell_error("SetTag", &e))?;
    toast
        .SetGroup(&HSTRING::from(TOAST_GROUP))
        .map_err(|e| shell_error("SetGroup", &e))?;
    let expires = winrt_time_after(SystemTime::now(), TOAST_EXPIRATION);
    let expires: IReference<DateTime> = PropertyValue::CreateDateTime(expires)
        .and_then(|v| v.cast())
        .map_err(|e| shell_error("ExpirationTime", &e))?;
    toast
        .SetExpirationTime(&expires)
        .map_err(|e| shell_error("SetExpirationTime", &e))?;
    Ok(toast)
}

/// Shows the omitted-tab toast under [`AUMID`].
#[derive(Debug, Default)]
pub struct ToastNotifier {
    _private: (),
}

impl ToastNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Show one toast stating `omitted` omitted Tabs. `0` shows nothing
    /// (Req 7.4: no notification when nothing was omitted).
    ///
    /// Never panics. Returns an error when Windows rejects the toast, when
    /// toasts are disabled for [`AUMID`] (nothing is shown then), or when the
    /// notification setting cannot be read, which usually means the AUMID
    /// shortcut is missing and the toast was silently dropped.
    pub fn notify_omitted(&self, omitted: u32) -> Result<(), ShellError> {
        if omitted == 0 {
            return Ok(());
        }
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
            .map_err(|e| shell_error("CreateToastNotifierWithId", &e))?;
        let setting_error = match notifier.Setting() {
            Ok(NotificationSetting::Enabled) => None,
            Ok(other) => {
                return Err(ShellError {
                    code: 0,
                    message: format!(
                        "toast: notifications are disabled for {AUMID} ({})",
                        setting_name(other)
                    ),
                });
            }
            Err(e) => Some(e),
        };
        let toast = build_notification(omitted)?;
        notifier.Show(&toast).map_err(|e| shell_error("Show", &e))?;
        match setting_error {
            None => Ok(()),
            Some(e) => Err(shell_error(
                "Setting (AUMID shortcut probably missing; the toast may have been dropped)",
                &e,
            )),
        }
    }
}

fn setting_name(setting: NotificationSetting) -> &'static str {
    match setting {
        NotificationSetting::Enabled => "Enabled",
        NotificationSetting::DisabledForApplication => "DisabledForApplication",
        NotificationSetting::DisabledForUser => "DisabledForUser",
        NotificationSetting::DisabledByGroupPolicy => "DisabledByGroupPolicy",
        NotificationSetting::DisabledByManifest => "DisabledByManifest",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text of every `<text>` element, unescaped by the platform XML parser.
    fn parsed_texts(xml: &str) -> Vec<String> {
        let doc = XmlDocument::new().unwrap();
        doc.LoadXml(&HSTRING::from(xml))
            .expect("toast XML must be well-formed");
        let nodes = doc.GetElementsByTagName(&HSTRING::from("text")).unwrap();
        (0..nodes.Length().unwrap())
            .map(|i| nodes.Item(i).unwrap().InnerText().unwrap().to_string())
            .collect()
    }

    #[test]
    fn wording_is_singular_for_one_and_plural_otherwise() {
        assert_eq!(toast_title(1), "1 tab couldn't be restored");
        assert_eq!(toast_title(3), "3 tabs couldn't be restored");
        assert_eq!(toast_title(0), "0 tabs couldn't be restored");
        assert_eq!(
            toast_title(u32::MAX),
            format!("{} tabs couldn't be restored", u32::MAX)
        );
    }

    #[test]
    fn body_points_to_activity_log() {
        assert!(toast_body().contains("activity.log"));
    }

    #[test]
    fn escape_xml_escapes_all_special_characters() {
        assert_eq!(
            escape_xml("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(escape_xml("plain 12"), "plain 12");
        assert_eq!(escape_xml(""), "");
    }

    #[test]
    fn toast_xml_is_escaped_toast_generic_without_launch() {
        let xml = toast_xml(1);
        assert!(xml.starts_with("<toast "));
        assert!(xml.contains("template=\"ToastGeneric\""));
        assert!(xml.contains("<text>1 tab couldn&apos;t be restored</text>"));
        assert!(
            !xml.contains("couldn't"),
            "apostrophe must be escaped: {xml}"
        );
        assert!(!xml.contains("launch="), "toast must not activate anything");
        assert!(!xml.contains("scenario="), "default scenario only");
    }

    #[test]
    fn toast_xml_is_well_formed_and_round_trips_text() {
        for omitted in [1, 2, 3, 10, 500, u32::MAX] {
            let texts = parsed_texts(&toast_xml(omitted));
            assert_eq!(
                texts,
                vec![toast_title(omitted), toast_body()],
                "omitted = {omitted}"
            );
        }
    }

    #[test]
    fn notification_is_built_with_tag_group_and_expiration() {
        let before = winrt_time_after(SystemTime::now(), TOAST_EXPIRATION).UniversalTime;
        let toast = build_notification(2).unwrap();
        let after = winrt_time_after(SystemTime::now(), TOAST_EXPIRATION).UniversalTime;
        assert_eq!(toast.Tag().unwrap().to_string(), TOAST_TAG);
        assert_eq!(toast.Group().unwrap().to_string(), TOAST_GROUP);
        let expires = toast
            .ExpirationTime()
            .unwrap()
            .Value()
            .unwrap()
            .UniversalTime;
        assert!(
            (before..=after).contains(&expires),
            "{before} <= {expires} <= {after}"
        );
    }

    #[test]
    fn winrt_time_uses_1601_epoch() {
        assert_eq!(
            winrt_time_after(UNIX_EPOCH, Duration::ZERO).UniversalTime,
            UNIX_EPOCH_AS_WINRT_TICKS
        );
        assert_eq!(
            winrt_time_after(UNIX_EPOCH, Duration::from_secs(1)).UniversalTime,
            UNIX_EPOCH_AS_WINRT_TICKS + 10_000_000
        );
    }

    #[test]
    fn zero_omitted_shows_nothing() {
        // Returns before touching WinRT, so this never shows a toast.
        assert_eq!(ToastNotifier::new().notify_omitted(0), Ok(()));
    }

    /// Manual: shows one real toast. Needs the AUMID shortcut from
    /// `esr.exe --install`; without it, expect an error or no visible toast.
    /// Run with `cargo test -p esr-win --lib toast::tests::manual_show_one_toast -- --ignored`.
    #[test]
    #[ignore = "manual: shows a real toast"]
    fn manual_show_one_toast() {
        let result = ToastNotifier::new().notify_omitted(3);
        eprintln!("notify_omitted(3) -> {result:?}");
    }
}
