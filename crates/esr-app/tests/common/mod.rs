//! Helpers shared by the `esr-app` integration tests that run `esr.exe` as a
//! separate process (`startup_coexistence.rs`, task 16.6; `smoke.rs`, task
//! 16.7).
//!
//! A test that uses this module must also declare the Explorer guard at its
//! crate root, as `mod explorer_guard;` (it is `esr-win`'s
//! `tests/common/explorer_guard.rs`, included with `#[path]`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use esr_app::dev_overrides;
use esr_app::install;
use esr_app::instance::{self, ENGINE_WINDOW_CLASS, InstanceMessages, InstanceRole};
use esr_core::format::parse;
use esr_core::model::{SessionDoc, Snapshot};
use esr_core::store::SESSION_FILE;
use esr_win::shell_util::{StaGuard, current_folder_pidl, enumerate_all_explorer_windows, is_cabinet_window};
use esr_win::startup::BACKGROUND_ARG;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HANDLE, HLOCAL, HWND,
    LPARAM, LocalFree, WPARAM,
};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID,
    SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenElevation,
    TokenElevationType, TokenGroups, TokenIntegrityLevel, TokenUser,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, ITaskService, TASK_LOGON_TYPE, TASK_RUNLEVEL_TYPE,
    TASK_TRIGGER_LOGON, TASK_TRIGGER_TYPE2, TaskScheduler,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, IsWindow, PostMessageW, SW_SHOWNOACTIVATE,
};
use windows::core::{BSTR, HSTRING, Interface, PCWSTR, PWSTR, w};

use crate::explorer_guard::{ExplorerGuard, parsing_name, pump_once};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

pub fn normalize(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    p.trim_end_matches('\\').to_lowercase()
}

/// Pumps messages until `f` yields or `timeout` passes.
pub fn wait<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + timeout;
    loop {
        pump_once();
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The overrides file is read by debug builds only; a release build would
/// use the real data directory.
pub fn require_debug_build() {
    if !cfg!(debug_assertions) {
        panic!(
            "these tests need a debug build of esr.exe (release builds ignore {})",
            dev_overrides::FILE_NAME
        );
    }
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0;
    // SAFETY: valid out pointer; a stale HWND yields 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Whether `hwnd` is still an Explorer window.
pub fn window_alive(hwnd: HWND) -> bool {
    // SAFETY: IsWindow tolerates stale HWNDs.
    unsafe { IsWindow(Some(hwnd)) }.as_bool() && is_cabinet_window(hwnd)
}

pub fn read_doc(data_dir: &Path) -> Option<SessionDoc> {
    parse(&std::fs::read(data_dir.join(SESSION_FILE.target)).ok()?).ok()
}

pub fn snapshot_folders(snapshot: &Snapshot) -> Vec<Vec<String>> {
    snapshot
        .windows
        .iter()
        .map(|w| {
            w.tabs
                .iter()
                .map(|t| normalize(&t.location.parsing_name.to_string_lossy()))
                .collect()
        })
        .collect()
}

pub fn lists(doc: &SessionDoc, folder: &Path) -> bool {
    let target = normalize(&folder.to_string_lossy());
    snapshot_folders(&doc.snapshot).iter().flatten().any(|f| *f == target)
}

// ---------------------------------------------------------------------------
// Explorer windows
// ---------------------------------------------------------------------------

/// Opens a new Explorer window on `dir` (from a launcher STA thread), like
/// any program asking the shell for a folder window.
pub fn request_open(dir: PathBuf) -> Result<(), String> {
    std::thread::spawn(move || {
        let _sta = StaGuard::init().map_err(|e| e.to_string())?;
        // SAFETY: valid path string; the PIDL is freed after the call.
        unsafe {
            let pidl = ILCreateFromPathW(&HSTRING::from(dir.as_os_str()));
            if pidl.is_null() {
                return Err("ILCreateFromPathW failed".to_string());
            }
            let mut sei = SHELLEXECUTEINFOW {
                cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
                fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                lpVerb: w!("opennewwindow"),
                lpIDList: pidl.cast(),
                nShow: SW_SHOWNOACTIVATE.0,
                ..Default::default()
            };
            let r = ShellExecuteExW(&mut sei);
            ILFree(Some(pidl));
            r.map_err(|e| format!("ShellExecuteExW: {e}"))
        }
    })
    .join()
    .map_err(|_| "launcher panicked".to_string())?
}

/// Parsing names of the tabs of the Explorer window `hwnd`.
pub fn window_folders(hwnd: HWND) -> Option<Vec<String>> {
    enumerate_all_explorer_windows()
        .ok()?
        .into_iter()
        .find(|g| g.top_hwnd == hwnd)
        .map(|g| {
            g.tabs
                .iter()
                .map(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .map(|n| normalize(&n))
                        .unwrap_or_default()
                })
                .collect()
        })
}

/// The new window showing `dir`, adopted by the guard as soon as it appears.
pub fn wait_for_window(guard: &ExplorerGuard, dir: &Path, timeout: Duration) -> Option<HWND> {
    let target = normalize(&dir.to_string_lossy());
    wait(timeout, || {
        enumerate_all_explorer_windows()
            .unwrap_or_default()
            .into_iter()
            .filter(|g| !guard.is_pre_existing(g.top_hwnd))
            .find(|g| {
                g.tabs.iter().any(|t| {
                    current_folder_pidl(&t.browser)
                        .ok()
                        .and_then(|p| parsing_name(&p))
                        .is_some_and(|n| normalize(&n) == target)
                })
            })
            .map(|g| {
                guard.adopt(g.top_hwnd);
                g.top_hwnd
            })
    })
}

pub fn open_window(guard: &ExplorerGuard, dir: &Path) -> Result<HWND, String> {
    request_open(dir.to_path_buf())?;
    wait_for_window(guard, dir, Duration::from_secs(20))
        .ok_or_else(|| format!("the test window on {} did not open", dir.display()))
}

// ---------------------------------------------------------------------------
// Processes and instances
// ---------------------------------------------------------------------------

pub fn process_image(pid: u32) -> Option<PathBuf> {
    // SAFETY: the handle is closed below; the buffer is valid for `len`.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// Every `ESR.Engine` window in the session, with its process id.
pub fn engine_windows() -> Vec<(HWND, u32)> {
    let class = HSTRING::from(ENGINE_WINDOW_CLASS);
    let mut out = Vec::new();
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: plain window enumeration by class.
        let Ok(hwnd) = (unsafe { FindWindowExW(None, after, &class, PCWSTR::null()) }) else {
            break;
        };
        if hwnd.is_invalid() {
            break;
        }
        out.push((hwnd, window_pid(hwnd)));
        after = Some(hwnd);
    }
    out
}

/// Fails if an Explorer Session Restore instance runs or holds the
/// single-instance mutex (test copies take the real mutex).
pub fn require_no_running_instance() -> Result<(), String> {
    let engines = engine_windows();
    if !engines.is_empty() {
        let list: Vec<String> = engines
            .iter()
            .map(|(_, pid)| format!("pid {pid} ({:?})", process_image(*pid)))
            .collect();
        return Err(format!("an Explorer Session Restore instance is running: {}", list.join(", ")));
    }
    let sid = instance::current_user_sid().map_err(|e| e.message())?;
    match instance::acquire_named(&instance::mutex_name(&sid), ["esr.exe", BACKGROUND_ARG])
        .map_err(|e| e.message())?
    {
        // Released at once.
        InstanceRole::Primary(_guard) => Ok(()),
        InstanceRole::Secondary { .. } => {
            Err("the Explorer Session Restore mutex is held by another process".into())
        }
    }
}

/// The running instance of the program `exe`: its Engine window and pid.
pub fn instance_of(exe: &Path) -> Option<(HWND, u32)> {
    engine_windows()
        .into_iter()
        .find(|(_, pid)| process_image(*pid).is_some_and(|p| install::same_path(&p, exe)))
}

/// Posts `ESR_EXIT` to an Engine window.
pub fn post_exit(engine: HWND) -> Result<(), String> {
    let exit = InstanceMessages::register().exit;
    // SAFETY: posting a registered message to a test copy's Engine window.
    unsafe { PostMessageW(Some(engine), exit, WPARAM(0), LPARAM(0)) }.map_err(|e| format!("ESR_EXIT: {e}"))
}

/// Terminates process `pid` if its image is `exe` (never anything else).
pub fn terminate_if_image(pid: u32, exe: &Path) {
    if process_image(pid).is_some_and(|p| install::same_path(&p, exe)) {
        // SAFETY: the handle is closed below; the process runs the test copy.
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
        install::wait_for_process_exit(pid, Duration::from_secs(5));
    }
}

/// Stops every running instance of the program `exe`: `ESR_EXIT`, then
/// `TerminateProcess` if it is still there after 10 s. Returns problems.
pub fn stop_instances_of(exe: &Path, tag: &str) -> Vec<String> {
    let mut problems = Vec::new();
    while let Some((engine, pid)) = instance_of(exe) {
        let _ = post_exit(engine);
        if install::wait_for_process_exit(pid, Duration::from_secs(10)) {
            println!("[{tag}] instance {pid} stopped");
            continue;
        }
        problems.push(format!("instance {pid} did not exit after ESR_EXIT"));
        terminate_if_image(pid, exe);
        if instance_of(exe).is_some_and(|(_, p)| p == pid) {
            problems.push(format!("instance {pid} could not be stopped"));
            break;
        }
    }
    problems
}

/// Makes a console-subsystem PE image a GUI-subsystem one (what release
/// builds are), so starting it opens no console window.
pub fn make_gui_subsystem(path: &Path) -> Result<(), String> {
    const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;
    const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;
    let mut b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let u16_at = |b: &[u8], o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let pe = b
        .get(0x3C..0x40)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
        .ok_or("not a PE image")?;
    if b.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        return Err("no PE signature".into());
    }
    // Optional header after the 4-byte signature and the 20-byte file
    // header; `Subsystem` is at offset 68 in both PE32 and PE32+.
    let subsystem = pe + 24 + 68;
    match u16_at(&b, subsystem) {
        Some(IMAGE_SUBSYSTEM_WINDOWS_CUI) => {
            b[subsystem..subsystem + 2].copy_from_slice(&IMAGE_SUBSYSTEM_WINDOWS_GUI.to_le_bytes());
            std::fs::write(path, &b).map_err(|e| format!("write {}: {e}", path.display()))
        }
        Some(IMAGE_SUBSYSTEM_WINDOWS_GUI) => Ok(()),
        other => Err(format!("unexpected PE subsystem {other:?}")),
    }
}

/// Runs `exe args` in its own directory and waits up to `timeout` for it to
/// end. Returns the exit code, the time it took, and its pid. A process
/// still running at the deadline is terminated and reported as an error.
pub fn run_to_end(exe: &Path, args: &[&str], timeout: Duration) -> Result<(i32, Duration, u32), String> {
    let t0 = Instant::now();
    let mut child = Process::new(exe)
        .args(args)
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start {} {args:?}: {e}", exe.display()))?;
    let pid = child.id();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok((status.code().unwrap_or(-1), t0.elapsed(), pid));
        }
        if t0.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} {args:?} did not end within {timeout:?}", exe.display()));
        }
        pump_once();
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Tokens (elevation)
// ---------------------------------------------------------------------------

/// `SECURITY_MANDATORY_MEDIUM_RID`.
pub const MEDIUM_INTEGRITY: u32 = 0x2000;
/// `SE_GROUP_ENABLED` / `SE_GROUP_USE_FOR_DENY_ONLY`.
const SE_GROUP_ENABLED: u32 = 0x4;
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x10;
/// The local Administrators group.
pub const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
/// The local system account.
pub const SYSTEM_SID: &str = "S-1-5-18";

/// How the Administrators group appears in a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminGroup {
    /// Not in the token (a standard account).
    Absent,
    /// Present for deny only (a filtered UAC token of an administrator).
    DenyOnly,
    /// Present and enabled (an elevated or a UAC-off administrator token).
    Enabled,
    /// Present, neither enabled nor deny-only.
    Other(u32),
}

/// What the tests check of a process token.
#[derive(Clone, Debug)]
pub struct TokenInfo {
    /// `TOKEN_ELEVATION::TokenIsElevated`.
    pub elevated: bool,
    /// `TokenElevationType`: 1 default, 2 full, 3 limited.
    pub elevation_type: i32,
    /// Mandatory integrity level RID (`0x2000` medium, `0x3000` high).
    pub integrity: u32,
    pub admins: AdminGroup,
    /// The token's user SID.
    pub user: String,
}

impl TokenInfo {
    /// No administrator rights: not elevated, at most medium integrity, and
    /// the Administrators group absent or deny-only.
    pub fn is_standard_user(&self) -> bool {
        !self.elevated
            && self.elevation_type != 2
            && self.integrity <= MEDIUM_INTEGRITY
            && matches!(self.admins, AdminGroup::Absent | AdminGroup::DenyOnly)
    }
}

fn sid_string(sid: PSID) -> Result<String, String> {
    let mut s = PWSTR::null();
    // SAFETY: `sid` points into a live buffer; the string is freed once.
    unsafe {
        ConvertSidToStringSidW(sid, &mut s).map_err(|e| format!("ConvertSidToStringSidW: {e}"))?;
        let out = s.to_string().map_err(|e| e.to_string());
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        out
    }
}

/// `GetTokenInformation` into an 8-byte-aligned buffer. `fixed`: the size of
/// a fixed-size class, which needs exactly that length.
///
/// # Safety
/// `token` must be a valid token handle opened with `TOKEN_QUERY`.
unsafe fn token_information(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
    fixed: Option<usize>,
) -> Result<Vec<u64>, String> {
    let mut len = 0u32;
    if let Some(size) = fixed {
        len = size as u32;
    } else {
        // SAFETY: size query; expected to fail with ERROR_INSUFFICIENT_BUFFER.
        let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut len) };
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: the buffer is valid for at least `len` bytes.
    unsafe { GetTokenInformation(token, class, Some(buf.as_mut_ptr().cast()), len, &mut len) }
        .map_err(|e| format!("GetTokenInformation({}): {e}", class.0))?;
    Ok(buf)
}

/// The token of process `pid` (0: this process).
pub fn token_info(pid: u32) -> Result<TokenInfo, String> {
    let pid = if pid == 0 { std::process::id() } else { pid };
    // SAFETY: the handles are closed below; every buffer is sized by the
    // API and outlives the pointers read from it.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(process);
        opened.map_err(|e| format!("OpenProcessToken({pid}): {e}"))?;
        let result = (|| {
            let buf = token_information(token, TokenElevation, Some(size_of::<TOKEN_ELEVATION>()))?;
            let elevated = (*(buf.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated != 0;
            let buf = token_information(token, TokenElevationType, Some(size_of::<TOKEN_ELEVATION_TYPE>()))?;
            let elevation_type = (*(buf.as_ptr() as *const TOKEN_ELEVATION_TYPE)).0;
            let buf = token_information(token, TokenIntegrityLevel, None)?;
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            let integrity = *GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1);
            let buf = token_information(token, TokenGroups, None)?;
            let groups = &*(buf.as_ptr() as *const TOKEN_GROUPS);
            let list: &[SID_AND_ATTRIBUTES] =
                std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
            let mut admins = AdminGroup::Absent;
            for g in list {
                if sid_string(g.Sid)? == ADMINISTRATORS_SID {
                    admins = if g.Attributes & SE_GROUP_USE_FOR_DENY_ONLY != 0 {
                        AdminGroup::DenyOnly
                    } else if g.Attributes & SE_GROUP_ENABLED != 0 {
                        AdminGroup::Enabled
                    } else {
                        AdminGroup::Other(g.Attributes)
                    };
                }
            }
            let buf = token_information(token, TokenUser, None)?;
            let user = sid_string((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid)?;
            Ok(TokenInfo { elevated, elevation_type, integrity, admins, user })
        })();
        let _ = CloseHandle(token);
        result
    }
}

// ---------------------------------------------------------------------------
// DACLs
// ---------------------------------------------------------------------------

/// One ACE of a DACL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ace {
    /// `ACE_HEADER::AceType` (0: access allowed, 1: access denied).
    pub kind: u8,
    /// `ACE_HEADER::AceFlags` (`0x1` OI, `0x2` CI, `0x10` inherited).
    pub flags: u8,
    pub mask: u32,
    pub sid: String,
}

/// The DACL of a file or directory.
#[derive(Clone, Debug)]
pub struct Dacl {
    /// `SE_DACL_PROTECTED`: no inheritance from the parent.
    pub protected: bool,
    pub aces: Vec<Ace>,
    /// The DACL as SDDL (for the log).
    pub sddl: String,
}

pub const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
pub const OBJECT_INHERIT_ACE: u8 = 0x1;
pub const CONTAINER_INHERIT_ACE: u8 = 0x2;
pub const INHERITED_ACE: u8 = 0x10;
/// `FILE_ALL_ACCESS` (SDDL `FA`).
pub const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// `SE_DACL_PROTECTED`.
const SE_DACL_PROTECTED: u16 = 0x1000;

/// Reads the DACL of `path` (`GetNamedSecurityInfoW`).
pub fn read_dacl(path: &Path) -> Result<Dacl, String> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: valid path and out pointers; `sd` is freed with LocalFree
    // below, and `dacl` points into it.
    unsafe {
        let err = GetNamedSecurityInfoW(
            &HSTRING::from(path),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        );
        if err != ERROR_SUCCESS {
            return Err(format!("GetNamedSecurityInfoW({}): {err:?}", path.display()));
        }
        let result = (|| {
            let (mut control, mut revision) = (0u16, 0u32);
            GetSecurityDescriptorControl(sd, &mut control, &mut revision)
                .map_err(|e| format!("GetSecurityDescriptorControl: {e}"))?;
            let mut sddl = PWSTR::null();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                None,
            )
            .map_err(|e| format!("ConvertSecurityDescriptorToStringSecurityDescriptorW: {e}"))?;
            let sddl_text = sddl.to_string().unwrap_or_default();
            let _ = LocalFree(Some(HLOCAL(sddl.0.cast())));
            if dacl.is_null() {
                return Err(format!("{} has a NULL DACL (everyone has access)", path.display()));
            }
            let mut size = ACL_SIZE_INFORMATION::default();
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
            .map_err(|e| format!("GetAclInformation: {e}"))?;
            let mut aces = Vec::new();
            for i in 0..size.AceCount {
                let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                GetAce(dacl, i, &mut ace).map_err(|e| format!("GetAce({i}): {e}"))?;
                let header = &*(ace as *const ACE_HEADER);
                // Access-allowed and access-denied ACEs share this layout;
                // other (object, callback) types are reported without a SID.
                let (mask, sid) = if header.AceType <= 1 {
                    let a = &*(ace as *const ACCESS_ALLOWED_ACE);
                    (a.Mask, sid_string(PSID(&a.SidStart as *const u32 as *mut _))?)
                } else {
                    (0, format!("<ACE type {}>", header.AceType))
                };
                aces.push(Ace { kind: header.AceType, flags: header.AceFlags, mask, sid });
            }
            Ok(Dacl { protected: control & SE_DACL_PROTECTED != 0, aces, sddl: sddl_text })
        })();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        result
    }
}

// ---------------------------------------------------------------------------
// Task Scheduler (inspection)
// ---------------------------------------------------------------------------

/// What the tests check of a registered task.
#[derive(Clone, Debug)]
pub struct TaskState {
    pub enabled: bool,
    pub last_run: f64,
    pub trigger_types: Vec<TASK_TRIGGER_TYPE2>,
    /// `Delay` of the logon trigger (empty: none).
    pub logon_delay: Option<String>,
    pub exe: String,
    pub args: String,
    /// The principal's run level (`TASK_RUNLEVEL_LUA`: no elevation).
    pub run_level: TASK_RUNLEVEL_TYPE,
    /// The principal's logon type.
    pub logon_type: TASK_LOGON_TYPE,
}

/// Whether the task folder `path` (for example `\ESRTest_1_x`) exists.
pub fn task_folder_exists(path: &str) -> Result<bool, String> {
    let err = |what: &str, e: windows::core::Error| format!("{what}: {}", e.message());
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let service: ITaskService =
            CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).map_err(|e| err("TaskScheduler", e))?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty).map_err(|e| err("Connect", e))?;
        match service.GetFolder(&BSTR::from(path)) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() || e.code() == ERROR_PATH_NOT_FOUND.to_hresult() => {
                Ok(false)
            }
            Err(e) => Err(err("GetFolder", e)),
        }
    }
}

pub fn query_task(path: &str) -> Result<Option<TaskState>, String> {
    let err = |what: &str, e: windows::core::Error| format!("{what}: {}", e.message());
    // SAFETY: plain COM calls on live interfaces with valid out pointers.
    unsafe {
        let service: ITaskService =
            CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).map_err(|e| err("TaskScheduler", e))?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty).map_err(|e| err("Connect", e))?;
        let root = service.GetFolder(&BSTR::from(r"\")).map_err(|e| err("GetFolder", e))?;
        let task = match root.GetTask(&BSTR::from(path)) {
            Ok(t) => t,
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(None),
            Err(e) if e.code() == ERROR_PATH_NOT_FOUND.to_hresult() => return Ok(None),
            Err(e) => return Err(err("GetTask", e)),
        };
        let def = task.Definition().map_err(|e| err("Definition", e))?;
        let triggers = def.Triggers().map_err(|e| err("Triggers", e))?;
        let mut count = 0;
        triggers.Count(&mut count).map_err(|e| err("Count", e))?;
        let mut trigger_types = Vec::new();
        let mut logon_delay = None;
        for i in 1..=count {
            let t = triggers.get_Item(i).map_err(|e| err("get_Item", e))?;
            let mut ty = TASK_TRIGGER_TYPE2::default();
            t.Type(&mut ty).map_err(|e| err("Type", e))?;
            if ty == TASK_TRIGGER_LOGON {
                let logon: ILogonTrigger = t.cast().map_err(|e| err("ILogonTrigger", e))?;
                let mut delay = BSTR::new();
                logon.Delay(&mut delay).map_err(|e| err("Delay", e))?;
                logon_delay = Some(delay.to_string());
            }
            trigger_types.push(ty);
        }
        let action = def.Actions().and_then(|a| a.get_Item(1)).map_err(|e| err("Actions", e))?;
        let exec: IExecAction = action.cast().map_err(|e| err("IExecAction", e))?;
        let (mut exe, mut args) = (BSTR::new(), BSTR::new());
        exec.Path(&mut exe).map_err(|e| err("Path", e))?;
        exec.Arguments(&mut args).map_err(|e| err("Arguments", e))?;
        let principal = def.Principal().map_err(|e| err("Principal", e))?;
        let mut run_level = TASK_RUNLEVEL_TYPE::default();
        principal.RunLevel(&mut run_level).map_err(|e| err("RunLevel", e))?;
        let mut logon_type = TASK_LOGON_TYPE::default();
        principal.LogonType(&mut logon_type).map_err(|e| err("LogonType", e))?;
        Ok(Some(TaskState {
            enabled: task.Enabled().map_err(|e| err("Enabled", e))?.as_bool(),
            last_run: task.LastRunTime().unwrap_or(0.0),
            trigger_types,
            logon_delay,
            exe: exe.to_string(),
            args: args.to_string(),
            run_level,
            logon_type,
        }))
    }
}

// ---------------------------------------------------------------------------
// PE imports and network endpoints (Req 11.2)
// ---------------------------------------------------------------------------

/// The DLLs a PE image imports (import and delay-import tables), lowercase.
pub fn pe_imported_dlls(path: &Path) -> Result<Vec<String>, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let u16_at = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize);
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize);
    let bad = || "malformed PE image".to_string();
    let pe = u32_at(0x3C).ok_or_else(bad)?;
    if b.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        return Err("no PE signature".into());
    }
    let sections = u16_at(pe + 6).ok_or_else(bad)?;
    let opt_size = u16_at(pe + 20).ok_or_else(bad)?;
    let opt = pe + 24;
    let dirs = match u16_at(opt).ok_or_else(bad)? {
        0x20B => opt + 112, // PE32+
        0x10B => opt + 96,  // PE32
        m => return Err(format!("unknown optional header magic {m:#x}")),
    };
    let table = opt + opt_size;
    let rva_to_offset = |rva: usize| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + i * 40;
            let (vsize, va, raw_size, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 16)?, u32_at(s + 20)?);
            (rva >= va && rva < va + vsize.max(raw_size)).then(|| rva - va + raw)
        })
    };
    let c_string = |rva: usize| -> Option<String> {
        let o = rva_to_offset(rva)?;
        let end = b.get(o..)?.iter().position(|&c| c == 0)?;
        Some(String::from_utf8_lossy(&b[o..o + end]).to_lowercase())
    };
    let mut out = Vec::new();
    // Data directory 1: imports (20-byte descriptors, name RVA at +12).
    if let Some(rva) = u32_at(dirs + 8).filter(|&r| r != 0) {
        let mut d = rva_to_offset(rva).ok_or_else(bad)?;
        while let Some(name) = u32_at(d + 12).filter(|&n| n != 0) {
            out.push(c_string(name).ok_or_else(bad)?);
            d += 20;
        }
    }
    // Data directory 13: delay imports (32-byte descriptors, name RVA at +4).
    if let Some(rva) = u32_at(dirs + 13 * 8).filter(|&r| r != 0) {
        let mut d = rva_to_offset(rva).ok_or_else(bad)?;
        while let Some(name) = u32_at(d + 4).filter(|&n| n != 0) {
            out.push(c_string(name).ok_or_else(bad)?);
            d += 32;
        }
    }
    Ok(out)
}

/// The TCP/UDP endpoints of process `pid` (`netstat -ano` lines).
pub fn network_endpoints(pid: u32) -> Result<Vec<String>, String> {
    let netstat = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("NETSTAT.EXE");
    let out = Process::new(netstat)
        .arg("-ano")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("netstat -ano: {e}"))?;
    if !out.status.success() {
        return Err(format!("netstat -ano failed: {:?}", out.status));
    }
    let pid = pid.to_string();
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            matches!(cols.first(), Some(&"TCP") | Some(&"UDP")) && cols.last() == Some(&pid.as_str())
        })
        .map(|l| l.trim().to_owned())
        .collect())
}
