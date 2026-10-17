//! `StartupRegistration`: per-user logon task with HKCU Run-key fallback
//! (Requirements 1.1, 1.9, 12.2; design.md "Start at sign-in").
//!
//! # Mechanisms
//!
//! 1. **Logon task (preferred).** A Task Scheduler task registered through
//!    `ITaskService` at `\ExplorerSessionRecall\<SID>`:
//!    - a logon trigger for the current user only (`ILogonTrigger::UserId` is
//!      the user's SID), with no delay;
//!    - principal `TASK_LOGON_INTERACTIVE_TOKEN`, run level
//!      `TASK_RUNLEVEL_LUA` (no elevation, so no administrator rights or UAC
//!      prompt are needed, Req 1.8);
//!    - the action `"<exe>" --background`;
//!    - settings: `AllowDemandStart`, `DisallowStartIfOnBatteries = false`,
//!      `StopIfGoingOnBatteries = false`, `ExecutionTimeLimit = PT0S` (no
//!      limit), `MultipleInstances = IgnoreNew`, and `Priority = 5`.
//!
//!    Disabling the option re-registers the same definition with
//!    `Enabled = false` (the task is kept, not deleted).
//!
//! 2. **Run key (fallback).** If the task cannot be registered (for example
//!    Task Scheduler is blocked by policy or the service is stopped), the
//!    value `ExplorerSessionRecall = "<exe>" --background` is written under
//!    `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Windows runs Run
//!    entries late and throttled after sign-in, so the 10 s target of Req 1.1
//!    may not be met; [`StartupRegistration::set_enabled`] reports
//!    [`StartupMechanism::RunKey`] so the app can log that.
//!
//! # Priority choice
//!
//! Task Scheduler's default task priority is 7, which starts the process with
//! `BELOW_NORMAL_PRIORITY_CLASS` and also lowers its I/O and memory priority.
//! This app is idle almost all the time (Req 1.6), so a lower priority saves
//! nothing, while at a contended sign-in it would delay the restore that has
//! to finish within 10 s, and processes it starts would inherit the lowered
//! priority. Priority 5 (`NORMAL_PRIORITY_CLASS`, the same as a Run-key or
//! manual start) is used instead.
//!
//! # Folder vs. root (standard users)
//!
//! A standard user can register a task for themselves with an
//! interactive-token principal in any folder they can write to. Whether a
//! standard (or non-elevated) user may create a new folder directly under the
//! root `\` depends on the root folder's security descriptor, which differs
//! between Windows builds and can be tightened by policy (per-user apps such
//! as OneDrive register `<Name>-<SID>` tasks in the root for this reason).
//! Therefore, if creating or using `\ExplorerSessionRecall` fails, the task
//! is registered in the root as `ExplorerSessionRecall <SID>` instead.
//! [`StartupRegistration::task_path`] reports where it ended up. On the
//! development machine (Windows 11, non-elevated token of an administrator,
//! which carries the same rights as a standard user here) creating the folder
//! succeeded; the root fallback is covered by a fault-injected test.
//!
//! # Mapping to `esr_app::ports::Startup` (task 14.6)
//!
//! `esr-win` cannot depend on `esr-app`, so this module has its own
//! [`StartupMechanism`] and returns `windows::core::Error`. The port impl maps
//! `LogonTask`/`RunKey` one to one and converts errors to
//! `ShellError { code: e.code().0, message: e.message() }`. When the result
//! is `RunKey`, it should log [`StartupRegistration::last_task_error`] and
//! that start at sign-in may take longer than 10 s.
//!
//! # Threading
//!
//! Every operation enters COM on the calling thread for its own duration
//! (STA; an existing MTA is accepted) and may block for the Task Scheduler
//! RPC (typically tens of milliseconds).

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    E_INVALIDARG, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, VARIANT_BOOL, VARIANT_FALSE,
    VARIANT_TRUE,
};
#[cfg(test)]
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, IRegisteredTask, ITaskDefinition, ITaskFolder, ITaskService,
    TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE, TASK_ENUM_HIDDEN, TASK_INSTANCES_IGNORE_NEW,
    TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_LUA, TASK_TRIGGER_LOGON, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, Error, HSTRING, Interface, Result};

use crate::shell_util::StaGuard;

/// Production base name: task folder, root-fallback task prefix, and Run value.
pub const APP_STARTUP_NAME: &str = "ExplorerSessionRecall";
/// Argument the logon task passes so a second instance only pings (design.md).
pub const BACKGROUND_ARG: &str = "--background";
/// `HKCU` subkey holding per-user Run entries.
pub const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Task priority (see the module documentation, "Priority choice").
pub const TASK_PRIORITY: i32 = 5;
/// `ExecutionTimeLimit` value meaning "no limit".
const NO_TIME_LIMIT: &str = "PT0S";

/// How start-at-sign-in is registered (mirrors `esr_app::ports::StartupMechanism`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StartupMechanism {
    /// Task Scheduler logon task (meets the 10 s target).
    LogonTask,
    /// `HKCU\...\Run` fallback (startup may be delayed by Windows).
    RunKey,
}

/// Names and paths used by [`StartupRegistration`]. Injectable so tests never
/// touch the real registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupConfig {
    /// Task folder directly under the root, without backslashes.
    pub folder: String,
    /// Task name inside [`Self::folder`].
    pub task_name: String,
    /// Task name used in the root `\` when the folder cannot be used.
    pub root_task_name: String,
    /// Value name under `HKCU\...\Run`.
    pub run_value_name: String,
    /// Executable started at sign-in.
    pub exe: PathBuf,
    /// User the logon trigger fires for (the user's SID string).
    pub user_id: String,
}

impl StartupConfig {
    /// Configuration for `base`: folder `\<base>`, task `<sid>`, root
    /// fallback `<base> <sid>`, Run value `<base>`.
    pub fn with_base_name(base: &str, user_sid: &str, exe: impl Into<PathBuf>) -> Self {
        Self {
            folder: base.to_owned(),
            task_name: user_sid.to_owned(),
            root_task_name: format!("{base} {user_sid}"),
            run_value_name: base.to_owned(),
            exe: exe.into(),
            user_id: user_sid.to_owned(),
        }
    }

    /// The production configuration for the current user and `exe`.
    pub fn for_current_user(exe: impl Into<PathBuf>) -> Result<Self> {
        let sid = crate::file_store::current_user_sid()?;
        Ok(Self::with_base_name(APP_STARTUP_NAME, &sid, exe))
    }

    /// Full path of the preferred task (`\<folder>\<task_name>`).
    pub fn folder_task_path(&self) -> String {
        format!(r"\{}\{}", self.folder, self.task_name)
    }

    /// Full path of the root fallback task (`\<root_task_name>`).
    pub fn root_task_path(&self) -> String {
        format!(r"\{}", self.root_task_name)
    }
}

/// Faults injected by tests to exercise the fallbacks.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct Faults {
    /// Every Task Scheduler call fails with `E_ACCESSDENIED`.
    task_scheduler: bool,
    /// Creating or opening the task folder fails with `E_ACCESSDENIED`.
    folder: bool,
}

/// Registers, enables, disables, and removes start at sign-in.
#[derive(Debug)]
pub struct StartupRegistration {
    config: StartupConfig,
    last_task_error: Option<Error>,
    task_path: Option<String>,
    #[cfg(test)]
    faults: Faults,
}

impl StartupRegistration {
    pub fn new(config: StartupConfig) -> Self {
        Self {
            config,
            last_task_error: None,
            task_path: None,
            #[cfg(test)]
            faults: Faults::default(),
        }
    }

    pub fn config(&self) -> &StartupConfig {
        &self.config
    }

    /// Why the logon task could not be used at the last `set_enabled` that
    /// returned [`StartupMechanism::RunKey`]; `None` after a task success.
    pub fn last_task_error(&self) -> Option<&Error> {
        self.last_task_error.as_ref()
    }

    /// Full path of the task registered by the last successful `set_enabled`.
    pub fn task_path(&self) -> Option<&str> {
        self.task_path.as_deref()
    }

    /// Registers (or updates) start at sign-in and enables or disables it.
    ///
    /// - Task success: the task exists with `Enabled = enabled`; a stale Run
    ///   value is removed so the app is not started twice. Returns `LogonTask`.
    /// - Task failure, enabling: writes the Run value. Returns `RunKey`.
    /// - Task failure, disabling: removes the Run value and returns `RunKey`,
    ///   unless an existing task could be found but not disabled, which is
    ///   returned as an error (the app would otherwise still start, Req 1.9).
    pub fn set_enabled(&mut self, enabled: bool) -> Result<StartupMechanism> {
        let _com = ComScope::enter();
        match self.apply_task(enabled) {
            Ok(path) => {
                self.task_path = Some(path);
                self.last_task_error = None;
                match self.delete_run_value() {
                    // While enabled, a leftover Run value only causes a
                    // second start, which the single-instance check absorbs.
                    Err(e) if !enabled => return Err(e),
                    _ => {}
                }
                Ok(StartupMechanism::LogonTask)
            }
            Err(task_error) => {
                self.task_path = None;
                if enabled {
                    self.last_task_error = Some(task_error);
                    self.write_run_value()?;
                } else {
                    if matches!(self.find_task(), Ok(Some(_))) {
                        return Err(task_error);
                    }
                    self.last_task_error = Some(task_error);
                    self.delete_run_value()?;
                }
                Ok(StartupMechanism::RunKey)
            }
        }
    }

    /// Removes the task (both locations), the task folder if it is then
    /// empty, and the Run value. Missing items are not errors. Every step is
    /// attempted; the first failure is returned.
    pub fn unregister(&mut self) -> Result<()> {
        let _com = ComScope::enter();
        self.task_path = None;
        let task_result = self.unregister_task();
        let run_result = self.delete_run_value();
        task_result.and(run_result)
    }

    /// The Run value's data, or `None` if it does not exist.
    pub fn run_value(&self) -> Result<Option<String>> {
        read_hkcu_string(RUN_SUBKEY, &self.config.run_value_name)
    }

    /// The command line used by the Run value: `"<exe>" --background`.
    pub fn command_line(&self) -> Result<String> {
        Ok(format!("{} {BACKGROUND_ARG}", quote_program_path(&self.config.exe)?))
    }

    // ---- Task Scheduler ----------------------------------------------------

    fn connect(&self) -> Result<ITaskService> {
        #[cfg(test)]
        if self.faults.task_scheduler {
            return Err(E_ACCESSDENIED.into());
        }
        // SAFETY: valid CLSID, no aggregation; Connect takes empty VARIANTs
        // to mean the local machine and the current user.
        unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let empty = VARIANT::default();
            service.Connect(&empty, &empty, &empty, &empty)?;
            Ok(service)
        }
    }

    /// Registers the definition in the folder, or in the root if the folder
    /// cannot be used. Returns the registered task's full path.
    fn apply_task(&self, enabled: bool) -> Result<String> {
        let service = self.connect()?;
        let root = root_folder(&service)?;
        let definition = self.build_definition(&service, enabled)?;

        let in_folder = self
            .ensure_folder(&root)
            .and_then(|folder| register(&folder, &self.config.task_name, &definition, enabled));
        match in_folder {
            Ok(path) => {
                // A root task from an earlier fallback would start the app twice.
                ignore_not_found(delete_task(&root, &self.config.root_task_name))?;
                Ok(path)
            }
            Err(_) => register(&root, &self.config.root_task_name, &definition, enabled),
        }
    }

    fn build_definition(&self, service: &ITaskService, enabled: bool) -> Result<ITaskDefinition> {
        let exe = quote_program_path(&self.config.exe)?;
        // SAFETY: plain COM calls on live interfaces; BSTRs outlive the calls.
        unsafe {
            let definition = service.NewTask(0)?;

            let info = definition.RegistrationInfo()?;
            info.SetDescription(&BSTR::from(
                "Starts Explorer Session Recall when you sign in.",
            ))?;

            let principal = definition.Principal()?;
            principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)?;
            principal.SetRunLevel(TASK_RUNLEVEL_LUA)?;

            let settings = definition.Settings()?;
            settings.SetEnabled(variant_bool(enabled))?;
            settings.SetAllowDemandStart(VARIANT_TRUE)?;
            settings.SetDisallowStartIfOnBatteries(VARIANT_FALSE)?;
            settings.SetStopIfGoingOnBatteries(VARIANT_FALSE)?;
            settings.SetExecutionTimeLimit(&BSTR::from(NO_TIME_LIMIT))?;
            settings.SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)?;
            settings.SetPriority(TASK_PRIORITY)?;

            // No SetDelay: the trigger fires as soon as the user signs in.
            let trigger: ILogonTrigger =
                definition.Triggers()?.Create(TASK_TRIGGER_LOGON)?.cast()?;
            trigger.SetUserId(&BSTR::from(self.config.user_id.as_str()))?;

            let action: IExecAction = definition.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
            action.SetPath(&BSTR::from(exe))?;
            action.SetArguments(&BSTR::from(BACKGROUND_ARG))?;

            Ok(definition)
        }
    }

    /// Opens `\<folder>`, creating it (default security) if it is missing.
    fn ensure_folder(&self, root: &ITaskFolder) -> Result<ITaskFolder> {
        #[cfg(test)]
        if self.faults.folder {
            return Err(E_ACCESSDENIED.into());
        }
        let name = BSTR::from(self.config.folder.as_str());
        // SAFETY: plain COM calls on a live interface.
        unsafe {
            match root.GetFolder(&name) {
                Ok(folder) => Ok(folder),
                Err(e) if is_not_found(&e) => root.CreateFolder(&name, &VARIANT::default()),
                Err(e) => Err(e),
            }
        }
    }

    /// The registered task (folder location first, then root), if any.
    fn find_task(&self) -> Result<Option<IRegisteredTask>> {
        let service = self.connect()?;
        let root = root_folder(&service)?;
        for path in [self.config.folder_task_path(), self.config.root_task_path()] {
            // SAFETY: plain COM call on a live interface.
            match unsafe { root.GetTask(&BSTR::from(path)) } {
                Ok(task) => return Ok(Some(task)),
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    fn unregister_task(&self) -> Result<()> {
        let service = self.connect()?;
        let root = root_folder(&service)?;
        let name = BSTR::from(self.config.folder.as_str());
        // SAFETY: plain COM call on a live interface.
        let folder_result = match unsafe { root.GetFolder(&name) } {
            Ok(folder) => ignore_not_found(delete_task(&folder, &self.config.task_name))
                .and_then(|()| delete_folder_if_empty(&root, &folder, &name)),
            Err(e) => ignore_not_found(Err(e)),
        };
        let root_result = ignore_not_found(delete_task(&root, &self.config.root_task_name));
        folder_result.and(root_result)
    }

    // ---- Run key -----------------------------------------------------------

    fn write_run_value(&self) -> Result<()> {
        let data = self.command_line()?;
        let wide: Vec<u16> = data.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer of the stated size.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_SUBKEY),
                &HSTRING::from(self.config.run_value_name.as_str()),
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            )
            .ok()
        }
    }

    fn delete_run_value(&self) -> Result<()> {
        // SAFETY: plain registry call with NUL-terminated strings.
        let result = unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_SUBKEY),
                &HSTRING::from(self.config.run_value_name.as_str()),
            )
            .ok()
        };
        ignore_not_found(result)
    }
}

/// Wraps an executable path in double quotes for a command line, as the
/// Run value and the task action expect (`"C:\Program Files\x.exe"`).
///
/// The program name is parsed literally up to the closing quote (no
/// backslash escaping applies to it), so quoting is enough. Paths that are
/// empty or contain `"` (not a valid file name character) are rejected.
pub fn quote_program_path(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .filter(|s| !s.is_empty() && !s.contains('"'))
        .ok_or_else(|| Error::from(E_INVALIDARG))?;
    Ok(format!("\"{text}\""))
}

/// Enters COM (STA) for the duration of one operation. If the thread is
/// already in the MTA, that apartment is used as is.
struct ComScope {
    _guard: Option<StaGuard>,
}

impl ComScope {
    fn enter() -> Self {
        Self {
            _guard: StaGuard::init().ok(),
        }
    }
}

fn root_folder(service: &ITaskService) -> Result<ITaskFolder> {
    // SAFETY: plain COM call on a live interface.
    unsafe { service.GetFolder(&BSTR::from(r"\")) }
}

/// Registers `definition` as `name` in `folder` with an interactive-token
/// principal for the current user, and makes sure `Enabled` matches.
fn register(
    folder: &ITaskFolder,
    name: &str,
    definition: &ITaskDefinition,
    enabled: bool,
) -> Result<String> {
    let empty = VARIANT::default();
    // SAFETY: plain COM calls on live interfaces; empty VARIANTs mean "the
    // current user, no password, default security descriptor".
    unsafe {
        let task = folder.RegisterTaskDefinition(
            &BSTR::from(name),
            definition,
            TASK_CREATE_OR_UPDATE.0,
            &empty,
            &empty,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &empty,
        )?;
        if task.Enabled()?.as_bool() != enabled {
            task.SetEnabled(variant_bool(enabled))?;
        }
        Ok(task.Path()?.to_string())
    }
}

fn delete_task(folder: &ITaskFolder, name: &str) -> Result<()> {
    // SAFETY: plain COM call on a live interface.
    unsafe { folder.DeleteTask(&BSTR::from(name), 0) }
}

fn delete_folder_if_empty(root: &ITaskFolder, folder: &ITaskFolder, name: &BSTR) -> Result<()> {
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let tasks = folder.GetTasks(TASK_ENUM_HIDDEN.0)?.Count()?;
        let subfolders = folder.GetFolders(0)?.Count()?;
        if tasks == 0 && subfolders == 0 {
            ignore_not_found(root.DeleteFolder(name, 0))?;
        }
    }
    Ok(())
}

fn read_hkcu_string(subkey: &str, value: &str) -> Result<Option<String>> {
    let subkey = HSTRING::from(subkey);
    let value = HSTRING::from(value);
    let mut size = 0u32;
    // SAFETY: size query with no data buffer.
    let probe = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, &subkey, &value, RRF_RT_REG_SZ, None, None, Some(&mut size))
    };
    if probe == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    probe.ok()?;
    let mut buf = vec![0u16; (size as usize).div_ceil(2).max(1)];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` is valid for `bytes` bytes; RegGetValueW NUL-terminates.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(Some(String::from_utf16_lossy(&buf[..len])))
}

fn variant_bool(value: bool) -> VARIANT_BOOL {
    if value { VARIANT_TRUE } else { VARIANT_FALSE }
}

fn is_not_found(error: &Error) -> bool {
    let code = error.code();
    code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_PATH_NOT_FOUND.to_hresult()
}

/// Treats "file/path not found" as success (the item is already gone).
fn ignore_not_found(result: Result<()>) -> Result<()> {
    match result {
        Err(e) if is_not_found(&e) => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    //! These tests register real, uniquely named test tasks
    //! (`\ESRTest_<pid>_<tag>\<SID>`) and Run values (`ESRTest_<pid>_<tag>`)
    //! for the current user. The executable path does not exist, nothing is
    //! ever run on demand, and the logon trigger would only fire at the next
    //! sign-in; the [`Cleanup`] guard unregisters everything in `Drop`, also
    //! when a test panics.

    use super::*;
    use windows::Win32::System::TaskScheduler::{
        IAction, TASK_INSTANCES_POLICY, TASK_LOGON_TYPE, TASK_RUNLEVEL_TYPE, TASK_TRIGGER_TYPE2,
    };

    /// Owns a test registration and removes it on drop.
    struct Cleanup(StartupRegistration);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.faults = Faults::default();
            let _ = self.0.unregister();
        }
    }

    impl std::ops::Deref for Cleanup {
        type Target = StartupRegistration;
        fn deref(&self) -> &StartupRegistration {
            &self.0
        }
    }

    impl std::ops::DerefMut for Cleanup {
        fn deref_mut(&mut self) -> &mut StartupRegistration {
            &mut self.0
        }
    }

    fn test_registration(tag: &str) -> Cleanup {
        let base = format!("ESRTest_{}_{tag}", std::process::id());
        assert!(base.starts_with("ESRTest_") && base != APP_STARTUP_NAME);
        let sid = crate::file_store::current_user_sid().expect("current user SID");
        // A path with spaces that does not exist: even if a leftover entry
        // fired at sign-in, nothing would run.
        let exe = std::env::temp_dir()
            .join(format!("ESRTest does not exist {}", std::process::id()))
            .join("esr test.exe");
        assert!(!exe.exists());
        Cleanup(StartupRegistration::new(StartupConfig::with_base_name(&base, &sid, exe)))
    }

    /// The registered task's observable properties.
    #[derive(Debug)]
    struct TaskInfo {
        path: String,
        enabled: bool,
        trigger_type: TASK_TRIGGER_TYPE2,
        trigger_user: String,
        trigger_delay: String,
        trigger_count: i32,
        exe: String,
        args: String,
        logon_type: TASK_LOGON_TYPE,
        run_level: TASK_RUNLEVEL_TYPE,
        priority: i32,
        time_limit: String,
        instances: TASK_INSTANCES_POLICY,
        disallow_on_batteries: bool,
        stop_on_batteries: bool,
        allow_demand_start: bool,
    }

    fn inspect(reg: &StartupRegistration) -> Option<TaskInfo> {
        let _com = ComScope::enter();
        let task = reg.find_task().expect("query task")?;
        // SAFETY: plain COM getters on live interfaces with valid out pointers.
        unsafe {
            let def = task.Definition().unwrap();
            let triggers = def.Triggers().unwrap();
            let mut trigger_count = 0;
            triggers.Count(&mut trigger_count).unwrap();
            let trigger = triggers.get_Item(1).unwrap();
            let mut trigger_type = TASK_TRIGGER_TYPE2::default();
            trigger.Type(&mut trigger_type).unwrap();
            let logon: ILogonTrigger = trigger.cast().unwrap();
            let (mut trigger_user, mut trigger_delay) = (BSTR::new(), BSTR::new());
            logon.UserId(&mut trigger_user).unwrap();
            logon.Delay(&mut trigger_delay).unwrap();

            let action: IAction = def.Actions().unwrap().get_Item(1).unwrap();
            let exec: IExecAction = action.cast().unwrap();
            let (mut exe, mut args) = (BSTR::new(), BSTR::new());
            exec.Path(&mut exe).unwrap();
            exec.Arguments(&mut args).unwrap();

            let principal = def.Principal().unwrap();
            let mut logon_type = TASK_LOGON_TYPE::default();
            principal.LogonType(&mut logon_type).unwrap();
            let mut run_level = TASK_RUNLEVEL_TYPE::default();
            principal.RunLevel(&mut run_level).unwrap();

            let settings = def.Settings().unwrap();
            let mut priority = 0;
            settings.Priority(&mut priority).unwrap();
            let mut time_limit = BSTR::new();
            settings.ExecutionTimeLimit(&mut time_limit).unwrap();
            let mut instances = TASK_INSTANCES_POLICY::default();
            settings.MultipleInstances(&mut instances).unwrap();
            let mut disallow = VARIANT_BOOL::default();
            settings.DisallowStartIfOnBatteries(&mut disallow).unwrap();
            let mut stop = VARIANT_BOOL::default();
            settings.StopIfGoingOnBatteries(&mut stop).unwrap();
            let mut demand = VARIANT_BOOL::default();
            settings.AllowDemandStart(&mut demand).unwrap();

            Some(TaskInfo {
                path: task.Path().unwrap().to_string(),
                enabled: task.Enabled().unwrap().as_bool(),
                trigger_type,
                trigger_user: trigger_user.to_string(),
                trigger_delay: trigger_delay.to_string(),
                trigger_count,
                exe: exe.to_string(),
                args: args.to_string(),
                logon_type,
                run_level,
                priority,
                time_limit: time_limit.to_string(),
                instances,
                disallow_on_batteries: disallow.as_bool(),
                stop_on_batteries: stop.as_bool(),
                allow_demand_start: demand.as_bool(),
            })
        }
    }

    /// Whether the logon trigger's user is the current user (Task Scheduler
    /// may report the SID or the resolved `DOMAIN\user` name).
    fn is_current_user(trigger_user: &str, sid: &str) -> bool {
        if trigger_user.eq_ignore_ascii_case(sid) {
            return true;
        }
        let user = std::env::var("USERNAME").unwrap_or_default();
        let name = trigger_user.rsplit('\\').next().unwrap_or(trigger_user);
        !user.is_empty() && name.eq_ignore_ascii_case(&user)
    }

    fn assert_task_definition(reg: &StartupRegistration, info: &TaskInfo) {
        let config = reg.config();
        assert_eq!(info.trigger_count, 1);
        assert_eq!(info.trigger_type, TASK_TRIGGER_LOGON);
        assert!(
            is_current_user(&info.trigger_user, &config.user_id),
            "trigger user {:?}",
            info.trigger_user
        );
        assert!(info.trigger_delay.is_empty(), "delay {:?}", info.trigger_delay);
        assert_eq!(info.exe, quote_program_path(&config.exe).unwrap());
        assert_eq!(info.args, BACKGROUND_ARG);
        assert_eq!(info.logon_type, TASK_LOGON_INTERACTIVE_TOKEN);
        assert_eq!(info.run_level, TASK_RUNLEVEL_LUA);
        assert_eq!(info.priority, TASK_PRIORITY);
        assert_eq!(info.time_limit, NO_TIME_LIMIT);
        assert_eq!(info.instances, TASK_INSTANCES_IGNORE_NEW);
        assert!(!info.disallow_on_batteries);
        assert!(!info.stop_on_batteries);
        assert!(info.allow_demand_start);
    }

    fn folder_exists(reg: &StartupRegistration) -> bool {
        let _com = ComScope::enter();
        let root = root_folder(&reg.connect().unwrap()).unwrap();
        // SAFETY: plain COM call on a live interface.
        match unsafe { root.GetFolder(&BSTR::from(reg.config().folder.as_str())) } {
            Ok(_) => true,
            Err(e) if is_not_found(&e) => false,
            Err(e) => panic!("GetFolder: {e:?}"),
        }
    }

    #[test]
    fn quote_program_path_wraps_paths_with_spaces() {
        let path = Path::new(r"C:\Users\A B\AppData\Local\Programs\ExplorerSessionRecall\esr.exe");
        assert_eq!(
            quote_program_path(path).unwrap(),
            r#""C:\Users\A B\AppData\Local\Programs\ExplorerSessionRecall\esr.exe""#
        );
        assert_eq!(quote_program_path(Path::new(r"C:\esr.exe")).unwrap(), r#""C:\esr.exe""#);
    }

    #[test]
    fn quote_program_path_rejects_empty_and_quotes() {
        assert_eq!(quote_program_path(Path::new("")).unwrap_err().code(), E_INVALIDARG);
        assert_eq!(
            quote_program_path(Path::new(r#"C:\a"b.exe"#)).unwrap_err().code(),
            E_INVALIDARG
        );
    }

    #[test]
    fn command_line_appends_background_flag() {
        let reg = StartupRegistration::new(StartupConfig::with_base_name(
            "ESRTest_cmdline",
            "S-1-5-21-1",
            r"C:\Program Files\ESR\esr.exe",
        ));
        assert_eq!(reg.command_line().unwrap(), r#""C:\Program Files\ESR\esr.exe" --background"#);
    }

    #[test]
    fn production_config_names() {
        let config = StartupConfig::with_base_name(APP_STARTUP_NAME, "S-1-5-21-7", r"C:\esr.exe");
        assert_eq!(config.folder_task_path(), r"\ExplorerSessionRecall\S-1-5-21-7");
        assert_eq!(config.root_task_path(), r"\ExplorerSessionRecall S-1-5-21-7");
        assert_eq!(config.run_value_name, "ExplorerSessionRecall");
        assert_eq!(config.user_id, "S-1-5-21-7");
    }

    #[test]
    fn logon_task_register_disable_enable_unregister() {
        let mut reg = test_registration("task");

        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::LogonTask);
        assert_eq!(reg.task_path(), Some(reg.config().folder_task_path().as_str()));
        let info = inspect(&reg).expect("task registered");
        println!("registered {} (trigger user {})", info.path, info.trigger_user);
        assert_eq!(info.path, reg.config().folder_task_path());
        assert!(info.enabled);
        assert_task_definition(&reg, &info);
        assert_eq!(reg.run_value().unwrap(), None);

        assert_eq!(reg.set_enabled(false).unwrap(), StartupMechanism::LogonTask);
        let info = inspect(&reg).expect("task kept when disabled");
        assert!(!info.enabled);
        assert_task_definition(&reg, &info);

        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::LogonTask);
        assert!(inspect(&reg).expect("task re-enabled").enabled);

        reg.unregister().unwrap();
        assert!(inspect(&reg).is_none());
        assert!(!folder_exists(&reg));
        assert_eq!(reg.run_value().unwrap(), None);
        // Unregistering again is a no-op.
        reg.unregister().unwrap();
    }

    #[test]
    fn root_fallback_when_folder_is_unavailable() {
        let mut reg = test_registration("root");
        reg.0.faults.folder = true;

        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::LogonTask);
        let info = inspect(&reg).expect("root task registered");
        assert_eq!(info.path, reg.config().root_task_path());
        assert_eq!(reg.task_path(), Some(info.path.as_str()));
        assert!(info.enabled);
        assert_task_definition(&reg, &info);

        assert_eq!(reg.set_enabled(false).unwrap(), StartupMechanism::LogonTask);
        assert!(!inspect(&reg).expect("root task kept").enabled);

        // Once the folder works again, the root task is replaced, not duplicated.
        reg.0.faults.folder = false;
        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::LogonTask);
        assert_eq!(inspect(&reg).unwrap().path, reg.config().folder_task_path());
        {
            let _com = ComScope::enter();
            let root = root_folder(&reg.connect().unwrap()).unwrap();
            // SAFETY: plain COM call on a live interface.
            let stale = unsafe { root.GetTask(&BSTR::from(reg.config().root_task_path())) };
            assert!(stale.is_err_and(|e| is_not_found(&e)));
        }

        reg.unregister().unwrap();
        assert!(inspect(&reg).is_none());
    }

    #[test]
    fn run_key_fallback_round_trip() {
        let mut reg = test_registration("runkey");
        reg.0.faults.task_scheduler = true;

        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::RunKey);
        assert_eq!(reg.last_task_error().map(Error::code), Some(E_ACCESSDENIED));
        assert_eq!(reg.task_path(), None);
        let expected = reg.command_line().unwrap();
        assert!(expected.ends_with(r#"esr test.exe" --background"#));
        assert_eq!(reg.run_value().unwrap(), Some(expected));

        assert_eq!(reg.set_enabled(false).unwrap(), StartupMechanism::RunKey);
        assert_eq!(reg.run_value().unwrap(), None);

        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::RunKey);
        assert!(reg.run_value().unwrap().is_some());

        // When Task Scheduler works again, the task replaces the Run value.
        reg.0.faults.task_scheduler = false;
        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::LogonTask);
        assert!(reg.last_task_error().is_none());
        assert_eq!(reg.run_value().unwrap(), None);

        // Unregister removes both.
        reg.0.faults.task_scheduler = true;
        assert_eq!(reg.set_enabled(true).unwrap(), StartupMechanism::RunKey);
        reg.0.faults.task_scheduler = false;
        reg.unregister().unwrap();
        assert_eq!(reg.run_value().unwrap(), None);
        assert!(inspect(&reg).is_none());
    }
}
