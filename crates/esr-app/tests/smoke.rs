//! Smoke tests (task 16.7; Requirements 1.8, 11.1, 11.2, 13.6).
//!
//! * [`install_and_uninstall_as_a_standard_user`] (`#[ignore]`) runs the real
//!   `esr.exe --install` and then the Installed-apps `QuietUninstallString`
//!   (`"<installed esr.exe>" --uninstall --quiet`) without elevation, checks
//!   what install creates, the DACL of the data folder and of every file in
//!   it, that the running monitor has no administrator rights and no network
//!   endpoint, and that uninstall removes the registration, the program, and
//!   the data (Req 12.6) while the open Explorer window stays open. On the
//!   way, the monitor is exited once (`ESR_EXIT`, as Exit in Settings) so it
//!   writes its Activity_Log, and started again with `--background` (the
//!   logon task's command line); uninstall then stops the running monitor.
//! * [`cargo_deny_check_passes_and_bans_bite`] (`#[ignore]`) runs
//!   `cargo deny check` on the workspace (no networking crate and no
//!   networking feature of the Windows bindings, licenses, sources,
//!   advisories) and shows with three altered copies of `deny.toml` that the
//!   crate allow-list, the exact feature list of `windows`, and the feature
//!   deny-list really fail the check.
//! * [`esr_exe_imports_no_networking_dll`] (runs by default) reads the import
//!   and delay-import tables of the `esr.exe` under test: no WinSock, WinHTTP,
//!   WinINet, or other networking DLL is linked.
//!
//! Run the ignored tests alone:
//!
//! ```text
//! cargo test -p esr-app --test smoke -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `cargo_deny_check_passes_and_bans_bite` needs cargo-deny
//! (`cargo install cargo-deny --locked --version 0.20.2`). It fetches the
//! advisory database; with `ESR_DENY_OFFLINE=1` it runs only
//! `bans licenses sources`.
//!
//! # Standard user, no elevation
//!
//! The test refuses to run from an elevated process: everything it starts
//! inherits its token, so an elevated run would prove nothing. It prints the
//! token of the test process and asserts from the token of the installed
//! monitor (started by `--install`, so it carries the token `--install` ran
//! with; it is also the token `--uninstall` runs with) that it is not
//! elevated, has at most medium integrity, and has the Administrators group
//! absent or deny-only. On an administrator account with UAC (the usual dev
//! VM) that is the filtered token Windows gives every program that is not
//! "Run as administrator"; on a standard account the group is absent. Both
//! have no administrator rights, so anything that needed them (writing to
//! `HKLM`, `Program Files`, a machine-wide task) would fail the commands.
//! `esr.exe`'s manifest requests `asInvoker`, so Windows never shows a UAC
//! prompt for it (an executable that requires elevation cannot be started
//! from an unelevated process without one; `CreateProcess` fails with
//! `ERROR_ELEVATION_REQUIRED`).
//!
//! # Isolation and cleanup
//!
//! The test copy is the test build of `esr.exe` with its PE subsystem patched
//! to GUI (as in `startup_coexistence.rs`). Next to it, the overrides file of
//! `esr_app::dev_overrides` (debug builds only) moves every location into the
//! test root `%LOCALAPPDATA%\ESRTest_<pid>_smoke` (laid out like the real
//! profile: `Programs\ExplorerSessionRestore`, `ExplorerSessionRestore`,
//! `Start Menu\Programs`), and names the logon task folder, the Run value,
//! and the Installed-apps entry `ESRTest_<pid>_smoke`. `--install` copies the
//! file along with the program, so the installed copy and the `%TEMP%`
//! uninstall copy use the same test locations. The real data directory, the
//! real program folder, the real Start-menu shortcut, the real
//! `\ExplorerSessionRestore` task and the real Installed-apps entry are never
//! touched; the test asserts that before it starts anything. The copies take
//! the real single-instance mutex, so the test first checks that no instance
//! runs. [`SmokeInstall`] stops every test instance, removes the test task,
//! Run value and Installed-apps entry, and deletes the test root and the
//! `%TEMP%` uninstall folder, in `finish` and in `Drop` (also on panic). The
//! one Explorer window is opened through `ExplorerGuard` and closed by it.

#![cfg(windows)]

#[path = "../../esr-win/tests/common/explorer_guard.rs"]
#[allow(dead_code)]
mod explorer_guard;
mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use common::{
    ACCESS_ALLOWED_ACE_TYPE, ADMINISTRATORS_SID, CONTAINER_INHERIT_ACE, FILE_ALL_ACCESS, INHERITED_ACE,
    OBJECT_INHERIT_ACE, SYSTEM_SID, instance_of, lists, make_gui_subsystem, network_endpoints, open_window,
    pe_imported_dlls, post_exit, query_task, read_dacl, read_doc, require_debug_build, require_no_running_instance,
    run_to_end, stop_instances_of, task_folder_exists, token_info, wait, window_alive, window_folders,
};
use esr_app::dev_overrides::{self, DevOverrides};
use esr_app::install::{self, Layout};
use esr_core::logfmt::{ACTIVITY_LOG_FILE_NAME, PREVIOUS_LOG_FILE_NAME};
use esr_core::settings::Settings;
use esr_core::store::{BACKUP_FILE, SESSION_FILE};
use esr_win::placeholder::PLACEHOLDER_XML;
use esr_win::shell_util::StaGuard;
use esr_win::startup::{APP_STARTUP_NAME, BACKGROUND_ARG, StartupConfig, StartupRegistration, quote_program_path};
use explorer_guard::{ExplorerGuard, pump_for};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW,
};
use windows::Win32::System::TaskScheduler::{TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_LUA, TASK_TRIGGER_LOGON};
use windows::core::HSTRING;

/// `--install` and `--uninstall` normally end within a few seconds.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// The installed monitor is running this soon after `--install` ended.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// The `%TEMP%` copy deletes the program folder, then itself (2 s delay).
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn reg_sz(subkey: &str, name: &str) -> Option<String> {
    let mut buf = vec![0u16; 2048];
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
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn reg_key_exists(subkey: &str) -> Result<bool, String> {
    let mut key = HKEY::default();
    // SAFETY: valid strings and out pointer; the key is closed below.
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(subkey), None, KEY_READ, &mut key) };
    if r == ERROR_SUCCESS {
        // SAFETY: opened above.
        unsafe {
            let _ = RegCloseKey(key);
        }
        return Ok(true);
    }
    if r == ERROR_FILE_NOT_FOUND { Ok(false) } else { Err(format!("RegOpenKeyExW({subkey}): {r:?}")) }
}

/// Splits `"<exe>" args` into the program and its arguments.
fn split_command(command: &str) -> Option<(PathBuf, Vec<String>)> {
    let rest = command.strip_prefix('"')?;
    let (exe, args) = rest.split_once('"')?;
    Some((PathBuf::from(exe), args.split_whitespace().map(str::to_owned).collect()))
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Every file and folder below `dir` (not `dir` itself).
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Req 11.1, 13.6: the data folder grants access to exactly the user,
/// SYSTEM, and Administrators, with inheritance from `%LOCALAPPDATA%`
/// stopped; every file and folder inside inherits exactly those three.
fn check_data_dacls(data_dir: &Path, user: &str) -> Result<(), String> {
    let expected: BTreeSet<String> = [user, SYSTEM_SID, ADMINISTRATORS_SID].iter().map(|s| s.to_string()).collect();
    let dacl = read_dacl(data_dir)?;
    println!("[smoke] DACL of {}: {}", data_dir.display(), dacl.sddl);
    let sids: BTreeSet<String> = dacl.aces.iter().map(|a| a.sid.clone()).collect();
    if !dacl.protected {
        return Err(format!("the data folder inherits ACEs from its parent: {}", dacl.sddl));
    }
    if dacl.aces.len() != 3 || sids != expected {
        return Err(format!("the data folder grants {sids:?} (expected exactly {expected:?}): {}", dacl.sddl));
    }
    for ace in &dacl.aces {
        let oici = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
        if ace.kind != ACCESS_ALLOWED_ACE_TYPE
            || ace.mask != FILE_ALL_ACCESS
            || ace.flags & oici != oici
            || ace.flags & INHERITED_ACE != 0
        {
            return Err(format!("unexpected ACE on the data folder: {ace:?} ({})", dacl.sddl));
        }
    }
    let items = tree(data_dir);
    for item in &items {
        let dacl = read_dacl(item)?;
        let sids: BTreeSet<String> = dacl.aces.iter().map(|a| a.sid.clone()).collect();
        let all_inherited_allows = dacl
            .aces
            .iter()
            .all(|a| a.kind == ACCESS_ALLOWED_ACE_TYPE && a.flags & INHERITED_ACE != 0);
        println!("[smoke] DACL of {}: {}", item.display(), dacl.sddl);
        if sids != expected || !all_inherited_allows {
            return Err(format!("{} grants {sids:?} (expected the inherited {expected:?}): {}", item.display(), dacl.sddl));
        }
    }
    println!("[smoke] {} item(s) in the data folder checked", items.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// SmokeInstall
// ---------------------------------------------------------------------------

/// The test copy and its test locations (see the module documentation).
struct SmokeInstall {
    name: String,
    root: PathBuf,
    /// The "downloaded" program `--install` is run from.
    src_exe: PathBuf,
    /// The locations the product computes from the overrides file.
    layout: Layout,
    sid: String,
    /// `%TEMP%` folders of uninstall copies that were started.
    handoff_dirs: Vec<PathBuf>,
    done: bool,
}

impl SmokeInstall {
    fn new() -> Result<Self, String> {
        require_debug_build();
        let pid = std::process::id();
        let name = format!("ESRTest_{pid}_smoke");
        let real = Layout::for_current_user().map_err(|e| e.message())?;
        let local_app_data = real.data_dir.parent().ok_or("no %LOCALAPPDATA%")?;
        let root = local_app_data.join(&name);
        let _ = std::fs::remove_dir_all(&root);
        let src_exe = root.join("Downloads").join("esr.exe");
        std::fs::create_dir_all(src_exe.parent().unwrap()).map_err(|e| e.to_string())?;
        let sid = esr_win::file_store::current_user_sid().map_err(|e| e.message())?;
        let mut me = Self {
            name: name.clone(),
            root: root.clone(),
            src_exe: src_exe.clone(),
            layout: real.clone(),
            sid,
            handoff_dirs: Vec::new(),
            done: false,
        };
        std::fs::copy(env!("CARGO_BIN_EXE_esr"), &src_exe).map_err(|e| format!("copy esr.exe: {e}"))?;
        make_gui_subsystem(&src_exe)?;

        let overrides = DevOverrides {
            data_dir: Some(root.join(esr_win::file_store::DATA_DIR_NAME)),
            startup_name: Some(name.clone()),
            install_dir: Some(root.join("Programs").join(install::INSTALL_DIR_NAME)),
            start_menu_dir: Some(root.join("Start Menu").join("Programs")),
        };
        let text = format!(
            "# task 16.7 smoke test\ndata_dir={}\nstartup_name={name}\ninstall_dir={}\nstart_menu_dir={}\n",
            overrides.data_dir.as_ref().unwrap().display(),
            overrides.install_dir.as_ref().unwrap().display(),
            overrides.start_menu_dir.as_ref().unwrap().display(),
        );
        std::fs::write(src_exe.with_file_name(dev_overrides::FILE_NAME), text).map_err(|e| e.to_string())?;
        let seen = DevOverrides::for_exe(&src_exe);
        if seen != overrides {
            return Err(format!("the overrides file is not read back as written: {seen:?}"));
        }
        // Exactly what `--install` computes from the file.
        me.layout = real.clone().with_overrides(&seen)?;
        let l = &me.layout;

        // Nothing real is touched.
        for (test, prod) in [(&l.data_dir, &real.data_dir), (&l.install_dir, &real.install_dir), (&l.shortcut, &real.shortcut)] {
            if install::same_path(test, prod) || !install::is_inside(test, &root) {
                return Err(format!("{} is not a test location (real: {})", test.display(), prod.display()));
            }
        }
        if l.uninstall_key == real.uninstall_key || !l.uninstall_key.ends_with(&format!(r"\{name}")) {
            return Err(format!("unexpected Installed-apps key {}", l.uninstall_key));
        }
        if l.startup_name == APP_STARTUP_NAME || l.startup_name != name {
            return Err(format!("unexpected startup name {}", l.startup_name));
        }
        println!(
            "[smoke] test copy {} -> {} (data {}, task \\{name}\\<SID>, HKCU\\{})",
            src_exe.display(),
            l.install_dir.display(),
            l.data_dir.display(),
            l.uninstall_key
        );
        Ok(me)
    }

    fn config(&self) -> StartupConfig {
        StartupConfig::with_base_name(&self.name, &self.sid, self.layout.installed_exe())
    }

    /// Stops every test instance, removes the test registration and the
    /// test folders, and reports anything left behind.
    fn cleanup(&mut self) -> Vec<String> {
        self.done = true;
        let mut problems = Vec::new();
        for exe in [self.layout.installed_exe(), self.src_exe.clone()] {
            problems.extend(stop_instances_of(&exe, "smoke"));
        }
        let mut reg = StartupRegistration::new(self.config());
        if let Err(e) = reg.unregister() {
            problems.push(format!("unregister: {}", e.message()));
        }
        let config = self.config();
        for path in [config.folder_task_path(), config.root_task_path()] {
            match query_task(&path) {
                Ok(None) => {}
                Ok(Some(_)) => problems.push(format!("task {path} still exists")),
                Err(e) => problems.push(format!("query {path}: {e}")),
            }
        }
        if !matches!(reg.run_value(), Ok(None)) {
            problems.push(format!("Run value {} still exists", config.run_value_name));
        }
        let key = &self.layout.uninstall_key;
        assert!(key.ends_with(&self.name), "never deletes another key: {key}");
        if let Err(e) = install::remove_registry_key(key) {
            problems.push(format!("delete HKCU\\{key}: {}", e.message()));
        }
        let mut dirs = self.handoff_dirs.clone();
        dirs.push(self.root.clone());
        for dir in dirs {
            for _ in 0..80 {
                if !dir.exists() || std::fs::remove_dir_all(&dir).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if dir.exists() {
                problems.push(format!("{} could not be deleted", dir.display()));
            }
        }
        if problems.is_empty() {
            println!("[smoke] cleanup complete: instances stopped, task, Run value, Installed-apps entry, and files removed");
        }
        problems
    }

    fn finish(mut self) -> Result<(), String> {
        let problems = self.cleanup();
        if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
    }
}

impl Drop for SmokeInstall {
    fn drop(&mut self) {
        if !self.done {
            let problems = self.cleanup();
            if !problems.is_empty() {
                eprintln!("[smoke] CLEANUP INCOMPLETE: {}", problems.join("; "));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Req 1.8, 11.1, 11.2, 12.6, 13.6: install and uninstall without elevation
// ---------------------------------------------------------------------------

#[test]
#[ignore = "installs and uninstalls a test copy of esr.exe (test task, test Installed-apps entry), opens one File Explorer window; run alone"]
fn install_and_uninstall_as_a_standard_user() {
    let _sta = StaGuard::init().expect("STA init");
    let me = token_info(0).expect("token of the test process");
    println!("[smoke] test process token: {me:?}");
    assert!(
        me.is_standard_user(),
        "run this test from a non-elevated process: an elevated test would start --install elevated too ({me:?})"
    );
    let manifest = include_str!("../esr.manifest");
    assert!(manifest.contains(r#"requestedExecutionLevel level="asInvoker""#), "esr.exe must never request elevation");

    let guard = ExplorerGuard::new("smoke_install");
    let mut setup = SmokeInstall::new().expect("test install");

    let result = (|| -> Result<(), String> {
        require_no_running_instance()?;
        let l = setup.layout.clone();
        let exe = l.installed_exe();

        // ---- Install ----
        let (code, took, _) = run_to_end(&setup.src_exe, &[install::INSTALL_FLAG, install::QUIET_FLAG], COMMAND_TIMEOUT)?;
        println!("[smoke] --install exited with {code} after {took:?}");
        if code != 0 {
            return Err(format!("--install failed with exit code {code}"));
        }
        if std::fs::read(&exe).ok() != std::fs::read(&setup.src_exe).ok() {
            return Err(format!("{} is not a copy of the program", exe.display()));
        }
        if !l.install_dir.join(dev_overrides::FILE_NAME).is_file() || !l.shortcut.is_file() {
            return Err("the overrides file or the Start-menu shortcut was not created".into());
        }
        // Start at sign-in: a logon task for this user only, no elevation.
        let config = setup.config();
        let task = query_task(&config.folder_task_path())?
            .ok_or_else(|| format!("no logon task {}", config.folder_task_path()))?;
        println!("[smoke] logon task {}: {task:?}", config.folder_task_path());
        let quoted = quote_program_path(&exe).map_err(|e| e.message())?;
        if !task.enabled
            || task.trigger_types != vec![TASK_TRIGGER_LOGON]
            || task.exe != quoted
            || task.args != BACKGROUND_ARG
            || task.run_level != TASK_RUNLEVEL_LUA
            || task.logon_type != TASK_LOGON_INTERACTIVE_TOKEN
        {
            return Err(format!("unexpected logon task: {task:?}"));
        }
        if StartupRegistration::new(config.clone()).run_value().map_err(|e| e.message())?.is_some() {
            return Err("a Run value exists next to the logon task".into());
        }
        // The Installed-apps entry.
        let quiet = reg_sz(&l.uninstall_key, "QuietUninstallString")
            .ok_or_else(|| format!("no QuietUninstallString under HKCU\\{}", l.uninstall_key))?;
        let display = reg_sz(&l.uninstall_key, "DisplayName");
        println!("[smoke] Installed-apps entry {display:?}: QuietUninstallString = {quiet}");
        let (uninstaller, args) = split_command(&quiet).ok_or_else(|| format!("unparsable {quiet}"))?;
        if !install::same_path(&uninstaller, &exe) || args != [install::UNINSTALL_FLAG, install::QUIET_FLAG] {
            return Err(format!("QuietUninstallString does not run the installed program: {quiet}"));
        }

        // ---- The installed monitor runs without administrator rights ----
        let (engine, pid) = wait(START_TIMEOUT, || instance_of(&exe))
            .ok_or_else(|| format!("the installed program was not running {START_TIMEOUT:?} after --install"))?;
        let token = token_info(pid)?;
        println!("[smoke] installed monitor pid {pid}: {token:?}");
        if !token.is_standard_user() {
            return Err(format!("the installed monitor runs with administrator rights: {token:?}"));
        }
        if token.user != setup.sid {
            return Err(format!("the installed monitor runs as {} (expected {})", token.user, setup.sid));
        }

        // ---- Data: Session_File, settings, logs, placeholders ----
        let window_dir = guard.dir("smoke_window");
        let hwnd = open_window(&guard, &window_dir)?;
        let doc = wait(Duration::from_secs(15), || read_doc(&l.data_dir).filter(|d| lists(d, &window_dir)))
            .ok_or("the installed monitor did not record the open window in its Session_File")?;
        println!("[smoke] Session_File lists the window ({:?}, {:?})", doc.status, doc.cause);
        // The Activity_Log gets its first line at a Session_End: exit the
        // monitor (as Exit in Settings does) and start it again the way the
        // logon task does, so the real log file is checked too.
        post_exit(engine)?;
        if !install::wait_for_process_exit(pid, Duration::from_secs(5)) {
            return Err("the monitor did not exit within 5 s of ESR_EXIT".into());
        }
        let log = std::fs::read_to_string(l.data_dir.join(ACTIVITY_LOG_FILE_NAME)).unwrap_or_default();
        if !log.lines().any(|line| line.contains(" SAVE ")) {
            return Err(format!("no SAVE line in the Activity_Log after the exit: {log:?}"));
        }
        Process::new(&exe)
            .arg(BACKGROUND_ARG)
            .current_dir(&l.install_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("restart {}: {e}", exe.display()))?;
        let (_, pid) = wait(START_TIMEOUT, || instance_of(&exe)).ok_or("the installed program did not start again")?;
        println!("[smoke] monitor exited (SAVE logged) and runs again as pid {pid}");
        // Files the monitor writes only in rare cases, written here so that
        // uninstall must remove them too (they inherit the folder's DACL).
        // settings.ini is written only by a Settings change, with the same
        // atomic replace as the Session_File.
        if !l.settings_file().exists() {
            std::fs::write(l.settings_file(), Settings::default().serialize()).map_err(|e| e.to_string())?;
        }
        std::fs::write(l.data_dir.join(BACKUP_FILE.target), b"unreadable session (test)").map_err(|e| e.to_string())?;
        std::fs::write(l.data_dir.join(PREVIOUS_LOG_FILE_NAME), b"previous log (test)\r\n").map_err(|e| e.to_string())?;
        let slot = l.placeholder_dir().join("1");
        std::fs::create_dir_all(&slot).map_err(|e| e.to_string())?;
        std::fs::write(slot.join("Smoke folder.library-ms"), PLACEHOLDER_XML).map_err(|e| e.to_string())?;
        println!("[smoke] data folder: {:?}", tree(&l.data_dir));
        if !l.data_dir.join(SESSION_FILE.target).is_file() {
            return Err("no Session_File".into());
        }
        check_data_dacls(&l.data_dir, &setup.sid)?;

        // ---- Req 11.2: no network endpoint ----
        let endpoints = network_endpoints(pid)?;
        if !endpoints.is_empty() {
            return Err(format!("the monitor has network endpoints: {endpoints:?}"));
        }
        println!("[smoke] the monitor has no TCP/UDP endpoint (netstat -ano)");
        pump_for(Duration::from_millis(300));
        let before = window_folders(hwnd);

        // ---- Uninstall, as Installed apps runs it ----
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let t0 = Instant::now();
        let (code, took, uninstall_pid) = run_to_end(&uninstaller, &arg_refs, COMMAND_TIMEOUT)?;
        let handoff_dir = l.handoff_exe(uninstall_pid).parent().unwrap().to_owned();
        setup.handoff_dirs.push(handoff_dir.clone());
        println!("[smoke] --uninstall (pid {uninstall_pid}) exited with {code} after {took:?}");
        if code != 0 {
            return Err(format!("--uninstall failed with exit code {code}"));
        }
        if !install::wait_for_process_exit(pid, Duration::from_secs(1)) || instance_of(&exe).is_some() {
            return Err("the monitor is still running after --uninstall".into());
        }
        // The `%TEMP%` copy removes the program folder, then itself.
        wait(HANDOFF_TIMEOUT, || (!l.install_dir.exists() && !handoff_dir.exists()).then_some(()))
            .ok_or_else(|| {
                format!(
                    "after {:?}: program folder exists = {}, uninstall copy {} exists = {}",
                    t0.elapsed(),
                    l.install_dir.exists(),
                    handoff_dir.display(),
                    handoff_dir.exists()
                )
            })?;
        println!("[smoke] program folder and uninstall copy gone {:?} after --uninstall started", t0.elapsed());

        let mut left = Vec::new();
        for path in [config.folder_task_path(), config.root_task_path()] {
            if query_task(&path)?.is_some() {
                left.push(format!("task {path}"));
            }
        }
        if task_folder_exists(&format!(r"\{}", config.folder))? {
            left.push(format!("task folder \\{}", config.folder));
        }
        if StartupRegistration::new(config.clone()).run_value().map_err(|e| e.message())?.is_some() {
            left.push("Run value".into());
        }
        if reg_key_exists(&l.uninstall_key)? {
            left.push(format!("HKCU\\{}", l.uninstall_key));
        }
        for path in [&l.shortcut, &l.data_dir, &l.install_dir] {
            if path.exists() {
                left.push(format!("{} ({:?})", path.display(), tree(path)));
            }
        }
        if !left.is_empty() {
            return Err(format!("uninstall left behind: {left:?}"));
        }
        // Req 12.6: the Explorer window is left open and unchanged.
        if !window_alive(hwnd) || window_folders(hwnd) != before {
            return Err(format!("the Explorer window changed: {before:?} -> {:?}", window_folders(hwnd)));
        }
        println!("[smoke] uninstall removed the task, Installed-apps entry, shortcut, data, and program; the window stayed open");
        Ok(())
    })();

    let cleaned = setup.finish();
    let windows = guard.finish();
    result.unwrap();
    cleaned.unwrap();
    windows.unwrap();
}

// ---------------------------------------------------------------------------
// Req 11.2: cargo deny
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `cargo deny --manifest-path <workspace> [--config <config>] check <checks>`:
/// success and the combined output.
fn cargo_deny(config: Option<&Path>, checks: &[&str]) -> Result<(bool, String), String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Process::new(cargo);
    command.arg("deny").arg("--manifest-path").arg(workspace_root().join("Cargo.toml"));
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    let out = command
        .arg("check")
        .args(checks)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cargo deny: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

#[test]
#[ignore = "runs cargo deny (cargo install cargo-deny --locked --version 0.20.2); fetches the advisory database unless ESR_DENY_OFFLINE=1"]
fn cargo_deny_check_passes_and_bans_bite() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let version = Process::new(&cargo).args(["deny", "--version"]).output();
    let version = match version {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        other => panic!(
            "cargo-deny is not installed ({other:?}); install it with \
             `cargo install cargo-deny --locked --version 0.20.2`"
        ),
    };
    println!("[deny] {version}");

    let offline = std::env::var("ESR_DENY_OFFLINE").is_ok_and(|v| v.trim() == "1");
    let checks: &[&str] = if offline { &["bans", "licenses", "sources"] } else { &[] };
    let (ok, text) = cargo_deny(None, checks).unwrap();
    let summary = text.lines().filter(|l| l.contains(" ok") || l.contains("FAILED")).collect::<Vec<_>>().join(" | ");
    println!("[deny] cargo deny check {checks:?}: {summary}");
    assert!(ok, "cargo deny check failed:\n{text}");
    assert!(!text.contains("warning["), "cargo deny reported warnings:\n{text}");

    // The configuration bites: each altered copy must fail `check bans`.
    let config = std::fs::read_to_string(workspace_root().join("deny.toml")).unwrap().replace("\r\n", "\n");
    let altered = [
        (
            "a windows feature that is not in the exact list (as a newly enabled WinSock would be)",
            config.replacen("    \"Win32_UI_HiDpi\",\n", "", 1),
            "exact-features-mismatch",
        ),
        (
            "a crate that is not in the allow-list (as a new networking crate would be)",
            config.replacen("    { name = \"windows-strings\" },\n", "", 1),
            "not-allowed",
        ),
        (
            "an enabled feature on the deny list",
            config.replacen(
                "crate = \"windows-sys\"\ndeny = [\n",
                "crate = \"windows-sys\"\ndeny = [\n    \"Win32_Foundation\",\n",
                1,
            ),
            "feature-banned",
        ),
    ];
    let dir = std::env::temp_dir().join(format!("esr_deny_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let results: Vec<_> = altered
        .iter()
        .enumerate()
        .map(|(i, (what, text, code))| {
            assert_ne!(text, &config, "the edit for {what} did not apply; update this test with deny.toml");
            let path = dir.join(format!("deny_{i}.toml"));
            std::fs::write(&path, text).unwrap();
            (what, code, cargo_deny(Some(&path), &["bans"]))
        })
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    for (what, code, result) in results {
        let (ok, text) = result.unwrap();
        println!("[deny] {what}: {}", if ok { "PASSED (wrong)" } else { "rejected" });
        assert!(!ok && text.contains(&format!("error[{code}]")), "{what}: expected error[{code}]:\n{text}");
    }
}

// ---------------------------------------------------------------------------
// Req 11.2: no networking DLL in esr.exe
// ---------------------------------------------------------------------------

/// DLLs that provide network access.
const NETWORK_DLLS: &[&str] = &[
    "dnsapi.dll",
    "httpapi.dll",
    "iphlpapi.dll",
    "mpr.dll",
    "mswsock.dll",
    "netapi32.dll",
    "rasapi32.dll",
    "urlmon.dll",
    "webio.dll",
    "websocket.dll",
    "winhttp.dll",
    "wininet.dll",
    "wldap32.dll",
    "ws2_32.dll",
    "wsock32.dll",
];

#[test]
fn esr_exe_imports_no_networking_dll() {
    let exe = Path::new(env!("CARGO_BIN_EXE_esr"));
    let dlls = pe_imported_dlls(exe).unwrap();
    println!("[imports] {}: {dlls:?}", exe.display());
    assert!(dlls.iter().any(|d| d == "user32.dll"), "the import table was not read: {dlls:?}");
    let network: Vec<&String> = dlls
        .iter()
        .filter(|d| NETWORK_DLLS.contains(&d.as_str()) || d.starts_with("api-ms-win-net"))
        .collect();
    assert!(network.is_empty(), "esr.exe links networking DLLs: {network:?}");
}
