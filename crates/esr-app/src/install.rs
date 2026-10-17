//! Per-user `--install` / `--uninstall` without elevation (Requirements 1.8,
//! 12.6; design.md "Install/uninstall").
//!
//! # What is created and removed
//!
//! | Item | Location |
//! |---|---|
//! | Program | `%LOCALAPPDATA%\Programs\ExplorerSessionRecall\esr.exe` |
//! | Start-menu shortcut | `<per-user Start menu>\Programs\Explorer Session Recall.lnk`, carrying `System.AppUserModel.ID =` [`AUMID`] (needed for toasts) |
//! | Start at sign-in | logon task `\ExplorerSessionRecall\<SID>` (or the root / Run-key fallbacks of `esr_win::startup`) |
//! | Uninstall entry | `HKCU\`[`UNINSTALL_SUBKEY`] |
//! | Data (uninstall only) | Session_File, its backup, settings, Activity_Log, Previous_Log_File, their temp files, and the placeholders of deferred tabs (`deferred\<n>\*.library-ms`) in `%LOCALAPPDATA%\ExplorerSessionRecall` |
//! | Old build (install only) | The v0.1 "Explorer Session Restore" items: its logon task / Run value `ExplorerSessionRestore`, `Explorer Session Restore.lnk`, `HKCU\...\Uninstall\ExplorerSessionRestore`, and `%LOCALAPPDATA%\Programs\ExplorerSessionRestore\esr.exe` are removed; its data folder `%LOCALAPPDATA%\ExplorerSessionRestore` is moved to the new data folder |
//!
//! Everything is under the user's profile or `HKCU`, so no administrator
//! rights or UAC prompt are needed (Req 1.8).
//!
//! Debug builds can redirect every location to test folders, a test task
//! name, and a test Installed-apps entry with the overrides file of
//! [`crate::dev_overrides`] ([`Layout::for_exe`]); `tests/smoke.rs` (task
//! 16.7) installs and uninstalls that way.
//!
//! # Structure
//!
//! [`install_plan`], [`uninstall_plan`], and [`finish_plan`] are pure: they
//! turn a [`Layout`] (every path and key, injectable) into a list of
//! [`Step`]s. [`execute`] runs a plan with any step runner and stops at the
//! first failing [fatal](Step::is_fatal) step; every other failure is
//! collected and the plan continues. [`System`] runs the steps for real, each
//! through one small function in this module. Tests exercise the plans and
//! run the primitives against temp directories and throwaway registry keys
//! only.
//!
//! # Stopping the running instance
//!
//! Both commands first make sure no other instance runs: unless this process
//! already holds the single-instance mutex, it sends
//! [`MSG_EXIT`](crate::instance::MSG_EXIT) to the `ESR.Engine` window and
//! waits (up to [`STOP_TIMEOUT`]) until it can take the mutex itself. The
//! running instance handles the message like Exit in Settings, so every
//! Explorer window stays open (Req 12.6) and its final Session_File write is
//! finished before the files are deleted. If it does not stop in time, the
//! command is cancelled without changing anything. The exit message is sent
//! again about every second while waiting, in case it reached another
//! `ESR.Engine` window (an old build's) first.
//!
//! # Migrating from the old build
//!
//! The product was called "Explorer Session Restore" in v0.1 (see
//! [`LegacyLayout`]). `--install` then also stops a running old instance
//! (same `ESR.Engine` window and `ESR_EXIT` message, old mutex
//! [`LEGACY_MUTEX_PREFIX`](crate::instance::LEGACY_MUTEX_PREFIX)); that step
//! is fatal, so data is never moved while the old app could still write it.
//! It then removes the old registration, shortcut, Installed-apps entry and
//! program, and renames the old data folder to the new one
//! ([`migrate_data_dir`]) if the new one does not exist yet. A rename on the
//! same volume keeps the protected DACL and the deferred-tab placeholders.
//! Every migration step except the stop is best effort: a failure is
//! reported but never cancels the install, and the old data is never deleted
//! (if the rename fails, it stays where it was).
//!
//! # Removing the program itself
//!
//! A running image cannot be deleted. When `--uninstall` runs from the install
//! directory, it copies itself to `%TEMP%\ESR-uninstall-<pid>\esr.exe`, starts
//! that copy with `--uninstall-finish <pid>`, and exits. The copy waits for
//! the original to end, deletes `esr.exe` and the then empty install
//! directory, and finally starts a hidden `cmd.exe` that waits two seconds
//! and deletes the copy and its folder. `--uninstall-finish` only ever deletes
//! the fixed install location and a copy that lives under `%TEMP%`.
//!
//! # Wiring (task 14.7)
//!
//! `main` calls [`Invocation::from_args`] after the version gate and the
//! single-instance check and, for `Some`, hands the [`InstanceRole`] to
//! [`run`] and exits with its result. `--install` finishes by starting the
//! installed copy with `--background`, after releasing the mutex.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use esr_core::logfmt::{ACTIVITY_LOG_FILE_NAME, PREVIOUS_LOG_FILE_NAME};
use esr_core::plan::defer::PLACEHOLDER_EXTENSION;
use esr_core::settings::Settings;
use esr_core::store::{BACKUP_FILE, SESSION_FILE};
use esr_win::file_store::{DATA_DIR_NAME, SETTINGS_FILE};
use esr_win::shell_util::StaGuard;
use esr_win::startup::{APP_STARTUP_NAME, StartupConfig, StartupRegistration, quote_program_path};
use esr_win::toast::AUMID;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, PROPERTYKEY, WAIT_OBJECT_0,
    WIN32_ERROR,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    SHStrDupW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MessageBoxW,
};
use windows::core::{GUID, HSTRING, Interface};

use crate::dev_overrides::{self, DevOverrides};
use crate::instance::{
    self, ENGINE_WINDOW_CLASS, InstanceGuard, InstanceRole, SecondaryAction, SignalOutcome,
};
use crate::runtime::PLACEHOLDER_SUBDIR;

/// Product name shown in the Start menu and in Installed apps.
pub const PRODUCT_NAME: &str = "Explorer Session Recall";
/// Install directory name under `%LOCALAPPDATA%\Programs`.
pub const INSTALL_DIR_NAME: &str = "ExplorerSessionRecall";
/// File name of the installed program.
pub const EXE_NAME: &str = "esr.exe";
/// File name of the per-user Start-menu shortcut.
pub const SHORTCUT_FILE_NAME: &str = "Explorer Session Recall.lnk";
/// `HKCU` subkey of the Installed-apps (Add/Remove Programs) entry.
pub const UNINSTALL_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ExplorerSessionRecall";
/// Parent of [`UNINSTALL_SUBKEY`].
pub const UNINSTALL_PARENT_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
/// Prefix of the `%TEMP%` folder holding the self-deleting copy.
pub const HANDOFF_DIR_PREFIX: &str = "ESR-uninstall-";
/// Name of the old (v0.1) build: its install-dir name, data-dir name,
/// start-at-sign-in base name, and Uninstall subkey leaf.
pub const LEGACY_NAME: &str = "ExplorerSessionRestore";
/// File name of the old (v0.1) Start-menu shortcut.
pub const LEGACY_SHORTCUT_FILE_NAME: &str = "Explorer Session Restore.lnk";

pub const INSTALL_FLAG: &str = "--install";
pub const UNINSTALL_FLAG: &str = "--uninstall";
/// Internal: run by the `%TEMP%` copy, followed by the original's PID.
pub const FINISH_UNINSTALL_FLAG: &str = "--uninstall-finish";
/// No message box at the end (used by `QuietUninstallString` and by Setup).
/// `--install` still shows a message box when something failed, so the
/// failed steps are never hidden.
pub const QUIET_FLAG: &str = "--quiet";

/// Exit code: a fatal step failed and the command was cancelled, or it could
/// not start at all.
pub const EXIT_FAILED: i32 = 1;
/// Exit code: the command finished, but a best-effort step failed. (`2` is
/// the unsupported-build exit code of `main`.)
pub const EXIT_COMPLETED_WITH_WARNINGS: i32 = 3;

/// How long to wait for a running instance to exit (Req 12.5 allows it 5 s).
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the `%TEMP%` copy waits for the original process to end.
pub const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Seconds the final `cmd.exe` waits before deleting the `%TEMP%` copy.
pub const SELF_DELETE_DELAY_SECS: u32 = 2;
/// Retries for deleting a file that is still briefly in use.
const REMOVE_RETRY_BUDGET: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(100);

/// `System.AppUserModel.ID` (`PKEY_AppUserModel_ID`, propkey.h).
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

/// `CREATE_NO_WINDOW`: the final `cmd.exe` gets no console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

/// Which install command was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Install,
    Uninstall,
    /// Run by the `%TEMP%` copy; `parent_pid` is the original `--uninstall`
    /// process, if it was given.
    FinishUninstall { parent_pid: Option<u32> },
}

/// A parsed install command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    /// [`QUIET_FLAG`] was given: no message box at the end (except for a
    /// failed `--install`).
    pub quiet: bool,
}

impl Invocation {
    /// The install command in `args`, or `None` for a normal start. The first
    /// command flag wins; including the program name in `args` is harmless.
    pub fn from_args<I, S>(args: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<S> = args.into_iter().collect();
        let quiet = args.iter().any(|a| a.as_ref() == QUIET_FLAG);
        let command = args.iter().enumerate().find_map(|(i, a)| match a.as_ref() {
            INSTALL_FLAG => Some(Command::Install),
            UNINSTALL_FLAG => Some(Command::Uninstall),
            FINISH_UNINSTALL_FLAG => Some(Command::FinishUninstall {
                parent_pid: args.get(i + 1).and_then(|p| p.as_ref().parse().ok()),
            }),
            _ => None,
        })?;
        Some(Self { command, quiet })
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Every path and registry key the commands touch. Injectable so tests never
/// touch the real profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// `%LOCALAPPDATA%\Programs\ExplorerSessionRecall`.
    pub install_dir: PathBuf,
    /// `%LOCALAPPDATA%\ExplorerSessionRecall` (Session_File, settings, logs).
    pub data_dir: PathBuf,
    /// The Start-menu shortcut file.
    pub shortcut: PathBuf,
    /// Uninstall entry subkey under `HKCU`.
    pub uninstall_key: String,
    /// `%TEMP%`.
    pub temp_dir: PathBuf,
    /// Base name of the start-at-sign-in registration
    /// ([`APP_STARTUP_NAME`]; see `StartupConfig::with_base_name`).
    pub startup_name: String,
    /// Names of files that travel with the program: copied next to the
    /// installed program and next to the `%TEMP%` uninstall copy, and
    /// deleted with them. Empty, except for the test overrides file of
    /// debug builds ([`Layout::with_overrides`]).
    pub carried_files: Vec<String>,
    /// Where the old (v0.1) build lives, for migration by `--install`.
    /// `None` with test overrides, so tests never touch a real installation.
    pub legacy: Option<LegacyLayout>,
}

/// The locations of the old (v0.1) "Explorer Session Restore" build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyLayout {
    /// `%LOCALAPPDATA%\Programs\ExplorerSessionRestore`.
    pub install_dir: PathBuf,
    /// `%LOCALAPPDATA%\ExplorerSessionRestore`.
    pub data_dir: PathBuf,
    /// `<Start menu>\Programs\Explorer Session Restore.lnk`.
    pub shortcut: PathBuf,
    /// Its Uninstall entry subkey under `HKCU`.
    pub uninstall_key: String,
    /// Its start-at-sign-in base name.
    pub startup_name: String,
}

impl Layout {
    /// The layout below the given roots (`%LOCALAPPDATA%`, the per-user
    /// Start menu `Programs` folder, and `%TEMP%`).
    pub fn from_roots(local_app_data: &Path, start_menu_programs: &Path, temp_dir: &Path) -> Self {
        Self {
            install_dir: local_app_data.join("Programs").join(INSTALL_DIR_NAME),
            data_dir: local_app_data.join(DATA_DIR_NAME),
            shortcut: start_menu_programs.join(SHORTCUT_FILE_NAME),
            uninstall_key: UNINSTALL_SUBKEY.to_owned(),
            temp_dir: temp_dir.to_owned(),
            startup_name: APP_STARTUP_NAME.to_owned(),
            carried_files: Vec::new(),
            legacy: Some(LegacyLayout {
                install_dir: local_app_data.join("Programs").join(LEGACY_NAME),
                data_dir: local_app_data.join(LEGACY_NAME),
                shortcut: start_menu_programs.join(LEGACY_SHORTCUT_FILE_NAME),
                uninstall_key: format!(r"{UNINSTALL_PARENT_SUBKEY}\{LEGACY_NAME}"),
                startup_name: LEGACY_NAME.to_owned(),
            }),
        }
    }

    /// The real layout for the current user.
    pub fn for_current_user() -> windows::core::Result<Self> {
        Ok(Self::from_roots(
            &known_folder(&FOLDERID_LocalAppData)?,
            &known_folder(&FOLDERID_Programs)?,
            &std::env::temp_dir(),
        ))
    }

    /// The layout for the running program `exe`: the real one, or in debug
    /// builds the one of the test overrides file next to `exe`
    /// ([`Layout::with_overrides`]).
    pub fn for_exe(exe: &Path) -> Result<Self, String> {
        let base = Self::for_current_user()
            .map_err(|e| format!("Couldn't find your profile folders: {}", e.message()))?;
        base.with_overrides(&DevOverrides::for_exe(exe))
    }

    /// Applies test overrides (debug builds; see `crate::dev_overrides`).
    /// No overrides: `self` unchanged. Otherwise every location must be
    /// overridden (data, program and Start-menu folders, and the test name,
    /// which also names the Installed-apps entry), so a test copy never
    /// touches a real installation; the overrides file is then carried
    /// along with the program.
    pub fn with_overrides(mut self, o: &DevOverrides) -> Result<Self, String> {
        if o.is_default() {
            return Ok(self);
        }
        let (Some(data_dir), Some(name), Some(install_dir), Some(start_menu)) =
            (&o.data_dir, &o.startup_name, &o.install_dir, &o.start_menu_dir)
        else {
            return Err(format!(
                "The test overrides file {} must set data_dir, startup_name, install_dir, and \
                 start_menu_dir for --install and --uninstall: {o:?}",
                dev_overrides::FILE_NAME
            ));
        };
        self.data_dir = data_dir.clone();
        self.install_dir = install_dir.clone();
        self.shortcut = start_menu.join(SHORTCUT_FILE_NAME);
        self.uninstall_key = format!(r"{UNINSTALL_PARENT_SUBKEY}\{name}");
        self.startup_name = name.clone();
        self.carried_files = vec![dev_overrides::FILE_NAME.to_owned()];
        self.legacy = None;
        Ok(self)
    }

    /// The installed program.
    pub fn installed_exe(&self) -> PathBuf {
        self.install_dir.join(EXE_NAME)
    }

    /// The settings file in the data directory.
    pub fn settings_file(&self) -> PathBuf {
        self.data_dir.join(SETTINGS_FILE.target)
    }

    /// The placeholder directory of deferred tabs (Req 5.13) in the data
    /// directory: `<data_dir>\deferred`.
    pub fn placeholder_dir(&self) -> PathBuf {
        self.data_dir.join(PLACEHOLDER_SUBDIR)
    }

    /// Every file the app writes in the data directory (Req 12.6): the
    /// Session_File, its backup, settings, both logs, and their temp files.
    pub fn data_files(&self) -> Vec<PathBuf> {
        [
            SESSION_FILE.target,
            SESSION_FILE.temp,
            BACKUP_FILE.target,
            BACKUP_FILE.temp,
            SETTINGS_FILE.target,
            SETTINGS_FILE.temp,
            ACTIVITY_LOG_FILE_NAME,
            PREVIOUS_LOG_FILE_NAME,
        ]
        .iter()
        .map(|name| self.data_dir.join(name))
        .collect()
    }

    /// Where the `%TEMP%` copy of process `pid` lives.
    pub fn handoff_exe(&self, pid: u32) -> PathBuf {
        self.temp_dir.join(format!("{HANDOFF_DIR_PREFIX}{pid}")).join(EXE_NAME)
    }
}

fn known_folder(id: &GUID) -> windows::core::Result<PathBuf> {
    // SAFETY: the returned string is copied and then freed exactly once.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(PathBuf::from(s.map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_UNEXPECTED))?))
    }
}

/// Normalised form for comparing Windows paths: `\` separators, no `\\?\`
/// prefix, no trailing separator, case-folded.
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.trim_end_matches('\\').to_lowercase()
}

/// Whether `a` and `b` name the same path (case-insensitively).
pub fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// Whether `path` lies strictly inside `dir`.
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    let dir = path_key(dir);
    !dir.is_empty() && path_key(path).starts_with(&format!("{dir}\\"))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// A registry value of the uninstall entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegValue {
    Sz(String),
    Dword(u32),
}

/// The Start-menu shortcut to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub path: PathBuf,
    pub target: PathBuf,
    pub working_dir: PathBuf,
    pub description: String,
    /// `System.AppUserModel.ID`.
    pub aumid: String,
}

/// One install or uninstall action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Make sure no other instance runs (see the module documentation).
    StopRunningInstance,
    /// Make sure no old (v0.1) instance runs (its mutex is free).
    StopLegacyInstance,
    /// Move the old data folder `from` to `to` unless `to` exists
    /// ([`migrate_data_dir`]).
    MigrateData { from: PathBuf, to: PathBuf },
    /// Copy the program, or a file that travels with it
    /// ([`Layout::carried_files`]), creating the target directory.
    CopyExe { from: PathBuf, to: PathBuf },
    CreateShortcut(ShortcutSpec),
    /// Register start at sign-in for `exe`, enabled or disabled, under the
    /// base name `name` ([`Layout::startup_name`]).
    RegisterStartup { exe: PathBuf, enabled: bool, name: String },
    WriteUninstallEntry { key: String, values: Vec<(String, RegValue)> },
    /// Release the single-instance mutex and start `exe --background`.
    StartInstalled { exe: PathBuf },
    /// Remove the logon task (both locations) and the Run value of the base
    /// name `name`.
    UnregisterStartup { exe: PathBuf, name: String },
    /// Delete a file; a missing file is not an error.
    RemoveFile(PathBuf),
    /// Delete the placeholders of deferred tabs (`<dir>\<n>\*.library-ms`),
    /// then the numbered folders and `dir` if they are empty. Missing is not
    /// an error; anything else in `dir` is left alone.
    RemovePlaceholders(PathBuf),
    /// Delete a directory if it is empty; a missing directory is not an error.
    RemoveDirIfEmpty(PathBuf),
    /// Delete an `HKCU` key and everything below it; missing is not an error.
    RemoveRegistryKey(String),
    /// Copy the running program to `to` and start it with
    /// `--uninstall-finish <this pid>` in `cwd`.
    HandOff { from: PathBuf, to: PathBuf, cwd: PathBuf },
    /// Start a hidden `cmd.exe` in `cwd` that deletes `exe` and then `dir`
    /// after this process has ended.
    ScheduleSelfDelete { exe: PathBuf, dir: PathBuf, cwd: PathBuf },
}

impl Step {
    /// A failing fatal step cancels the rest of the plan: nothing after it
    /// can work (or would be safe) without it.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Step::StopRunningInstance | Step::StopLegacyInstance | Step::CopyExe { .. })
    }

    /// Short description for the result message.
    pub fn describe(&self) -> String {
        match self {
            Step::StopRunningInstance => "Stop the running Explorer Session Recall".into(),
            Step::StopLegacyInstance => "Stop the old Explorer Session Restore".into(),
            Step::MigrateData { from, to } => {
                format!("Move your saved session from {} to {}", from.display(), to.display())
            }
            Step::CopyExe { to, .. } => format!("Copy the program to {}", to.display()),
            Step::CreateShortcut(s) => format!("Create the Start menu shortcut {}", s.path.display()),
            Step::RegisterStartup { .. } => "Register start at sign-in".into(),
            Step::WriteUninstallEntry { .. } => "Add the Installed apps entry".into(),
            Step::StartInstalled { .. } => "Start Explorer Session Recall".into(),
            Step::UnregisterStartup { .. } => "Remove start at sign-in".into(),
            Step::RemoveFile(p) => format!("Delete {}", p.display()),
            Step::RemovePlaceholders(p) => format!("Delete the placeholders in {}", p.display()),
            Step::RemoveDirIfEmpty(p) => format!("Delete the folder {}", p.display()),
            Step::RemoveRegistryKey(_) => "Remove the Installed apps entry".into(),
            Step::HandOff { .. } => "Remove the program folder".into(),
            Step::ScheduleSelfDelete { .. } => "Delete the temporary uninstaller".into(),
        }
    }
}

/// Inputs of [`install_plan`] that come from the running process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallInputs {
    /// The running program (`std::env::current_exe`).
    pub current_exe: PathBuf,
    /// Its size in bytes (for `EstimatedSize`).
    pub exe_size: u64,
    /// The saved "Start when I sign in" option (default on), so a reinstall
    /// keeps a disabled option disabled (Req 1.9).
    pub start_at_sign_in: bool,
    /// Version shown in Installed apps.
    pub version: String,
}

/// "Start when I sign in" from the saved settings text (`None` = no file).
pub fn start_at_sign_in_from(settings_text: Option<&str>) -> bool {
    settings_text.map_or(Settings::default(), Settings::parse_or_default).start_at_sign_in
}

/// The values of the Installed-apps entry.
pub fn uninstall_values(
    layout: &Layout,
    version: &str,
    exe_size: u64,
) -> Result<Vec<(String, RegValue)>, String> {
    let exe = layout.installed_exe();
    let quoted = quote_program_path(&exe).map_err(|e| e.message())?;
    let size_kb = u32::try_from(exe_size.div_ceil(1024)).unwrap_or(u32::MAX);
    let sz = |name: &str, value: String| (name.to_owned(), RegValue::Sz(value));
    Ok(vec![
        sz("DisplayName", PRODUCT_NAME.to_owned()),
        sz("DisplayVersion", version.to_owned()),
        sz("DisplayIcon", format!("{},0", exe.display())),
        sz("InstallLocation", layout.install_dir.display().to_string()),
        sz("UninstallString", format!("{quoted} {UNINSTALL_FLAG}")),
        sz("QuietUninstallString", format!("{quoted} {UNINSTALL_FLAG} {QUIET_FLAG}")),
        ("NoModify".to_owned(), RegValue::Dword(1)),
        ("NoRepair".to_owned(), RegValue::Dword(1)),
        ("EstimatedSize".to_owned(), RegValue::Dword(size_kb)),
    ])
}

/// `--install`: stop a running instance, migrate the old (v0.1) build if
/// [`Layout::legacy`] is set, copy the program (unless it already runs from
/// the install location), create the shortcut, start at sign-in, and the
/// uninstall entry, then start the installed copy.
pub fn install_plan(layout: &Layout, inputs: &InstallInputs) -> Result<Vec<Step>, String> {
    let exe = layout.installed_exe();
    let mut plan = vec![Step::StopRunningInstance];
    if let Some(legacy) = &layout.legacy {
        let legacy_exe = legacy.install_dir.join(EXE_NAME);
        plan.push(Step::StopLegacyInstance);
        plan.push(Step::UnregisterStartup { exe: legacy_exe.clone(), name: legacy.startup_name.clone() });
        plan.push(Step::RemoveFile(legacy.shortcut.clone()));
        plan.push(Step::RemoveRegistryKey(legacy.uninstall_key.clone()));
        plan.push(Step::MigrateData { from: legacy.data_dir.clone(), to: layout.data_dir.clone() });
        // A running image cannot be deleted (only if started from there).
        if !same_path(&inputs.current_exe, &legacy_exe) {
            plan.push(Step::RemoveFile(legacy_exe));
            plan.push(Step::RemoveDirIfEmpty(legacy.install_dir.clone()));
        }
    }
    if !same_path(&inputs.current_exe, &exe) {
        plan.push(Step::CopyExe { from: inputs.current_exe.clone(), to: exe.clone() });
        plan.extend(layout.carried_files.iter().map(|name| Step::CopyExe {
            from: inputs.current_exe.with_file_name(name),
            to: layout.install_dir.join(name),
        }));
    }
    plan.push(Step::CreateShortcut(ShortcutSpec {
        path: layout.shortcut.clone(),
        target: exe.clone(),
        working_dir: layout.install_dir.clone(),
        description: "Reopens your File Explorer windows and tabs after you sign in.".into(),
        aumid: AUMID.to_owned(),
    }));
    plan.push(Step::RegisterStartup {
        exe: exe.clone(),
        enabled: inputs.start_at_sign_in,
        name: layout.startup_name.clone(),
    });
    plan.push(Step::WriteUninstallEntry {
        key: layout.uninstall_key.clone(),
        values: uninstall_values(layout, &inputs.version, inputs.exe_size)?,
    });
    plan.push(Step::StartInstalled { exe });
    Ok(plan)
}

/// `--uninstall`: stop a running instance, remove start at sign-in, the
/// shortcut, the uninstall entry, every data file (Req 12.6) and the
/// placeholders of deferred tabs, then the program. When this process is the
/// installed program, the program folder is removed by a `%TEMP%` copy
/// ([`Step::HandOff`]); `pid` names that copy.
pub fn uninstall_plan(layout: &Layout, current_exe: &Path, pid: u32) -> Vec<Step> {
    let exe = layout.installed_exe();
    let mut plan = vec![
        Step::StopRunningInstance,
        Step::UnregisterStartup { exe: exe.clone(), name: layout.startup_name.clone() },
        Step::RemoveFile(layout.shortcut.clone()),
        Step::RemoveRegistryKey(layout.uninstall_key.clone()),
    ];
    plan.extend(layout.data_files().into_iter().map(Step::RemoveFile));
    plan.push(Step::RemovePlaceholders(layout.placeholder_dir()));
    plan.push(Step::RemoveDirIfEmpty(layout.data_dir.clone()));
    if is_inside(current_exe, &layout.install_dir) {
        let to = layout.handoff_exe(pid);
        // The carried files must be in place before the copy starts.
        plan.extend(layout.carried_files.iter().map(|name| Step::CopyExe {
            from: layout.install_dir.join(name),
            to: to.with_file_name(name),
        }));
        plan.push(Step::HandOff { from: current_exe.to_owned(), to, cwd: layout.temp_dir.clone() });
    } else {
        plan.push(Step::RemoveFile(exe));
        plan.extend(layout.carried_files.iter().map(|name| Step::RemoveFile(layout.install_dir.join(name))));
        plan.push(Step::RemoveDirIfEmpty(layout.install_dir.clone()));
    }
    plan
}

/// `--uninstall-finish` (in the `%TEMP%` copy, after the original ended):
/// delete the installed program and its folder, then this copy if it is a
/// hand-off copy under `%TEMP%`.
pub fn finish_plan(layout: &Layout, current_exe: &Path) -> Vec<Step> {
    let mut plan = vec![Step::RemoveFile(layout.installed_exe())];
    plan.extend(layout.carried_files.iter().map(|name| Step::RemoveFile(layout.install_dir.join(name))));
    plan.push(Step::RemoveDirIfEmpty(layout.install_dir.clone()));
    let own_dir = current_exe.parent().filter(|dir| {
        same_path(dir.parent().unwrap_or(Path::new("")), &layout.temp_dir)
            && dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(HANDOFF_DIR_PREFIX))
    });
    if let Some(dir) = own_dir {
        // The self-delete only removes the copy and then an empty folder.
        plan.extend(layout.carried_files.iter().map(|name| Step::RemoveFile(dir.join(name))));
        plan.push(Step::ScheduleSelfDelete {
            exe: current_exe.to_owned(),
            dir: dir.to_owned(),
            cwd: layout.temp_dir.clone(),
        });
    }
    plan
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// A step that failed, with the error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: Step,
    pub error: String,
}

/// Result of running a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub failures: Vec<Failure>,
    /// A fatal step failed and the remaining steps were not run.
    pub cancelled: bool,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.failures.is_empty()
    }

    /// The process exit code: 0 on success, [`EXIT_FAILED`] if cancelled,
    /// [`EXIT_COMPLETED_WITH_WARNINGS`] if only best-effort steps failed.
    pub fn exit_code(&self) -> i32 {
        if self.succeeded() {
            0
        } else if self.cancelled {
            EXIT_FAILED
        } else {
            EXIT_COMPLETED_WITH_WARNINGS
        }
    }
}

/// Runs `plan` in order with `run_step`. A failing fatal step stops the plan;
/// other failures are recorded and the plan continues.
pub fn execute<F>(plan: &[Step], mut run_step: F) -> Outcome
where
    F: FnMut(&Step) -> Result<(), String>,
{
    let mut outcome = Outcome::default();
    for step in plan {
        if let Err(error) = run_step(step) {
            outcome.failures.push(Failure { step: step.clone(), error });
            if step.is_fatal() {
                outcome.cancelled = true;
                break;
            }
        }
    }
    outcome
}

/// Runs steps against the real system.
pub struct System {
    /// Held while installing/uninstalling so no instance starts meanwhile.
    guard: Option<InstanceGuard>,
    /// Name of the single-instance mutex.
    mutex_name: String,
    /// Name of the old (v0.1) build's single-instance mutex.
    legacy_mutex_name: String,
    stop_timeout: Duration,
}

/// How often the exit message is sent again while waiting for an instance to stop.
const RESIGNAL_INTERVAL: Duration = Duration::from_secs(1);

impl System {
    /// `role` is this process's single-instance role: `Primary` means no
    /// other instance runs.
    pub fn new(role: InstanceRole) -> windows::core::Result<Self> {
        let guard = match role {
            InstanceRole::Primary(guard) => Some(guard),
            InstanceRole::Secondary { .. } => None,
        };
        let sid = instance::current_user_sid()?;
        Ok(Self {
            guard,
            mutex_name: instance::mutex_name(&sid),
            legacy_mutex_name: instance::legacy_mutex_name(&sid),
            stop_timeout: STOP_TIMEOUT,
        })
    }

    pub fn run_step(&mut self, step: &Step) -> Result<(), String> {
        match step {
            Step::StopRunningInstance => {
                if self.guard.is_some() {
                    return Ok(());
                }
                let name = self.mutex_name.clone();
                self.guard = Some(self.stop_instance(&name, PRODUCT_NAME)?);
                Ok(())
            }
            Step::StopLegacyInstance => {
                // Only needed until the old instance is gone; dropped at once.
                let name = self.legacy_mutex_name.clone();
                self.stop_instance(&name, "Explorer Session Restore").map(drop)
            }
            Step::MigrateData { from, to } => migrate_data_dir(from, to).map(|_| ()).map_err(|e| e.to_string()),
            Step::CopyExe { from, to } => copy_exe(from, to).map_err(|e| e.to_string()),
            Step::CreateShortcut(spec) => create_shortcut(spec).map_err(|e| e.message()),
            Step::RegisterStartup { exe, enabled, name } => {
                StartupRegistration::new(startup_config(name, exe)?)
                    .set_enabled(*enabled)
                    .map(|_| ())
                    .map_err(|e| e.message())
            }
            Step::WriteUninstallEntry { key, values } => {
                write_registry_values(key, values).map_err(|e| e.message())
            }
            Step::StartInstalled { exe } => {
                // The new process must be able to take the mutex.
                self.guard = None;
                start_background(exe).map_err(|e| e.to_string())
            }
            Step::UnregisterStartup { exe, name } => {
                StartupRegistration::new(startup_config(name, exe)?).unregister().map_err(|e| e.message())
            }
            Step::RemoveFile(path) => remove_file(path).map_err(|e| e.to_string()),
            Step::RemovePlaceholders(dir) => remove_placeholders(dir).map_err(|e| e.to_string()),
            Step::RemoveDirIfEmpty(path) => remove_dir_if_empty(path).map_err(|e| e.to_string()),
            Step::RemoveRegistryKey(key) => remove_registry_key(key).map_err(|e| e.message()),
            Step::HandOff { from, to, cwd } => hand_off(from, to, cwd).map_err(|e| e.to_string()),
            Step::ScheduleSelfDelete { exe, dir, cwd } => {
                schedule_self_delete(exe, dir, cwd).map_err(|e| e.to_string())
            }
        }
    }

    /// Ask the instance holding the mutex `mutex_name` to exit and wait until
    /// this process can take that mutex; returns the guard (see the module
    /// documentation). `product` names the instance in the timeout error.
    fn stop_instance(&self, mutex_name: &str, product: &str) -> Result<InstanceGuard, String> {
        let deadline = Instant::now() + self.stop_timeout;
        let mut last_signal: Option<Instant> = None;
        let mut find_budget = instance::FIND_WINDOW_BUDGET;
        loop {
            match instance::acquire_named(mutex_name, [UNINSTALL_FLAG]) {
                Ok(InstanceRole::Primary(guard)) => return Ok(guard),
                Ok(InstanceRole::Secondary { .. }) => {}
                Err(e) => return Err(e.message()),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{product} is still running after {} seconds. Exit it from its Settings \
                     and try again.",
                    self.stop_timeout.as_secs()
                ));
            }
            // Signal again about every second: the message may have reached
            // another `ESR.Engine` window (an old build's) first.
            if last_signal.is_none_or(|t| t.elapsed() >= RESIGNAL_INTERVAL) {
                // The instance may still be starting: keep looking briefly.
                let outcome =
                    instance::signal_window_class(ENGINE_WINDOW_CLASS, SecondaryAction::Exit, find_budget);
                if matches!(outcome, SignalOutcome::Delivered { .. }) {
                    last_signal = Some(Instant::now());
                }
                find_budget = POLL;
            }
            thread::sleep(POLL);
        }
    }
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// The start-at-sign-in configuration of base name `name` for the current
/// user (`StartupConfig::for_current_user` for [`APP_STARTUP_NAME`]).
fn startup_config(name: &str, exe: &Path) -> Result<StartupConfig, String> {
    let sid = esr_win::file_store::current_user_sid().map_err(|e| e.message())?;
    Ok(StartupConfig::with_base_name(name, &sid, exe))
}

/// Deletes the placeholders of deferred tabs below `dir`
/// ([`Step::RemovePlaceholders`]): `<dir>\<digits>\*.library-ms`, then each
/// numbered folder and `dir` itself if empty. Missing is success.
pub fn remove_placeholders(dir: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    let mut first_error = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let numbered = name.to_str().is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !numbered || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let slot = entry.path();
        for file in std::fs::read_dir(&slot).into_iter().flatten().flatten() {
            let is_placeholder = file
                .file_name()
                .to_str()
                .is_some_and(|n| n.to_lowercase().ends_with(PLACEHOLDER_EXTENSION));
            if is_placeholder
                && file.file_type().is_ok_and(|t| t.is_file())
                && let Err(e) = remove_file(&file.path())
            {
                first_error.get_or_insert(e);
            }
        }
        if let Err(e) = remove_dir_if_empty(&slot) {
            first_error.get_or_insert(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => remove_dir_if_empty(dir),
    }
}
/// Copy `from` to `to`, creating `to`'s directory. A target that is still
/// in use (the program that just exited) is retried briefly.
pub fn copy_exe(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let deadline = Instant::now() + REMOVE_RETRY_BUDGET;
    loop {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(POLL);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Move the old data folder `from` to `to` ([`Step::MigrateData`]) with one
/// rename, which keeps the folder's protected DACL and everything in it.
/// Nothing is done (`Ok(false)`) if `to` already exists or `from` is not a
/// folder. On failure nothing was moved and `from` is left as it was.
pub fn migrate_data_dir(from: &Path, to: &Path) -> io::Result<bool> {
    if to.exists() || !from.is_dir() {
        return Ok(false);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(from, to)?;
    Ok(true)
}

/// Delete `path`; missing is success. A file that is still in use (the
/// program that just exited) is retried briefly.
pub fn remove_file(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + REMOVE_RETRY_BUDGET;
    loop {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(POLL);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Delete `dir` if it is empty. Missing or non-empty is success (files the
/// app did not create are left alone).
pub fn remove_dir_if_empty(dir: &Path) -> io::Result<()> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Ok(());
            }
        }
    }
    match std::fs::remove_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Start `exe --background` detached, in its own directory.
fn start_background(exe: &Path) -> io::Result<()> {
    let mut command = Process::new(exe);
    command
        .arg(instance::BACKGROUND_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    command.spawn().map(|_| ())
}

/// Copy the running program to `to` and start it with
/// `--uninstall-finish <pid>`.
fn hand_off(from: &Path, to: &Path, cwd: &Path) -> io::Result<()> {
    copy_exe(from, to)?;
    Process::new(to)
        .arg(FINISH_UNINSTALL_FLAG)
        .arg(std::process::id().to_string())
        .arg(QUIET_FLAG)
        // Not the install directory, so it can be deleted.
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Wait until process `pid` has ended. `true` if it ended (or no longer
/// exists) within `timeout`.
pub fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    // SAFETY: the handle is closed once below; a failed open means the
    // process is gone (or was never there).
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return true;
        };
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        let ended = WaitForSingleObject(handle, ms) == WAIT_OBJECT_0;
        let _ = CloseHandle(handle);
        ended
    }
}

fn system_dir() -> io::Result<PathBuf> {
    let mut buf = [0u16; 260];
    // SAFETY: the buffer is valid for its length.
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if len == 0 || len > buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// The `cmd.exe` arguments (after the program name) that wait about
/// `delay_secs` seconds and then delete `exe` and the directory `dir`.
///
/// `None` if a path contains `"` or `%`, which cannot be passed safely
/// inside a quoted `cmd` command.
pub fn self_delete_arguments(
    system_dir: &Path,
    exe: &Path,
    dir: &Path,
    delay_secs: u32,
) -> Option<String> {
    fn safe(p: &Path) -> Option<&str> {
        p.to_str().filter(|s| !s.is_empty() && !s.contains(['"', '%']))
    }
    let ping = system_dir.join("PING.EXE");
    let (ping, exe, dir) = (safe(&ping)?, safe(exe)?, safe(dir)?);
    // `/s`: cmd strips only the outer quotes and runs the rest verbatim.
    Some(format!(
        "/d /v:off /s /c \"\"{ping}\" -n {} 127.0.0.1 >nul & del /f /q \"{exe}\" & rmdir \"{dir}\"\"",
        delay_secs + 1
    ))
}

/// Start the hidden `cmd.exe` that deletes this `%TEMP%` copy once it ended.
fn schedule_self_delete(exe: &Path, dir: &Path, cwd: &Path) -> io::Result<()> {
    let system = system_dir()?;
    let args = self_delete_arguments(&system, exe, dir, SELF_DELETE_DELAY_SECS)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsupported temp path"))?;
    spawn_cmd(&system, &args, cwd).map(|_| ())
}

fn spawn_cmd(system: &Path, args: &str, cwd: &Path) -> io::Result<std::process::Child> {
    Process::new(system.join("cmd.exe"))
        .raw_arg(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

/// Write `values` under `HKCU\<subkey>`, creating the key.
pub fn write_registry_values(subkey: &str, values: &[(String, RegValue)]) -> windows::core::Result<()> {
    let mut key = HKEY::default();
    // SAFETY: valid strings; `key` is closed below.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let result = values.iter().try_for_each(|(name, value)| {
        let (kind, data) = match value {
            RegValue::Sz(s) => {
                let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
                (REG_SZ, wide.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>())
            }
            RegValue::Dword(d) => (REG_DWORD, d.to_le_bytes().to_vec()),
        };
        // SAFETY: `key` is open; `data` is valid for the call.
        unsafe { RegSetValueExW(key, &HSTRING::from(name.as_str()), None, kind, Some(&data)).ok() }
    });
    // SAFETY: opened above, closed once.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn is_missing(code: WIN32_ERROR) -> bool {
    code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
}

/// Delete `HKCU\<subkey>` with everything below it; missing is success.
pub fn remove_registry_key(subkey: &str) -> windows::core::Result<()> {
    let name = HSTRING::from(subkey);
    // SAFETY: plain registry calls with valid strings.
    unsafe {
        let tree = RegDeleteTreeW(HKEY_CURRENT_USER, &name);
        if !is_missing(tree) {
            tree.ok()?;
        }
        // RegDeleteTreeW may leave the (now empty) key itself.
        let key = RegDeleteKeyW(HKEY_CURRENT_USER, &name);
        if !is_missing(key) {
            key.ok()?;
        }
    }
    Ok(())
}

/// Create (or replace) the `.lnk` shortcut with its AUMID.
pub fn create_shortcut(spec: &ShortcutSpec) -> windows::core::Result<()> {
    if let Some(dir) = spec.path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
                e.raw_os_error().unwrap_or(0) as u32,
            ))
        })?;
    }
    // An existing MTA is fine too.
    let _com = StaGuard::init().ok();
    // SAFETY: plain COM calls on live interfaces; the PROPVARIANT owns a
    // CoTaskMem string that PropVariantClear frees exactly once.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(spec.target.as_path()))?;
        link.SetWorkingDirectory(&HSTRING::from(spec.working_dir.as_path()))?;
        link.SetDescription(&HSTRING::from(spec.description.as_str()))?;
        link.SetIconLocation(&HSTRING::from(spec.target.as_path()), 0)?;

        let store: IPropertyStore = link.cast()?;
        let mut value = PROPVARIANT::default();
        {
            let inner = &mut *value.Anonymous.Anonymous;
            inner.vt = VT_LPWSTR;
            inner.Anonymous.pwszVal = SHStrDupW(&HSTRING::from(spec.aumid.as_str()))?;
        }
        let set = store.SetValue(&PKEY_APP_USER_MODEL_ID, &value);
        let _ = PropVariantClear(&mut value);
        set?;
        store.Commit()?;

        let file: IPersistFile = link.cast()?;
        file.Save(&HSTRING::from(spec.path.as_path()), true)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Runs `invocation` and returns the process exit code ([`Outcome::exit_code`]).
/// `role` is this process's single-instance role.
pub fn run(invocation: Invocation, role: InstanceRole) -> i32 {
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            report(invocation, &format!("Couldn't find the program file: {e}"), true);
            return EXIT_FAILED;
        }
    };
    let layout = match Layout::for_exe(&current_exe) {
        Ok(layout) => layout,
        Err(e) => {
            report(invocation, &e, true);
            return EXIT_FAILED;
        }
    };

    let plan = match invocation.command {
        Command::Install => {
            let exe_size = std::fs::metadata(&current_exe).map(|m| m.len()).unwrap_or(0);
            // An old (v0.1) user's settings count until they are migrated.
            let settings = std::fs::read_to_string(layout.settings_file()).ok().or_else(|| {
                let legacy = layout.legacy.as_ref()?;
                std::fs::read_to_string(legacy.data_dir.join(SETTINGS_FILE.target)).ok()
            });
            let inputs = InstallInputs {
                current_exe: current_exe.clone(),
                exe_size,
                start_at_sign_in: start_at_sign_in_from(settings.as_deref()),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            };
            match install_plan(&layout, &inputs) {
                Ok(plan) => plan,
                Err(e) => {
                    report(invocation, &e, true);
                    return EXIT_FAILED;
                }
            }
        }
        Command::Uninstall => uninstall_plan(&layout, &current_exe, std::process::id()),
        Command::FinishUninstall { parent_pid } => {
            drop(role);
            if let Some(pid) = parent_pid {
                wait_for_process_exit(pid, PARENT_EXIT_TIMEOUT);
            }
            let plan = finish_plan(&layout, &current_exe);
            // Best effort and silent: the original already reported.
            let outcome = execute(&plan, run_finish_step);
            return outcome.exit_code();
        }
    };

    let mut system = match System::new(role) {
        Ok(system) => system,
        Err(e) => {
            report(invocation, &e.message(), true);
            return EXIT_FAILED;
        }
    };
    let outcome = execute(&plan, |step| system.run_step(step));
    drop(system);
    let (text, is_error) = outcome_text(invocation.command, &layout, &outcome);
    report(invocation, &text, is_error);
    outcome.exit_code()
}

/// Finish steps never need the single-instance mutex or COM.
fn run_finish_step(step: &Step) -> Result<(), String> {
    match step {
        Step::RemoveFile(path) => remove_file(path).map_err(|e| e.to_string()),
        Step::RemoveDirIfEmpty(path) => remove_dir_if_empty(path).map_err(|e| e.to_string()),
        Step::ScheduleSelfDelete { exe, dir, cwd } => {
            schedule_self_delete(exe, dir, cwd).map_err(|e| e.to_string())
        }
        other => Err(format!("unexpected step: {other:?}")),
    }
}

/// The final message and whether it reports a problem.
pub fn outcome_text(command: Command, layout: &Layout, outcome: &Outcome) -> (String, bool) {
    let verb = match command {
        Command::Install => "installed",
        _ => "uninstalled",
    };
    if outcome.succeeded() {
        let text = match command {
            Command::Install => format!(
                "{PRODUCT_NAME} was installed in {} and is running.",
                layout.install_dir.display()
            ),
            _ => format!(
                "{PRODUCT_NAME} was uninstalled. Your File Explorer windows were left open."
            ),
        };
        return (text, false);
    }
    let head = if outcome.cancelled {
        format!("{PRODUCT_NAME} was not {verb}.")
    } else {
        format!("{PRODUCT_NAME} was {verb}, but some steps failed:")
    };
    let details: Vec<String> = outcome
        .failures
        .iter()
        .map(|f| format!("\u{2022} {}: {}", f.step.describe(), f.error.trim()))
        .collect();
    let mut text = format!("{head}\n\n{}", details.join("\n"));
    // The old data was left in place; say where, so it can be moved by hand.
    for failure in &outcome.failures {
        if let Step::MigrateData { from, to } = &failure.step {
            text.push_str(&format!(
                "\n\nYour previous session and settings are still in {}. To keep them, exit \
                 {PRODUCT_NAME} from its Settings, then copy {} and {} from that folder into {}, \
                 replacing the files there.",
                from.display(),
                SESSION_FILE.target,
                SETTINGS_FILE.target,
                to.display()
            ));
        }
    }
    (text, true)
}

/// Whether the final message box is shown: always without [`QUIET_FLAG`];
/// with it, only for a failed `--install` (Setup runs `--install --quiet` and
/// relies on this message to name the failed steps).
pub fn shows_report(invocation: Invocation, is_error: bool) -> bool {
    !invocation.quiet || (is_error && invocation.command == Command::Install)
}

fn report(invocation: Invocation, text: &str, is_error: bool) {
    if !shows_report(invocation, is_error) {
        return;
    }
    let icon = if is_error { MB_ICONWARNING } else { MB_ICONINFORMATION };
    // SAFETY: both strings outlive the call.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(crate::ui::prompt::CAPTION),
            MB_OK | icon | MB_SETFOREGROUND,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Registry::{RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};

    fn layout() -> Layout {
        Layout::from_roots(
            Path::new(r"C:\Users\A B\AppData\Local"),
            Path::new(r"C:\Users\A B\AppData\Roaming\Microsoft\Windows\Start Menu\Programs"),
            Path::new(r"C:\Users\A B\AppData\Local\Temp"),
        )
    }

    fn inputs(current_exe: &str) -> InstallInputs {
        InstallInputs {
            current_exe: PathBuf::from(current_exe),
            exe_size: 2_000_000,
            start_at_sign_in: true,
            version: "0.1.0".into(),
        }
    }

    /// `%TEMP%\esr_test_install_<pid>\<name>`, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("esr_test_install_{}", std::process::id()))
                .join(name);
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            if let Some(parent) = self.0.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    // ---- Command line --------------------------------------------------------

    #[test]
    fn parses_commands() {
        let inv = |args: &[&str]| Invocation::from_args(args.iter().copied());
        assert_eq!(inv(&["esr.exe"]), None);
        assert_eq!(inv(&["esr.exe", "--background"]), None);
        assert_eq!(
            inv(&["esr.exe", "--install"]),
            Some(Invocation { command: Command::Install, quiet: false })
        );
        assert_eq!(
            inv(&["esr.exe", "--uninstall", "--quiet"]),
            Some(Invocation { command: Command::Uninstall, quiet: true })
        );
        assert_eq!(
            inv(&["x", "--uninstall-finish", "4242", "--quiet"]),
            Some(Invocation {
                command: Command::FinishUninstall { parent_pid: Some(4242) },
                quiet: true
            })
        );
        assert_eq!(
            inv(&["x", "--uninstall-finish"]),
            Some(Invocation {
                command: Command::FinishUninstall { parent_pid: None },
                quiet: false
            })
        );
        // Exact flags only; the first command wins.
        assert_eq!(inv(&["x", "--installx", "install"]), None);
        assert_eq!(inv(&["x", "--uninstall", "--install"]).unwrap().command, Command::Uninstall);
    }

    // ---- Layout and paths ------------------------------------------------------

    #[test]
    fn layout_matches_design() {
        let l = layout();
        assert_eq!(
            l.installed_exe(),
            PathBuf::from(r"C:\Users\A B\AppData\Local\Programs\ExplorerSessionRecall\esr.exe")
        );
        assert_eq!(l.data_dir, PathBuf::from(r"C:\Users\A B\AppData\Local\ExplorerSessionRecall"));
        assert!(l.shortcut.ends_with(r"Start Menu\Programs\Explorer Session Recall.lnk"));
        assert_eq!(
            l.uninstall_key,
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ExplorerSessionRecall"
        );
        let names: Vec<String> = l
            .data_files()
            .iter()
            .map(|p| {
                assert_eq!(p.parent().unwrap(), l.data_dir);
                p.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        for required in [
            "session.esr",
            "session.esr.unreadable",
            "settings.ini",
            "activity.log",
            "activity.previous.log",
        ] {
            assert!(names.iter().any(|n| n == required), "{required} missing from {names:?}");
        }
        assert_eq!(
            l.handoff_exe(7),
            PathBuf::from(r"C:\Users\A B\AppData\Local\Temp\ESR-uninstall-7\esr.exe")
        );
    }

    #[test]
    fn path_comparison_is_case_insensitive() {
        assert!(same_path(Path::new(r"C:\A\b.exe"), Path::new(r"c:\a\B.EXE")));
        assert!(same_path(Path::new(r"\\?\C:\A\"), Path::new(r"C:/a")));
        assert!(!same_path(Path::new(r"C:\A\b.exe"), Path::new(r"C:\A\c.exe")));
        assert!(is_inside(Path::new(r"C:\Dir\Sub\x.exe"), Path::new(r"c:\dir")));
        assert!(!is_inside(Path::new(r"C:\Dir"), Path::new(r"C:\Dir")));
        assert!(!is_inside(Path::new(r"C:\Directory\x.exe"), Path::new(r"C:\Dir")));
    }

    // ---- Plans -------------------------------------------------------------------

    #[test]
    fn install_plan_from_download_copies_and_registers_everything() {
        // Without the old build's steps (see `install_plan_migrates_the_old_build`).
        let l = Layout { legacy: None, ..layout() };
        let plan = install_plan(&l, &inputs(r"C:\Users\A B\Downloads\esr.exe")).unwrap();
        let exe = l.installed_exe();
        assert_eq!(plan[0], Step::StopRunningInstance);
        assert_eq!(
            plan[1],
            Step::CopyExe { from: PathBuf::from(r"C:\Users\A B\Downloads\esr.exe"), to: exe.clone() }
        );
        let Step::CreateShortcut(shortcut) = &plan[2] else { panic!("{:?}", plan[2]) };
        assert_eq!(shortcut.path, l.shortcut);
        assert_eq!(shortcut.target, exe);
        assert_eq!(shortcut.aumid, "ExplorerSessionRecall");
        assert_eq!(
            plan[3],
            Step::RegisterStartup { exe: exe.clone(), enabled: true, name: "ExplorerSessionRecall".into() }
        );
        let Step::WriteUninstallEntry { key, values } = &plan[4] else { panic!("{:?}", plan[4]) };
        assert_eq!(key, UNINSTALL_SUBKEY);
        assert!(values.contains(&(
            "UninstallString".into(),
            RegValue::Sz(format!("\"{}\" --uninstall", exe.display()))
        )));
        assert_eq!(plan[5], Step::StartInstalled { exe });
        assert_eq!(plan.len(), 6);
    }

    #[test]
    fn legacy_layout_names_the_old_build() {
        let legacy = layout().legacy.expect("the real layout migrates the old build");
        assert_eq!(
            legacy.install_dir,
            PathBuf::from(r"C:\Users\A B\AppData\Local\Programs\ExplorerSessionRestore")
        );
        assert_eq!(legacy.data_dir, PathBuf::from(r"C:\Users\A B\AppData\Local\ExplorerSessionRestore"));
        assert!(legacy.shortcut.ends_with(r"Start Menu\Programs\Explorer Session Restore.lnk"));
        assert_eq!(
            legacy.uninstall_key,
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ExplorerSessionRestore"
        );
        assert_eq!(legacy.startup_name, "ExplorerSessionRestore");
    }

    #[test]
    fn install_plan_migrates_the_old_build() {
        let l = layout();
        let legacy = l.legacy.clone().unwrap();
        let old_exe = legacy.install_dir.join(EXE_NAME);
        let plan = install_plan(&l, &inputs(r"C:\Users\A B\AppData\Local\Temp\is-X\esr.exe")).unwrap();
        assert_eq!(
            plan[..9],
            [
                Step::StopRunningInstance,
                Step::StopLegacyInstance,
                Step::UnregisterStartup { exe: old_exe.clone(), name: "ExplorerSessionRestore".into() },
                Step::RemoveFile(legacy.shortcut.clone()),
                Step::RemoveRegistryKey(legacy.uninstall_key.clone()),
                Step::MigrateData { from: legacy.data_dir.clone(), to: l.data_dir.clone() },
                Step::RemoveFile(old_exe),
                Step::RemoveDirIfEmpty(legacy.install_dir.clone()),
                Step::CopyExe {
                    from: PathBuf::from(r"C:\Users\A B\AppData\Local\Temp\is-X\esr.exe"),
                    to: l.installed_exe()
                },
            ]
        );
        // Data is never moved while the old app might still write it, and a
        // failed migration never cancels the install.
        assert!(Step::StopLegacyInstance.is_fatal());
        assert!(!plan[5].is_fatal());
        assert_eq!(plan.last(), Some(&Step::StartInstalled { exe: l.installed_exe() }));
        // The old program is never deleted while it is the one running.
        let plan = install_plan(&l, &inputs(&legacy.install_dir.join(EXE_NAME).to_string_lossy())).unwrap();
        assert!(!plan.contains(&Step::RemoveFile(legacy.install_dir.join(EXE_NAME))));
        assert!(plan.contains(&Step::MigrateData { from: legacy.data_dir.clone(), to: l.data_dir.clone() }));
    }

    #[test]
    fn execute_cancels_the_install_when_the_old_build_does_not_stop() {
        let l = layout();
        let plan = install_plan(&l, &inputs(r"C:\Users\A B\Downloads\esr.exe")).unwrap();
        let mut ran = Vec::new();
        let outcome = execute(&plan, |step| {
            ran.push(step.clone());
            match step {
                Step::StopLegacyInstance => Err("still running".into()),
                _ => Ok(()),
            }
        });
        assert_eq!(ran, vec![Step::StopRunningInstance, Step::StopLegacyInstance]);
        assert!(outcome.cancelled);

        // A failed migration is reported, and the install goes on.
        let mut ran = 0;
        let outcome = execute(&plan, |step| {
            ran += 1;
            match step {
                Step::MigrateData { .. } => Err("denied".into()),
                _ => Ok(()),
            }
        });
        assert_eq!(ran, plan.len());
        assert!(!outcome.cancelled);
        assert_eq!(outcome.exit_code(), EXIT_COMPLETED_WITH_WARNINGS);
        let (text, is_error) = outcome_text(Command::Install, &l, &outcome);
        assert!(is_error && text.contains("Move your saved session"), "{text}");
        assert!(text.contains("was installed, but some steps failed"), "{text}");
        // The old folder is named so the session can be recovered by hand.
        let legacy_dir = l.legacy.as_ref().unwrap().data_dir.display().to_string();
        assert!(text.contains(&format!("still in {legacy_dir}.")), "{text}");
        assert!(text.contains("session.esr") && text.contains("settings.ini"), "{text}");
    }

    #[test]
    fn exit_codes_separate_failed_from_completed_with_warnings() {
        let failure = |step: Step| Failure { step, error: "x".into() };
        assert_eq!(Outcome::default().exit_code(), 0);
        let warned = Outcome { failures: vec![failure(Step::RemoveFile("a".into()))], cancelled: false };
        assert_eq!(warned.exit_code(), EXIT_COMPLETED_WITH_WARNINGS);
        let cancelled = Outcome { failures: vec![failure(Step::StopRunningInstance)], cancelled: true };
        assert_eq!(cancelled.exit_code(), EXIT_FAILED);
        // Neither collides with success or main's unsupported-build code 2.
        assert!(![0, 2].contains(&EXIT_FAILED) && ![0, 2].contains(&EXIT_COMPLETED_WITH_WARNINGS));
        assert_ne!(EXIT_FAILED, EXIT_COMPLETED_WITH_WARNINGS);
    }

    #[test]
    fn quiet_install_still_reports_failures() {
        let inv = |command, quiet| Invocation { command, quiet };
        // Not quiet: always shown.
        assert!(shows_report(inv(Command::Install, false), false));
        assert!(shows_report(inv(Command::Uninstall, false), true));
        // Quiet install (Setup): only problems are shown.
        assert!(!shows_report(inv(Command::Install, true), false));
        assert!(shows_report(inv(Command::Install, true), true));
        // Quiet uninstall stays silent (QuietUninstallString).
        assert!(!shows_report(inv(Command::Uninstall, true), true));
        assert!(!shows_report(inv(Command::FinishUninstall { parent_pid: None }, true), true));
    }

    #[test]
    fn migrate_data_dir_moves_everything_once() {
        let dir = TestDir::new("migrate");
        let from = dir.0.join("ExplorerSessionRestore");
        let to = dir.0.join("ExplorerSessionRecall");
        // Missing source: nothing to do.
        assert!(!migrate_data_dir(&from, &to).unwrap());
        assert!(!to.exists());

        std::fs::create_dir_all(from.join("deferred").join("1")).unwrap();
        std::fs::write(from.join("session.esr"), b"session").unwrap();
        std::fs::write(from.join("settings.ini"), b"settings").unwrap();
        std::fs::write(from.join("activity.log"), b"log").unwrap();
        std::fs::write(from.join("deferred").join("1").join("x.library-ms"), b"placeholder").unwrap();
        assert!(migrate_data_dir(&from, &to).unwrap());
        assert!(!from.exists());
        assert_eq!(std::fs::read(to.join("session.esr")).unwrap(), b"session");
        assert_eq!(std::fs::read(to.join("settings.ini")).unwrap(), b"settings");
        assert_eq!(std::fs::read(to.join("activity.log")).unwrap(), b"log");
        assert_eq!(std::fs::read(to.join("deferred").join("1").join("x.library-ms")).unwrap(), b"placeholder");

        // The new folder exists: the old one is left alone.
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("session.esr"), b"old").unwrap();
        assert!(!migrate_data_dir(&from, &to).unwrap());
        assert_eq!(std::fs::read(from.join("session.esr")).unwrap(), b"old");
        assert_eq!(std::fs::read(to.join("session.esr")).unwrap(), b"session");
    }

    #[test]
    fn reinstall_from_install_dir_skips_copy_and_keeps_disabled_startup() {
        let l = layout();
        let mut i = inputs(r"c:\users\a b\appdata\local\programs\explorersessionrecall\ESR.EXE");
        i.start_at_sign_in = false;
        let plan = install_plan(&l, &i).unwrap();
        assert!(!plan.iter().any(|s| matches!(s, Step::CopyExe { .. })));
        assert!(plan.contains(&Step::RegisterStartup {
            exe: l.installed_exe(),
            enabled: false,
            name: APP_STARTUP_NAME.into()
        }));
    }

    #[test]
    fn start_at_sign_in_follows_saved_settings() {
        assert!(start_at_sign_in_from(None));
        assert!(start_at_sign_in_from(Some("")));
        let off = Settings { start_at_sign_in: false, ..Settings::default() };
        let on = Settings::default();
        // Whatever the format, the parser must round-trip the option.
        let text_off = esr_core::settings::Settings::serialize(&off);
        let text_on = esr_core::settings::Settings::serialize(&on);
        assert!(!start_at_sign_in_from(Some(&text_off)));
        assert!(start_at_sign_in_from(Some(&text_on)));
    }

    #[test]
    fn uninstall_values_describe_the_installed_program() {
        let l = layout();
        let values = uninstall_values(&l, "1.2.3", 1025).unwrap();
        let get = |name: &str| values.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        let exe = l.installed_exe().display().to_string();
        assert_eq!(get("DisplayName"), Some(RegValue::Sz("Explorer Session Recall".into())));
        assert_eq!(get("DisplayVersion"), Some(RegValue::Sz("1.2.3".into())));
        assert_eq!(get("DisplayIcon"), Some(RegValue::Sz(format!("{exe},0"))));
        assert_eq!(
            get("InstallLocation"),
            Some(RegValue::Sz(l.install_dir.display().to_string()))
        );
        assert_eq!(get("UninstallString"), Some(RegValue::Sz(format!("\"{exe}\" --uninstall"))));
        assert_eq!(
            get("QuietUninstallString"),
            Some(RegValue::Sz(format!("\"{exe}\" --uninstall --quiet")))
        );
        assert_eq!(get("NoModify"), Some(RegValue::Dword(1)));
        assert_eq!(get("NoRepair"), Some(RegValue::Dword(1)));
        assert_eq!(get("EstimatedSize"), Some(RegValue::Dword(2)));
    }

    #[test]
    fn uninstall_plan_removes_everything_from_req_12_6() {
        let l = layout();
        let plan = uninstall_plan(&l, &l.installed_exe(), 99);
        assert_eq!(plan[0], Step::StopRunningInstance);
        assert!(plan.contains(&Step::UnregisterStartup {
            exe: l.installed_exe(),
            name: APP_STARTUP_NAME.into()
        }));
        assert!(plan.contains(&Step::RemoveFile(l.shortcut.clone())));
        assert!(plan.contains(&Step::RemoveRegistryKey(l.uninstall_key.clone())));
        for file in l.data_files() {
            assert!(plan.contains(&Step::RemoveFile(file.clone())), "{file:?}");
        }
        // Placeholders before the data directory itself.
        let placeholders = plan.iter().position(|s| *s == Step::RemovePlaceholders(l.placeholder_dir()));
        let data_dir = plan.iter().position(|s| *s == Step::RemoveDirIfEmpty(l.data_dir.clone()));
        assert!(placeholders.is_some() && placeholders < data_dir, "{plan:?}");
        // Running from the install dir: the program is removed by a temp copy.
        assert_eq!(
            plan.last(),
            Some(&Step::HandOff {
                from: l.installed_exe(),
                to: l.handoff_exe(99),
                cwd: l.temp_dir.clone()
            })
        );
        assert!(!plan.contains(&Step::RemoveFile(l.installed_exe())));
        // Data files are removed only after the instance stopped.
        assert_eq!(plan.iter().filter(|s| **s == Step::StopRunningInstance).count(), 1);
    }

    #[test]
    fn uninstall_from_elsewhere_deletes_program_directly() {
        let l = layout();
        let plan = uninstall_plan(&l, Path::new(r"C:\Users\A B\Downloads\esr.exe"), 99);
        assert!(!plan.iter().any(|s| matches!(s, Step::HandOff { .. })));
        let n = plan.len();
        assert_eq!(plan[n - 2], Step::RemoveFile(l.installed_exe()));
        assert_eq!(plan[n - 1], Step::RemoveDirIfEmpty(l.install_dir.clone()));
    }

    #[test]
    fn finish_plan_self_deletes_only_a_temp_handoff_copy() {
        let l = layout();
        let copy = l.handoff_exe(5);
        let plan = finish_plan(&l, &copy);
        assert_eq!(
            plan,
            vec![
                Step::RemoveFile(l.installed_exe()),
                Step::RemoveDirIfEmpty(l.install_dir.clone()),
                Step::ScheduleSelfDelete {
                    exe: copy.clone(),
                    dir: copy.parent().unwrap().to_owned(),
                    cwd: l.temp_dir.clone(),
                },
            ]
        );
        for elsewhere in [
            l.installed_exe(),
            PathBuf::from(r"C:\Users\A B\Downloads\esr.exe"),
            l.temp_dir.join("esr.exe"),
            l.temp_dir.join("Other").join("esr.exe"),
        ] {
            let plan = finish_plan(&l, &elsewhere);
            assert!(
                !plan.iter().any(|s| matches!(s, Step::ScheduleSelfDelete { .. })),
                "{elsewhere:?}"
            );
        }
    }

    #[test]
    fn execute_stops_only_at_fatal_failures() {
        let l = layout();
        let plan = uninstall_plan(&l, &l.installed_exe(), 1);

        // A non-fatal failure is recorded and the plan continues.
        let mut ran = 0;
        let outcome = execute(&plan, |step| {
            ran += 1;
            match step {
                Step::RemoveRegistryKey(_) => Err("denied".into()),
                _ => Ok(()),
            }
        });
        assert_eq!(ran, plan.len());
        assert!(!outcome.cancelled);
        assert_eq!(outcome.failures.len(), 1);

        // A failed stop cancels everything else (no data is deleted).
        let mut ran = Vec::new();
        let outcome = execute(&plan, |step| {
            ran.push(step.clone());
            match step {
                Step::StopRunningInstance => Err("still running".into()),
                _ => Ok(()),
            }
        });
        assert_eq!(ran, vec![Step::StopRunningInstance]);
        assert!(outcome.cancelled);
        let (text, is_error) = outcome_text(Command::Uninstall, &l, &outcome);
        assert!(is_error);
        assert!(text.contains("was not uninstalled"), "{text}");
        assert!(text.contains("still running"), "{text}");

        let (text, is_error) = outcome_text(Command::Install, &l, &Outcome::default());
        assert!(!is_error);
        assert!(text.contains("installed"), "{text}");
    }

    // ---- Primitives (temp directories and throwaway keys only) -------------------

    #[test]
    fn copy_and_remove_files_in_temp_dir() {
        let dir = TestDir::new("copy");
        let src = dir.0.join("src.exe");
        std::fs::write(&src, b"program").unwrap();
        let dst = dir.0.join("Programs").join("ESR").join("esr.exe");
        copy_exe(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"program");
        // Overwrite on reinstall.
        std::fs::write(&src, b"program v2").unwrap();
        copy_exe(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"program v2");

        let other = dst.parent().unwrap().join("user file.txt");
        std::fs::write(&other, b"x").unwrap();
        remove_file(&dst).unwrap();
        remove_file(&dst).unwrap(); // missing is fine
        assert!(!dst.exists());
        // Not empty: kept.
        remove_dir_if_empty(dst.parent().unwrap()).unwrap();
        assert!(other.exists());
        remove_file(&other).unwrap();
        remove_dir_if_empty(dst.parent().unwrap()).unwrap();
        assert!(!dst.parent().unwrap().exists());
        remove_dir_if_empty(dst.parent().unwrap()).unwrap(); // missing is fine
    }

    #[test]
    fn uninstall_file_steps_clear_a_temp_data_dir() {
        let dir = TestDir::new("data");
        let l = Layout::from_roots(&dir.0.join("Local"), &dir.0.join("Start"), &dir.0.join("Temp"));
        std::fs::create_dir_all(&l.data_dir).unwrap();
        for file in l.data_files() {
            std::fs::write(&file, b"data").unwrap();
        }
        // Regression (task 16.7): placeholders of deferred tabs used to keep
        // the data directory (and saved folder names) after an uninstall.
        for (slot, name) in [("1", "Projekt.library-ms"), ("27", "Other.LIBRARY-MS")] {
            let slot = l.placeholder_dir().join(slot);
            std::fs::create_dir_all(&slot).unwrap();
            std::fs::write(slot.join(name), esr_win::placeholder::PLACEHOLDER_XML).unwrap();
        }
        let plan = uninstall_plan(&l, Path::new(r"C:\elsewhere\esr.exe"), 1);
        let outcome = execute(&plan, |step| match step {
            Step::RemoveFile(p) => remove_file(p).map_err(|e| e.to_string()),
            Step::RemovePlaceholders(p) => remove_placeholders(p).map_err(|e| e.to_string()),
            Step::RemoveDirIfEmpty(p) => remove_dir_if_empty(p).map_err(|e| e.to_string()),
            // System-wide steps are covered by the plan tests only.
            _ => Ok(()),
        });
        assert!(outcome.succeeded(), "{outcome:?}");
        assert!(!l.data_dir.exists());
    }

    /// Only the app's placeholders are deleted; other content is kept.
    #[test]
    fn remove_placeholders_leaves_foreign_files() {
        let dir = TestDir::new("placeholders");
        let p = dir.0.join("deferred");
        remove_placeholders(&p).unwrap(); // missing is fine
        for (sub, name) in [("3", "A.library-ms"), ("3", "notes.txt"), ("x", "B.library-ms")] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
            std::fs::write(p.join(sub).join(name), b"x").unwrap();
        }
        std::fs::write(p.join("C.library-ms"), b"x").unwrap();
        remove_placeholders(&p).unwrap();
        assert!(!p.join("3").join("A.library-ms").exists());
        for kept in [p.join("3").join("notes.txt"), p.join("x").join("B.library-ms"), p.join("C.library-ms")] {
            assert!(kept.exists(), "{kept:?}");
        }
    }

    fn full_overrides(root: &Path) -> DevOverrides {
        DevOverrides {
            data_dir: Some(root.join("Data")),
            startup_name: Some("ESRTest_1_smoke".into()),
            install_dir: Some(root.join("Programs").join("ESR")),
            start_menu_dir: Some(root.join("Start")),
        }
    }

    #[test]
    fn overrides_redirect_every_location() {
        let root = Path::new(r"C:\T");
        assert_eq!(layout().with_overrides(&DevOverrides::default()), Ok(layout()));
        let l = layout().with_overrides(&full_overrides(root)).unwrap();
        assert_eq!(l.data_dir, root.join("Data"));
        assert_eq!(l.install_dir, root.join("Programs").join("ESR"));
        assert_eq!(l.shortcut, root.join("Start").join(SHORTCUT_FILE_NAME));
        assert_eq!(l.uninstall_key, r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ESRTest_1_smoke");
        assert_eq!(l.startup_name, "ESRTest_1_smoke");
        assert_eq!(l.carried_files, vec![dev_overrides::FILE_NAME.to_owned()]);
        assert_eq!(l.temp_dir, layout().temp_dir);
        // A test copy never migrates (or removes) the real old build.
        assert_eq!(l.legacy, None);
        let plan = install_plan(&l, &inputs(r"C:\T\src\esr.exe")).unwrap();
        assert!(!plan.iter().any(|s| matches!(s, Step::StopLegacyInstance | Step::MigrateData { .. })));
        // A partial file (as task 16.6 writes) is refused, never mixed with
        // the real install locations.
        for drop in 0..4 {
            let mut o = full_overrides(root);
            match drop {
                0 => o.data_dir = None,
                1 => o.startup_name = None,
                2 => o.install_dir = None,
                _ => o.start_menu_dir = None,
            }
            assert!(layout().with_overrides(&o).is_err(), "{o:?}");
        }
    }

    #[test]
    fn plans_carry_the_overrides_file_with_the_program() {
        let root = Path::new(r"C:\T");
        let l = layout().with_overrides(&full_overrides(root)).unwrap();
        let file = dev_overrides::FILE_NAME;
        let src = root.join("src").join("esr.exe");

        let plan = install_plan(&l, &inputs(&src.to_string_lossy())).unwrap();
        assert_eq!(plan[1], Step::CopyExe { from: src.clone(), to: l.installed_exe() });
        assert_eq!(plan[2], Step::CopyExe { from: src.with_file_name(file), to: l.install_dir.join(file) });
        assert!(plan.contains(&Step::RegisterStartup {
            exe: l.installed_exe(),
            enabled: true,
            name: "ESRTest_1_smoke".into()
        }));

        // From the install dir: the file is next to the copy before it starts.
        let plan = uninstall_plan(&l, &l.installed_exe(), 9);
        assert!(plan.contains(&Step::UnregisterStartup { exe: l.installed_exe(), name: "ESRTest_1_smoke".into() }));
        let n = plan.len();
        assert_eq!(
            plan[n - 2],
            Step::CopyExe { from: l.install_dir.join(file), to: l.handoff_exe(9).with_file_name(file) }
        );
        assert!(matches!(plan[n - 1], Step::HandOff { .. }));

        let plan = uninstall_plan(&l, &src, 9);
        let n = plan.len();
        assert_eq!(plan[n - 2], Step::RemoveFile(l.install_dir.join(file)));
        assert_eq!(plan[n - 1], Step::RemoveDirIfEmpty(l.install_dir.clone()));

        let copy = l.handoff_exe(9);
        assert_eq!(
            finish_plan(&l, &copy),
            vec![
                Step::RemoveFile(l.installed_exe()),
                Step::RemoveFile(l.install_dir.join(file)),
                Step::RemoveDirIfEmpty(l.install_dir.clone()),
                Step::RemoveFile(copy.with_file_name(file)),
                Step::ScheduleSelfDelete {
                    exe: copy.clone(),
                    dir: copy.parent().unwrap().to_owned(),
                    cwd: l.temp_dir.clone(),
                },
            ]
        );
    }

    fn read_reg_sz(subkey: &str, name: &str) -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: the buffer is valid for `bytes` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(subkey),
                &HSTRING::from(name),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if r.is_err() {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    fn read_reg_dword(subkey: &str, name: &str) -> Option<u32> {
        let mut value = 0u32;
        let mut bytes = 4u32;
        // SAFETY: `value` is valid for 4 bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(subkey),
                &HSTRING::from(name),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut value as *mut u32).cast()),
                Some(&mut bytes),
            )
        };
        r.is_ok().then_some(value)
    }

    /// Uses `HKCU\Software\ESRTest_<pid>_install\…`, never the real
    /// Uninstall key.
    #[test]
    fn registry_entry_round_trip_on_test_key() {
        let root = format!(r"Software\ESRTest_{}_install", std::process::id());
        let subkey = format!(r"{root}\Uninstall\ExplorerSessionRecall");
        assert!(!subkey.contains(r"CurrentVersion\Uninstall"));
        let values = uninstall_values(&layout(), "0.1.0", 4096).unwrap();

        let result = std::panic::catch_unwind(|| {
            write_registry_values(&subkey, &values).unwrap();
            assert_eq!(
                read_reg_sz(&subkey, "DisplayName").as_deref(),
                Some("Explorer Session Recall")
            );
            assert_eq!(read_reg_dword(&subkey, "EstimatedSize"), Some(4));
            assert_eq!(read_reg_dword(&subkey, "NoModify"), Some(1));
            remove_registry_key(&subkey).unwrap();
            assert_eq!(read_reg_sz(&subkey, "DisplayName"), None);
            // Missing is fine.
            remove_registry_key(&subkey).unwrap();
        });
        let _ = remove_registry_key(&root);
        result.unwrap();
    }

    /// Creates a `.lnk` in a temp directory (not the Start menu) and reads the
    /// target, icon location, and AUMID back.
    #[test]
    fn shortcut_carries_target_and_aumid() {
        let dir = TestDir::new("shortcut");
        let target = dir.0.join("Program Dir").join("esr.exe");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"MZ").unwrap();
        let spec = ShortcutSpec {
            path: dir.0.join("Start Menu").join(SHORTCUT_FILE_NAME),
            target: target.clone(),
            working_dir: target.parent().unwrap().to_owned(),
            description: "test".into(),
            aumid: AUMID.into(),
        };
        create_shortcut(&spec).unwrap();
        assert!(spec.path.is_file());
        // Replacing an existing shortcut works too.
        create_shortcut(&spec).unwrap();

        let _com = StaGuard::init().ok();
        // SAFETY: plain COM calls; strings are freed exactly once.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let file: IPersistFile = link.cast().unwrap();
            file.Load(&HSTRING::from(spec.path.as_path()), STGM_READ).unwrap();
            let mut buf = [0u16; 1024];
            link.GetPath(&mut buf, std::ptr::null_mut(), 0).unwrap();
            let len = buf.iter().position(|&c| c == 0).unwrap();
            assert!(same_path(Path::new(&String::from_utf16_lossy(&buf[..len])), &target));

            // The shortcut's icon is esr.exe's first icon (`<exe>,0`).
            let mut icon_buf = [0u16; 1024];
            let mut index = -1i32;
            link.GetIconLocation(&mut icon_buf, &mut index).unwrap();
            let len = icon_buf.iter().position(|&c| c == 0).unwrap();
            assert!(same_path(Path::new(&String::from_utf16_lossy(&icon_buf[..len])), &target));
            assert_eq!(index, 0);

            let store: IPropertyStore = link.cast().unwrap();
            let mut value = store.GetValue(&PKEY_APP_USER_MODEL_ID).unwrap();
            let s = PropVariantToStringAlloc(&value).unwrap();
            let aumid = s.to_string().unwrap();
            CoTaskMemFree(Some(s.0 as *const _));
            let _ = PropVariantClear(&mut value);
            assert_eq!(aumid, "ExplorerSessionRecall");
        }
    }

    #[test]
    fn self_delete_arguments_quote_paths_and_reject_unsafe_ones() {
        let args = self_delete_arguments(
            Path::new(r"C:\Windows\system32"),
            Path::new(r"C:\Users\A B\Temp\ESR-uninstall-1\esr.exe"),
            Path::new(r"C:\Users\A B\Temp\ESR-uninstall-1"),
            2,
        )
        .unwrap();
        assert_eq!(
            args,
            r#"/d /v:off /s /c ""C:\Windows\system32\PING.EXE" -n 3 127.0.0.1 >nul & del /f /q "C:\Users\A B\Temp\ESR-uninstall-1\esr.exe" & rmdir "C:\Users\A B\Temp\ESR-uninstall-1"""#
        );
        let sys = Path::new(r"C:\Windows\system32");
        assert_eq!(self_delete_arguments(sys, Path::new(r"C:\a%b\x.exe"), Path::new(r"C:\a"), 0), None);
        assert_eq!(self_delete_arguments(sys, Path::new(r"C:\a\x.exe"), Path::new("C:\\a\"b"), 0), None);
    }

    /// Runs the real `cmd.exe` command on a dummy file in a temp directory
    /// whose name needs quoting.
    #[test]
    fn self_delete_command_deletes_file_and_folder() {
        let dir = TestDir::new("selfdelete");
        let victim_dir = dir.0.join("ESR-uninstall-1 a & b");
        std::fs::create_dir_all(&victim_dir).unwrap();
        let victim = victim_dir.join("esr copy.exe");
        std::fs::write(&victim, b"x").unwrap();
        let system = system_dir().unwrap();
        let args = self_delete_arguments(&system, &victim, &victim_dir, 0).unwrap();
        let status = spawn_cmd(&system, &args, &dir.0).unwrap().wait().unwrap();
        assert!(status.success() || !victim_dir.exists(), "{status:?}");
        assert!(!victim.exists());
        assert!(!victim_dir.exists());
        assert!(dir.0.exists());
    }

    #[test]
    fn waits_for_process_exit() {
        let system = system_dir().unwrap();
        let mut child = Process::new(system.join("cmd.exe"))
            .args(["/d", "/c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        assert!(wait_for_process_exit(child.id(), Duration::from_secs(10)));
        child.wait().unwrap();
        // This process is still running.
        assert!(!wait_for_process_exit(std::process::id(), Duration::from_millis(50)));
    }
}
