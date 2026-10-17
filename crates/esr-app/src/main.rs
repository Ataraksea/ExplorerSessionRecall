//! `esr.exe` entry point.
//!
//! Startup order (task 14.7; design.md "Startup, single instance, and
//! installation"):
//!
//! 1. **Version gate** (Req 1.2, 1.3). An unsupported build shows the
//!    unsupported message and exits after dismissal. Nothing else runs: no
//!    mutex, no data directory, no Session_File.
//! 2. **Single instance** (Req 1.4). The per-user mutex decides the role. The
//!    first instance keeps its [`InstanceGuard`](esr_app::instance::InstanceGuard)
//!    alive until the runtime returns.
//! 3. **`--install` / `--uninstall` / `--uninstall-finish`** (Req 1.8). These
//!    run with either role (`install::run` stops a running instance itself)
//!    and exit with their result. Start at sign-in is registered by
//!    `--install` from the saved start-at-sign-in setting, and later changes
//!    go through `SettingsChanged` → `SetStartAtSignIn` (Req 1.9, 12.2).
//!    A normal start does not re-register it: the design registers start at
//!    sign-in only at install and on a settings change, so running an
//!    uninstalled copy (for example from Downloads) never points the logon
//!    task at that copy. Debug builds honour the test overrides of
//!    [`esr_app::dev_overrides`] here too (every install location; see
//!    `install::Layout::for_exe`).
//! 4. **Second instance**: signal the first instance (`ESR_SHOW_SETTINGS`, or
//!    `ESR_PING` with `--background`) and exit. The Session_File is not
//!    opened.
//! 5. **Normal start** (first instance): open the data directory, load the
//!    settings and the Session_File, and run the runtime (Req 1.1). Debug
//!    builds honour the test overrides of [`esr_app::dev_overrides`] (data
//!    directory and start-at-sign-in name) for tests that start `esr.exe`
//!    through a test logon task.
//!
//! The sequence is [`boot`], generic over [`Boot`] so the order and the
//! paths are unit-tested with a recording fake. The decision after the gate
//! is the pure [`plan_start`]. Only [`Boot::run_primary`] opens the data
//! directory; [`WindowsBoot`] constructs `Storage` nowhere else, so the
//! Session_File cannot be opened on the unsupported, second-instance, or
//! install paths.

// No console window in release builds (Req 1.1). Debug builds keep the
// console for development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use esr_app::dev_overrides::DevOverrides;
use esr_app::install::{self, Invocation};
use esr_app::instance::{self, BACKGROUND_FLAG, InstanceRole, MutexState, SecondaryAction};
use esr_app::runtime::{self, EXIT_OK, EXIT_SETUP_FAILED, RuntimeOptions, Storage};
use esr_app::version;

/// Exit code after the unsupported-build message (Req 1.3).
const EXIT_UNSUPPORTED: i32 = 2;

/// Where a start goes once the build passed the version gate and the
/// single-instance role is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartPath {
    /// `--install`, `--uninstall`, or `--uninstall-finish`, with either role.
    Install(Invocation),
    /// Another instance runs: signal it and exit.
    Secondary(SecondaryAction),
    /// First instance: load settings and the Session_File, run the runtime.
    Run { background: bool },
}

/// Pure decision after the version gate. An install command wins over the
/// role; otherwise the mutex state decides.
fn plan_start(args: &[String], mutex: MutexState) -> StartPath {
    if let Some(invocation) = Invocation::from_args(args) {
        return StartPath::Install(invocation);
    }
    match instance::decide(mutex, args) {
        instance::Decision::Secondary { action } => StartPath::Secondary(action),
        instance::Decision::Primary => StartPath::Run {
            background: args.iter().any(|a| a == BACKGROUND_FLAG),
        },
    }
}

/// The side effects of starting, in the order [`boot`] calls them.
trait Boot {
    /// The single-instance role; holding it keeps the mutex (first instance).
    type Role;

    /// The version gate. Shows the unsupported message and waits for its
    /// dismissal before returning `false`.
    fn version_supported(&mut self) -> bool;
    /// Acquires the per-user mutex.
    fn acquire_instance(&mut self, args: &[String]) -> Self::Role;
    /// Whether `role` created the mutex.
    fn mutex_state(role: &Self::Role) -> MutexState;
    /// Runs an install command and returns its exit code.
    fn install(&mut self, invocation: Invocation, role: Self::Role) -> i32;
    /// Signals the first instance (silent on failure).
    fn signal_primary(&mut self, action: SecondaryAction);
    /// Opens the data directory, loads settings and the Session_File, and
    /// runs the runtime while holding `role`. The only step that touches the
    /// Session_File.
    fn run_primary(&mut self, role: Self::Role, background: bool) -> i32;
}

/// The startup sequence (see the module documentation). Returns the exit code.
fn boot<B: Boot>(args: &[String], b: &mut B) -> i32 {
    if !b.version_supported() {
        return EXIT_UNSUPPORTED;
    }
    let role = b.acquire_instance(args);
    match plan_start(args, B::mutex_state(&role)) {
        StartPath::Install(invocation) => b.install(invocation, role),
        StartPath::Secondary(action) => {
            drop(role);
            b.signal_primary(action);
            EXIT_OK
        }
        StartPath::Run { background } => b.run_primary(role, background),
    }
}

/// The real side effects.
struct WindowsBoot;

impl Boot for WindowsBoot {
    type Role = InstanceRole;

    fn version_supported(&mut self) -> bool {
        version::check_or_notify()
    }

    /// If the mutex cannot be created at all, this process cannot prove it is
    /// the only instance, so it acts as a second instance: it never runs a
    /// duplicate monitor and never opens the Session_File. (An install
    /// command then retries the mutex itself and reports the failure.)
    fn acquire_instance(&mut self, args: &[String]) -> InstanceRole {
        instance::acquire(args).unwrap_or_else(|_| InstanceRole::Secondary {
            action: SecondaryAction::from_args(args),
        })
    }

    fn mutex_state(role: &InstanceRole) -> MutexState {
        match role {
            InstanceRole::Primary(_) => MutexState::Created,
            InstanceRole::Secondary { .. } => MutexState::AlreadyExists,
        }
    }

    fn install(&mut self, invocation: Invocation, role: InstanceRole) -> i32 {
        install::run(invocation, role)
    }

    fn signal_primary(&mut self, action: SecondaryAction) {
        let _ = instance::signal_primary(action);
    }

    fn run_primary(&mut self, role: InstanceRole, background: bool) -> i32 {
        // Held until the runtime has returned (the mutex marks this process
        // as the first instance).
        let _guard = role;
        // Debug builds only: test overrides next to the executable.
        let overrides = std::env::current_exe()
            .map(|exe| DevOverrides::for_exe(&exe))
            .unwrap_or_default();
        let data_dir = match overrides.data_dir {
            Some(dir) => dir,
            None => match esr_win::file_store::default_data_dir() {
                Ok(dir) => dir,
                Err(_) => return EXIT_SETUP_FAILED,
            },
        };
        let storage = Storage::open(data_dir);
        let settings = storage.settings();
        let startup = runtime::startup_input(storage.read_session(), background);
        let options = RuntimeOptions { startup_name: overrides.startup_name, ..RuntimeOptions::default() };
        runtime::run(settings, startup, storage, options)
    }
}

fn main() {
    // Lossy so a non-Unicode argument cannot panic; every flag is ASCII.
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let code = boot(&args, &mut WindowsBoot);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_app::install::Command;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        VersionGate,
        Acquire,
        Install(Invocation, MutexState),
        Signal(SecondaryAction),
        /// The only call that would open the data directory / Session_File.
        RunPrimary { mutex: MutexState, background: bool },
    }

    /// Records every effect; `Role` is the mutex state it was given.
    struct Fake {
        supported: bool,
        mutex: MutexState,
        install_code: i32,
        run_code: i32,
        calls: Vec<Call>,
    }

    impl Fake {
        fn new(supported: bool, mutex: MutexState) -> Self {
            Self { supported, mutex, install_code: 7, run_code: 0, calls: Vec::new() }
        }
    }

    impl Boot for Fake {
        type Role = MutexState;
        fn version_supported(&mut self) -> bool {
            self.calls.push(Call::VersionGate);
            self.supported
        }
        fn acquire_instance(&mut self, _: &[String]) -> MutexState {
            self.calls.push(Call::Acquire);
            self.mutex
        }
        fn mutex_state(role: &MutexState) -> MutexState {
            *role
        }
        fn install(&mut self, invocation: Invocation, role: MutexState) -> i32 {
            self.calls.push(Call::Install(invocation, role));
            self.install_code
        }
        fn signal_primary(&mut self, action: SecondaryAction) {
            self.calls.push(Call::Signal(action));
        }
        fn run_primary(&mut self, role: MutexState, background: bool) -> i32 {
            self.calls.push(Call::RunPrimary { mutex: role, background });
            self.run_code
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("esr.exe").chain(list.iter().copied()).map(String::from).collect()
    }

    fn run(supported: bool, mutex: MutexState, list: &[&str]) -> (i32, Vec<Call>) {
        let mut fake = Fake::new(supported, mutex);
        let code = boot(&args(list), &mut fake);
        (code, fake.calls)
    }

    fn opens_session_file(calls: &[Call]) -> bool {
        calls.iter().any(|c| matches!(c, Call::RunPrimary { .. }))
    }

    const ALL_ARGS: &[&[&str]] = &[
        &[],
        &["--background"],
        &["--install"],
        &["--uninstall"],
        &["--uninstall", "--quiet"],
        &["--uninstall-finish", "1234", "--quiet"],
    ];

    #[test]
    fn unsupported_build_stops_after_the_gate() {
        for list in ALL_ARGS {
            for mutex in [MutexState::Created, MutexState::AlreadyExists] {
                let (code, calls) = run(false, mutex, list);
                assert_eq!(code, EXIT_UNSUPPORTED, "{list:?}");
                // No mutex, no install, no signal, no Session_File.
                assert_eq!(calls, vec![Call::VersionGate], "{list:?}");
            }
        }
    }

    #[test]
    fn second_instance_signals_and_never_opens_the_session_file() {
        let (code, calls) = run(true, MutexState::AlreadyExists, &[]);
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            calls,
            vec![Call::VersionGate, Call::Acquire, Call::Signal(SecondaryAction::ShowSettings)]
        );

        let (code, calls) = run(true, MutexState::AlreadyExists, &["--background"]);
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            calls,
            vec![Call::VersionGate, Call::Acquire, Call::Signal(SecondaryAction::Ping)]
        );
    }

    #[test]
    fn install_commands_run_with_either_role() {
        let cases = [
            (&["--install"][..], Command::Install, false),
            (&["--uninstall"][..], Command::Uninstall, false),
            (&["--uninstall", "--quiet"][..], Command::Uninstall, true),
            (
                &["--uninstall-finish", "1234", "--quiet"][..],
                Command::FinishUninstall { parent_pid: Some(1234) },
                true,
            ),
        ];
        for (list, command, quiet) in cases {
            for mutex in [MutexState::Created, MutexState::AlreadyExists] {
                let (code, calls) = run(true, mutex, list);
                assert_eq!(code, 7, "install's exit code is returned ({list:?})");
                assert_eq!(
                    calls,
                    vec![
                        Call::VersionGate,
                        Call::Acquire,
                        Call::Install(Invocation { command, quiet }, mutex),
                    ],
                    "{list:?} {mutex:?}"
                );
                assert!(!opens_session_file(&calls));
            }
        }
    }

    #[test]
    fn first_instance_runs_with_its_guard() {
        let (code, calls) = run(true, MutexState::Created, &[]);
        assert_eq!(code, 0);
        assert_eq!(
            calls,
            vec![
                Call::VersionGate,
                Call::Acquire,
                Call::RunPrimary { mutex: MutexState::Created, background: false },
            ]
        );

        let (_, calls) = run(true, MutexState::Created, &["--background"]);
        assert_eq!(
            calls.last(),
            Some(&Call::RunPrimary { mutex: MutexState::Created, background: true })
        );
    }

    #[test]
    fn runtime_exit_code_is_returned() {
        let mut fake = Fake::new(true, MutexState::Created);
        fake.run_code = EXIT_SETUP_FAILED;
        assert_eq!(boot(&args(&[]), &mut fake), EXIT_SETUP_FAILED);
    }

    /// The Session_File is opened exactly on the supported, first-instance,
    /// non-install path (Req 1.3, 1.4).
    #[test]
    fn session_file_opened_only_on_the_normal_path() {
        for list in ALL_ARGS {
            for supported in [false, true] {
                for mutex in [MutexState::Created, MutexState::AlreadyExists] {
                    let (_, calls) = run(supported, mutex, list);
                    let is_install = Invocation::from_args(args(list)).is_some();
                    let expected = supported && mutex == MutexState::Created && !is_install;
                    assert_eq!(
                        opens_session_file(&calls),
                        expected,
                        "{list:?} supported={supported} {mutex:?}: {calls:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn plan_start_decisions() {
        assert_eq!(
            plan_start(&args(&[]), MutexState::Created),
            StartPath::Run { background: false }
        );
        assert_eq!(
            plan_start(&args(&["--background"]), MutexState::Created),
            StartPath::Run { background: true }
        );
        // Only the exact flag counts.
        assert_eq!(
            plan_start(&args(&["--backgroundx"]), MutexState::Created),
            StartPath::Run { background: false }
        );
        assert_eq!(
            plan_start(&args(&[]), MutexState::AlreadyExists),
            StartPath::Secondary(SecondaryAction::ShowSettings)
        );
        assert_eq!(
            plan_start(&args(&["--background"]), MutexState::AlreadyExists),
            StartPath::Secondary(SecondaryAction::Ping)
        );
        // An install command wins over both roles and over --background.
        for mutex in [MutexState::Created, MutexState::AlreadyExists] {
            assert_eq!(
                plan_start(&args(&["--background", "--uninstall"]), mutex),
                StartPath::Install(Invocation { command: Command::Uninstall, quiet: false })
            );
        }
    }

    #[test]
    fn mutex_state_of_real_roles() {
        assert_eq!(
            WindowsBoot::mutex_state(&InstanceRole::Secondary { action: SecondaryAction::Ping }),
            MutexState::AlreadyExists
        );
        // A real guard on a unique test mutex (never the app's own name).
        let name = format!(r"Local\ESR.Test.Main.{}", std::process::id());
        let role = instance::acquire_named(&name, ["esr.exe"]).unwrap();
        assert_eq!(WindowsBoot::mutex_state(&role), MutexState::Created);
    }
}
