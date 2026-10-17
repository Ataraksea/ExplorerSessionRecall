//! Test-only overrides for integration tests that start `esr.exe` as a
//! separate process, for example through a logon task run on demand (task
//! 16.6, `tests/startup_coexistence.rs`) or as `--install` / `--uninstall`
//! (task 16.7, `tests/smoke.rs`).
//!
//! Such a process is started by Task Scheduler with the exact command line
//! of the real registration (`"<exe>" --background`), so its environment and
//! arguments cannot carry test settings. Instead, a **debug build** reads the
//! file [`FILE_NAME`] next to its executable:
//!
//! ```text
//! # comments and blank lines are ignored
//! data_dir=C:\Users\me\AppData\Local\Temp\esr_startup_1234\data
//! startup_name=ESRTest_1234_startup
//! ```
//!
//! * `data_dir`: the data directory to use instead of
//!   `%LOCALAPPDATA%\ExplorerSessionRecall` (Session_File, settings,
//!   Activity_Log). Must be an absolute path.
//! * `startup_name`: the base name of the start-at-sign-in registration that
//!   a Settings change updates (task folder, root task prefix, and Run value;
//!   see `esr_win::startup::StartupConfig::with_base_name`) instead of
//!   `ExplorerSessionRecall`. Must start with [`TEST_NAME_PREFIX`] and
//!   contain only ASCII letters, digits, `_`, and `-`, so it can never name
//!   the production registration. `--install` / `--uninstall` also use it as
//!   the name of the Installed-apps entry
//!   (`HKCU\...\Uninstall\<startup_name>`).
//! * `install_dir`: the program folder of `--install` / `--uninstall`
//!   instead of `%LOCALAPPDATA%\Programs\ExplorerSessionRecall`.
//! * `start_menu_dir`: the folder of the Start-menu shortcut instead of the
//!   per-user Start menu `Programs` folder.
//!
//! `install_dir` and `start_menu_dir` must be absolute paths. The install
//! commands accept a file only if it sets all four keys (see
//! `crate::install::Layout::with_overrides`), so a test copy never touches
//! a real installation, and they copy the file along with the program so
//! the installed copy and the `%TEMP%` uninstall copy see it too.
//!
//! Invalid or unknown entries are ignored (the default is used). Release
//! builds never read the file ([`DevOverrides::for_exe`] returns the
//! defaults), so the shipped program has no hidden switch.

use std::path::{Path, PathBuf};

/// Name of the overrides file next to the executable.
pub const FILE_NAME: &str = "esr.dev-overrides";
/// Required prefix of an overridden start-at-sign-in base name.
pub const TEST_NAME_PREFIX: &str = "ESRTest_";

/// The overrides in effect; `None` means the production default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevOverrides {
    pub data_dir: Option<PathBuf>,
    pub startup_name: Option<String>,
    pub install_dir: Option<PathBuf>,
    pub start_menu_dir: Option<PathBuf>,
}

impl DevOverrides {
    /// Whether no override is in effect.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Parses the file contents (see the module documentation). The last
    /// valid value of a key wins.
    pub fn parse(text: &str) -> Self {
        fn absolute(value: &str) -> Option<PathBuf> {
            let path = PathBuf::from(value);
            (!value.is_empty() && path.is_absolute()).then_some(path)
        }
        let mut out = Self::default();
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let slot = match key.trim().to_ascii_lowercase().as_str() {
                "data_dir" => &mut out.data_dir,
                "install_dir" => &mut out.install_dir,
                "start_menu_dir" => &mut out.start_menu_dir,
                "startup_name" => {
                    if is_test_name(value) {
                        out.startup_name = Some(value.to_owned());
                    }
                    continue;
                }
                _ => continue,
            };
            if let Some(path) = absolute(value) {
                *slot = Some(path);
            }
        }
        out
    }

    /// The overrides for the executable `exe`: the parsed [`FILE_NAME`] in
    /// its directory in debug builds, the defaults otherwise (or when the
    /// file is missing or unreadable).
    pub fn for_exe(exe: &Path) -> Self {
        if !cfg!(debug_assertions) {
            return Self::default();
        }
        std::fs::read_to_string(exe.with_file_name(FILE_NAME))
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }
}

/// `ESRTest_` followed by at least one of `[A-Za-z0-9_-]`.
fn is_test_name(name: &str) -> bool {
    name.strip_prefix(TEST_NAME_PREFIX).is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_keys() {
        let o = DevOverrides::parse(
            "\u{FEFF}# test\r\n\r\ndata_dir = C:\\Temp\\esr data\r\n STARTUP_NAME=ESRTest_12_startup\r\n",
        );
        assert_eq!(o.data_dir, Some(PathBuf::from(r"C:\Temp\esr data")));
        assert_eq!(o.startup_name.as_deref(), Some("ESRTest_12_startup"));
    }

    #[test]
    fn empty_or_unknown_input_is_the_default() {
        assert_eq!(DevOverrides::parse(""), DevOverrides::default());
        assert_eq!(DevOverrides::parse("other=1\nno equals sign\n; c"), DevOverrides::default());
    }

    #[test]
    fn rejects_relative_data_dirs() {
        for value in ["", "data", r"..\data", r"\data", "C:data"] {
            let o = DevOverrides::parse(&format!("data_dir={value}"));
            assert_eq!(o.data_dir, None, "{value:?}");
        }
        let o = DevOverrides::parse(r"data_dir=\\server\share\esr");
        assert_eq!(o.data_dir, Some(PathBuf::from(r"\\server\share\esr")));
    }

    /// The production registration (or any non-test name) can never be
    /// selected through the file.
    #[test]
    fn startup_name_must_be_a_test_name() {
        for value in [
            "ExplorerSessionRecall",
            "ExplorerSessionRestore",
            "ESRTest_",
            "esrtest_1",
            "ESRTest_1\\x",
            "ESRTest_1 x",
            r"ESRTest_..\ExplorerSessionRecall",
            "",
        ] {
            let o = DevOverrides::parse(&format!("startup_name={value}"));
            assert_eq!(o.startup_name, None, "{value:?}");
        }
        let o = DevOverrides::parse("startup_name=ESRTest_9-a_B");
        assert_eq!(o.startup_name.as_deref(), Some("ESRTest_9-a_B"));
    }

    #[test]
    fn parses_install_locations_and_rejects_relative_ones() {
        let o = DevOverrides::parse(
            "install_dir=C:\\T\\Programs\\ESR\nstart_menu_dir=C:\\T\\Start Menu\\Programs\n",
        );
        assert_eq!(o.install_dir, Some(PathBuf::from(r"C:\T\Programs\ESR")));
        assert_eq!(o.start_menu_dir, Some(PathBuf::from(r"C:\T\Start Menu\Programs")));
        assert!(!o.is_default());
        let o = DevOverrides::parse("install_dir=Programs\nstart_menu_dir=\\Start\n");
        assert_eq!(o, DevOverrides::default());
        assert!(o.is_default());
    }

    #[test]
    fn last_valid_value_wins() {
        let o = DevOverrides::parse("data_dir=C:\\a\ndata_dir=rel\ndata_dir=C:\\b\nstartup_name=ESRTest_1\nstartup_name=bad");
        assert_eq!(o.data_dir, Some(PathBuf::from(r"C:\b")));
        assert_eq!(o.startup_name.as_deref(), Some("ESRTest_1"));
    }

    #[test]
    fn for_exe_reads_the_file_next_to_the_executable() {
        let dir = std::env::temp_dir().join(format!("esr_dev_overrides_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("esr.exe");
        assert_eq!(DevOverrides::for_exe(&exe), DevOverrides::default(), "no file");
        std::fs::write(dir.join(FILE_NAME), "data_dir=C:\\x\nstartup_name=ESRTest_2").unwrap();
        let o = DevOverrides::for_exe(&exe);
        let _ = std::fs::remove_dir_all(&dir);
        if cfg!(debug_assertions) {
            assert_eq!(o.data_dir, Some(PathBuf::from(r"C:\x")));
            assert_eq!(o.startup_name.as_deref(), Some("ESRTest_2"));
        } else {
            assert_eq!(o, DevOverrides::default(), "release builds ignore the file");
        }
    }
}
