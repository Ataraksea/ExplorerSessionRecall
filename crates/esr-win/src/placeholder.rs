//! Placeholder files of deferred tabs (Requirement 5.13; design.md, "Restore
//! pipeline" → "Deferred tabs").
//!
//! The core names a deferred tab's placeholder
//! `<dir>\<n>\<name>.library-ms` (`esr_core::plan::defer::placeholder_location`).
//! Before such a Location is opened or navigated to
//! ([`crate::navigator::location_pidl`]), [`ensure`] writes the file: an
//! empty library description, which Explorer shows as a library named
//! `<name>` ("… is empty") without viewing any folder, so it adds no entry to
//! Explorer's sync-status cache (task 16.3: 133 such placeholders in one
//! window did not crash Explorer 22621; 134 distinct real folders did).
//! Other `.library-ms` files in `<dir>\<n>` (an earlier restoration's
//! placeholder with another name) are removed, so the directory never holds
//! more than one file per deferred tab of the largest restoration. A
//! restoration starts only while no Explorer window is open, so no tab still
//! shows a file that is replaced.
//!
//! Only paths directly two levels under the directory set with [`set_dir`]
//! are touched.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use esr_core::model::{LocationId, LocationKind, ShellKind, WideString};
use esr_core::plan::defer::PLACEHOLDER_EXTENSION;

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The content of a placeholder: a library without folders.
pub const PLACEHOLDER_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n\
<libraryDescription xmlns=\"http://schemas.microsoft.com/windows/2009/library\">\r\n\
<isLibraryPinned>false</isLibraryPinned>\r\n\
<searchConnectorDescriptionList/>\r\n\
</libraryDescription>\r\n";

/// Sets (or clears) the placeholder directory, `<data dir>\deferred`.
pub fn set_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// The placeholder directory, if set.
pub fn dir() -> Option<PathBuf> {
    DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The placeholder directory as the core's `Environment::placeholder_dir`.
pub fn dir_wide() -> Option<WideString> {
    dir().map(|d| WideString::new(d.as_os_str().encode_wide().collect()))
}

/// The placeholder file `location` names, if it is one: a library whose
/// parsing name is `<dir>\<n>\<name>.library-ms` for the configured `dir`.
pub fn placeholder_path(location: &LocationId) -> Option<PathBuf> {
    if location.kind != LocationKind::Shell(ShellKind::Library) {
        return None;
    }
    let dir = dir()?;
    let path = PathBuf::from(OsString::from_wide(location.parsing_name.units()));
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if !name.ends_with(PLACEHOLDER_EXTENSION) {
        return None;
    }
    let numbered = path.parent()?;
    let index = numbered.file_name()?.to_str()?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    same_path(numbered.parent()?, &dir).then_some(path)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

/// Writes the placeholder file `location` names, if it is one (see the
/// module docs). Errors are returned for the caller to report; the
/// navigation then fails because the file is missing.
pub fn ensure(location: &LocationId) -> std::io::Result<()> {
    let Some(path) = placeholder_path(location) else { return Ok(()) };
    let parent = path.parent().expect("checked by placeholder_path");
    std::fs::create_dir_all(parent)?;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let p = entry.path();
        let stale = p != path
            && p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(PLACEHOLDER_EXTENSION));
        if stale {
            let _ = std::fs::remove_file(&p);
        }
    }
    if !path.exists() {
        std::fs::write(&path, PLACEHOLDER_XML)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use esr_core::plan::defer::placeholder_location;

    fn fs(path: &str) -> LocationId {
        LocationId {
            case_sensitive: false,
            kind: LocationKind::FileSystem(esr_core::model::Volume::Fixed),
            parsing_name: path.into(),
            display_name: path.rsplit('\\').next().unwrap().into(),
            pidl: vec![],
            search: None,
        }
    }

    // One test, as the directory is process-wide.
    #[test]
    fn writes_only_numbered_placeholders_under_the_directory() {
        let root = std::env::temp_dir().join(format!("esr_placeholder_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        set_dir(Some(root.clone()));
        let wide = dir_wide().unwrap();
        let p = placeholder_location(&wide, 3, &fs(r"D:\Work\Projekt Ü"));
        assert_eq!(placeholder_path(&p), Some(root.join("3").join("Projekt Ü.library-ms")));
        ensure(&p).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("3").join("Projekt Ü.library-ms")).unwrap(), PLACEHOLDER_XML);

        // Another name in the same slot replaces the earlier file.
        let q = placeholder_location(&wide, 3, &fs(r"D:\Other"));
        ensure(&q).unwrap();
        let names: Vec<_> = std::fs::read_dir(root.join("3")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, [OsString::from("Other.library-ms")]);

        // Anything else is left alone.
        let mut outside = p.clone();
        outside.parsing_name = r"C:\elsewhere\1\x.library-ms".into();
        assert_eq!(placeholder_path(&outside), None);
        let mut unnumbered = p.clone();
        unnumbered.parsing_name = format!("{}\\x\\y.library-ms", root.display()).as_str().into();
        assert_eq!(placeholder_path(&unnumbered), None);
        assert_eq!(placeholder_path(&fs(&format!("{}\\1\\y.library-ms", root.display()))), None, "not a library");
        ensure(&fs(r"C:\Windows")).unwrap();

        set_dir(None);
        assert_eq!(placeholder_path(&p), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
