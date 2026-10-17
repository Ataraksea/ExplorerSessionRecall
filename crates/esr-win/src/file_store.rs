//! Win32 `FsOps` and `DurableFileStore` (atomic replace, protected DACL,
//! unreadable-file backup).
//!
//! # Data directory
//!
//! Every file lives in one data directory, normally
//! `%LOCALAPPDATA%\ExplorerSessionRestore` ([`default_data_dir`]). The path is
//! injected into [`DurableFileStore::open`] so tests never touch the real one.
//! [`ensure_data_dir`] gives the directory the protected DACL
//! [`data_dir_sddl`] = `D:P(A;OICI;FA;;;<UserSID>)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`
//! (Req 11.1, 13.6):
//!
//! * If the directory does not exist, it is created with `CreateDirectoryW`
//!   and that security descriptor, so it is never visible with a weaker DACL.
//! * If it already exists (an earlier run, or created by someone else), the
//!   same DACL is applied with `SetNamedSecurityInfoW(DACL_SECURITY_INFORMATION
//!   | PROTECTED_DACL_SECURITY_INFORMATION)`. This replaces any explicit ACEs,
//!   stops inheritance from `%LOCALAPPDATA%`, and propagates the inheritable
//!   ACEs to files already inside.
//!
//! Every ACE is `OICI`, so files created inside (the Session_File, its
//! backup, the temp files, settings, and the Activity_Log) inherit exactly
//! these three ACEs. `MoveFileExW` within the directory keeps the temp file's
//! inherited DACL, and `ReplaceFileW` merges the replaced file's (equally
//! inherited) DACL onto the replacement.
//!
//! # Atomic writes
//!
//! [`WinFs`] implements [`FsOps`] with `DeleteFileW`, `CreateFileW` +
//! `WriteFile`, `FlushFileBuffers`, `ReplaceFileW`
//! (`REPLACEFILE_IGNORE_MERGE_ERRORS`), and `MoveFileExW(MOVEFILE_REPLACE_EXISTING
//! | MOVEFILE_WRITE_THROUGH)`; [`atomic_replace`] sequences them (Req 3.9).
//! Leftover temp files are deleted when the store opens, before the
//! Session_File is read. Unreadable Session_File content is preserved as
//! `session.esr.unreadable` with the same algorithm (Req 11.7). Settings are
//! written the same way to `settings.ini` ([`SETTINGS_FILE`]).
//!
//! # Writer thread
//!
//! Session_File and backup writes run on one writer thread (design "Threads":
//! Writer) so slow disks never block the Engine. Jobs run in FIFO order, except
//! that urgent session writes run before every queued non-urgent job. Command
//! `seq` numbers increase, so a queued session write whose `seq` is lower than
//! a session write that has already been committed is skipped and reported as
//! `Ok(())` (the file already holds newer content). This keeps urgent-first
//! ordering from ever regressing the file. Each job's result is reported
//! through the [`CompletionCallback`] on the writer thread.
//! [`DurableFileStore::flush`] blocks until the queue is drained (including
//! the callback of the last job) or the timeout passes; it is used at
//! `WM_QUERYENDSESSION` (≤ 1.5 s). Dropping the store finishes the queued jobs
//! and joins the thread.
//!
//! The `esr_app::ports::FileStore` implementation that wraps this type and
//! posts `Event::WriteCompleted { seq, result }` through the `EventSink` is
//! written in task 14.6 (`esr-win` does not depend on `esr-app`).
//!
//! Requirements 3.9, 11.1, 11.7, 13.6.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use esr_core::engine::{IoErrorKind, SessionFileRead};
use esr_core::store::{
    AtomicFile, BACKUP_FILE, FsOps, SESSION_FILE, atomic_replace, cleanup_all_leftover_temps,
    cleanup_leftover_temp,
};
use windows::Win32::Foundation::{
    CloseHandle, E_UNEXPECTED, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DISK_FULL,
    ERROR_FILE_NOT_FOUND, ERROR_HANDLE_DISK_FULL, ERROR_LOCK_VIOLATION, ERROR_PATH_NOT_FOUND,
    ERROR_SHARING_VIOLATION, ERROR_SUCCESS, ERROR_UNABLE_TO_MOVE_REPLACEMENT,
    ERROR_UNABLE_TO_MOVE_REPLACEMENT_2, ERROR_WRITE_FAULT, GENERIC_WRITE, HANDLE, HLOCAL,
    LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateDirectoryW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_MODE, FlushFileBuffers, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_IGNORE_ACL_ERRORS,
    REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW, WriteFile,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};

/// Name of the data directory under `%LOCALAPPDATA%`.
pub const DATA_DIR_NAME: &str = "ExplorerSessionRestore";

/// `settings.ini`, written with [`atomic_replace`] like the Session_File.
pub const SETTINGS_FILE: AtomicFile = AtomicFile::new("settings.ini", "settings.ini.tmp");

/// Receives `(seq, result)` for every `write_session` and `preserve_backup`
/// job, on the writer thread.
pub type CompletionCallback = Box<dyn Fn(u64, Result<(), IoErrorKind>) + Send + 'static>;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Maps a Win32 error code to the file store's error kind.
pub fn map_win32_error(code: u32) -> IoErrorKind {
    match code {
        c if c == ERROR_FILE_NOT_FOUND.0 || c == ERROR_PATH_NOT_FOUND.0 => IoErrorKind::NotFound,
        c if c == ERROR_ACCESS_DENIED.0 => IoErrorKind::AccessDenied,
        c if c == ERROR_SHARING_VIOLATION.0 || c == ERROR_LOCK_VIOLATION.0 => {
            IoErrorKind::SharingViolation
        }
        c if c == ERROR_DISK_FULL.0 || c == ERROR_HANDLE_DISK_FULL.0 => IoErrorKind::DiskFull,
        c => IoErrorKind::Other(c),
    }
}

/// The Win32 code inside a `windows` error (`HRESULT_FROM_WIN32`), or the raw
/// HRESULT bits for errors that are not Win32 codes.
fn win32_code(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr }
}

fn kind_of(e: &windows::core::Error) -> IoErrorKind {
    map_win32_error(win32_code(e))
}

fn io_kind(e: &io::Error) -> IoErrorKind {
    match e.raw_os_error() {
        Some(code) => map_win32_error(code as u32),
        None => match e.kind() {
            io::ErrorKind::NotFound => IoErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => IoErrorKind::AccessDenied,
            _ => IoErrorKind::Other(0),
        },
    }
}

// ---------------------------------------------------------------------------
// Data directory and DACL
// ---------------------------------------------------------------------------

/// `%LOCALAPPDATA%\ExplorerSessionRestore` (`FOLDERID_LocalAppData`). The
/// directory is not created; see [`ensure_data_dir`].
pub fn default_data_dir() -> windows::core::Result<PathBuf> {
    // SAFETY: the returned string is copied and then freed with
    // CoTaskMemFree exactly once, as SHGetKnownFolderPath requires.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        let s = s.map_err(|_| windows::core::Error::from(E_UNEXPECTED))?;
        Ok(PathBuf::from(s).join(DATA_DIR_NAME))
    }
}

/// The current user's SID as a string (`S-1-5-21-…`).
///
/// Same token technique as `esr_app::instance::current_user_sid`, repeated
/// here because `esr-win` does not depend on `esr-app`.
pub fn current_user_sid() -> windows::core::Result<String> {
    // SAFETY: standard token query; buffers are sized from the first call and
    // the token handle and the SID string are released before returning.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let result = (|| {
            let mut len = 0u32;
            // Expected to fail with ERROR_INSUFFICIENT_BUFFER, filling `len`.
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            // u64 elements keep the buffer aligned for TOKEN_USER.
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                (buf.len() * 8) as u32,
                &mut len,
            )?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid_str)?;
            let s = sid_str.to_string();
            let _ = LocalFree(Some(HLOCAL(sid_str.0.cast())));
            s.map_err(|_| windows::core::Error::from(E_UNEXPECTED))
        })();
        let _ = CloseHandle(token);
        result
    }
}

/// The protected DACL of the data directory, as SDDL (Req 11.1, 13.6).
pub fn data_dir_sddl(user_sid: &str) -> String {
    format!("D:P(A;OICI;FA;;;{user_sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
}

/// A security descriptor from `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
/// freed with `LocalFree`.
struct OwnedSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl OwnedSecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self, IoErrorKind> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: valid null-terminated string; on success `sd` receives a
        // LocalAlloc'ed descriptor that `Drop` frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| kind_of(&e))?;
        Ok(Self(sd))
    }

    /// The DACL inside the descriptor (borrowed; lives as long as `self`).
    fn dacl(&self) -> Result<*const ACL, IoErrorKind> {
        let mut present = BOOL::default();
        let mut defaulted = BOOL::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid self-relative descriptor.
        unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
            .map_err(|e| kind_of(&e))?;
        if !present.as_bool() || dacl.is_null() {
            return Err(IoErrorKind::Other(E_UNEXPECTED.0 as u32));
        }
        Ok(dacl)
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
        // and freed exactly once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

/// Create `dir` with the protected DACL of [`data_dir_sddl`], or apply that
/// DACL if `dir` already exists (see the module documentation). Missing
/// parent directories are created normally (they inherit from their parent).
pub fn ensure_data_dir(dir: &Path) -> Result<(), IoErrorKind> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| io_kind(&e))?;
    }
    let sid = current_user_sid().map_err(|e| kind_of(&e))?;
    let sd = OwnedSecurityDescriptor::from_sddl(&data_dir_sddl(&sid))?;
    let path = HSTRING::from(dir);
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: valid path and security attributes that outlive the call.
    match unsafe { CreateDirectoryW(&path, Some(&sa)) } {
        Ok(()) => Ok(()),
        Err(e) if win32_code(&e) == ERROR_ALREADY_EXISTS.0 => {
            let dacl = sd.dacl()?;
            // SAFETY: valid path; `dacl` points into `sd`, alive for the call.
            let err = unsafe {
                SetNamedSecurityInfoW(
                    &path,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                )
            };
            if err == ERROR_SUCCESS { Ok(()) } else { Err(map_win32_error(err.0)) }
        }
        Err(e) => Err(kind_of(&e)),
    }
}

// ---------------------------------------------------------------------------
// FsOps
// ---------------------------------------------------------------------------

/// [`FsOps`] on real files in one directory. Names are relative to `dir`.
#[derive(Clone, Debug)]
pub struct WinFs {
    dir: PathBuf,
}

/// An open, written temp file; the handle is closed on drop.
#[derive(Debug)]
pub struct WinTemp(HANDLE);

impl Drop for WinTemp {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl WinFs {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> HSTRING {
        HSTRING::from(self.dir.join(name).as_path())
    }

    fn exists(&self, name: &str) -> bool {
        // SAFETY: valid null-terminated path.
        unsafe { GetFileAttributesW(&self.path(name)) != INVALID_FILE_ATTRIBUTES }
    }

    fn move_file(&self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
        // SAFETY: valid null-terminated paths.
        unsafe {
            MoveFileExW(
                &self.path(temp),
                &self.path(target),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| kind_of(&e))
    }

    /// Read `name` in full; `Ok(None)` when it does not exist.
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, IoErrorKind> {
        match std::fs::read(self.dir.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) => match io_kind(&e) {
                IoErrorKind::NotFound => Ok(None),
                kind => Err(kind),
            },
        }
    }
}

impl FsOps for WinFs {
    type Temp = WinTemp;

    fn remove_if_exists(&mut self, name: &str) -> Result<(), IoErrorKind> {
        // SAFETY: valid null-terminated path.
        match unsafe { DeleteFileW(&self.path(name)) } {
            Ok(()) => Ok(()),
            Err(e) => match kind_of(&e) {
                IoErrorKind::NotFound => Ok(()),
                kind => Err(kind),
            },
        }
    }

    fn write_temp(&mut self, name: &str, content: &[u8]) -> Result<WinTemp, IoErrorKind> {
        // SAFETY: valid path; the handle is owned by `WinTemp`, which closes
        // it on every path out of this function except success.
        let handle = unsafe {
            CreateFileW(
                &self.path(name),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| kind_of(&e))?;
        let temp = WinTemp(handle);
        let mut rest = content;
        while !rest.is_empty() {
            let chunk = &rest[..rest.len().min(u32::MAX as usize)];
            let mut written = 0u32;
            // SAFETY: synchronous write from a valid buffer to an open handle.
            unsafe { WriteFile(temp.0, Some(chunk), Some(&mut written), None) }
                .map_err(|e| kind_of(&e))?;
            if written == 0 {
                return Err(map_win32_error(ERROR_WRITE_FAULT.0));
            }
            rest = &rest[written as usize..];
        }
        Ok(temp)
    }

    fn flush(&mut self, temp: WinTemp) -> Result<(), IoErrorKind> {
        // SAFETY: `temp.0` is an open handle; it is closed when `temp` drops.
        let result = unsafe { FlushFileBuffers(temp.0) }.map_err(|e| kind_of(&e));
        drop(temp);
        result
    }

    fn replace_existing(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
        // SAFETY: valid null-terminated paths; no backup name.
        let result = unsafe {
            ReplaceFileW(
                &self.path(target),
                &self.path(temp),
                PCWSTR::null(),
                REPLACEFILE_IGNORE_MERGE_ERRORS | REPLACEFILE_IGNORE_ACL_ERRORS,
                None,
                None,
            )
        };
        let Err(e) = result else { return Ok(()) };
        let code = win32_code(&e);
        if code == ERROR_FILE_NOT_FOUND.0 || code == ERROR_PATH_NOT_FOUND.0 {
            // The trait contract: NotFound only when the target is absent
            // (nothing changed; the caller falls back to `move_into_place`).
            // If the target exists, the replacement was missing instead.
            return if self.exists(target) {
                Err(IoErrorKind::Other(code))
            } else {
                Err(IoErrorKind::NotFound)
            };
        }
        if code == ERROR_UNABLE_TO_MOVE_REPLACEMENT.0 || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT_2.0
        {
            // Without a backup name, these leave the target removed (or
            // renamed away) and the complete, flushed replacement under the
            // temp name. Finish the commit with a rename so the target holds
            // the new content. If that fails too, report the original error.
            return self.move_file(temp, target).map_err(|_| IoErrorKind::Other(code));
        }
        // Every other error (including ERROR_UNABLE_TO_REMOVE_REPLACED)
        // leaves both files under their original names: target unchanged.
        Err(map_win32_error(code))
    }

    fn move_into_place(&mut self, temp: &str, target: &str) -> Result<(), IoErrorKind> {
        self.move_file(temp, target)
    }
}

// ---------------------------------------------------------------------------
// DurableFileStore
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum JobKind {
    Session,
    Backup,
}

#[derive(Debug)]
struct Job {
    seq: u64,
    kind: JobKind,
    content: Vec<u8>,
}

#[derive(Default)]
struct Queue {
    urgent: VecDeque<Job>,
    normal: VecDeque<Job>,
    /// A job has been taken and its callback has not returned yet.
    busy: bool,
    shutdown: bool,
}

impl Queue {
    fn idle(&self) -> bool {
        self.urgent.is_empty() && self.normal.is_empty() && !self.busy
    }

    fn pop(&mut self) -> Option<Job> {
        self.urgent.pop_front().or_else(|| self.normal.pop_front())
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a job is queued or shutdown is requested.
    work: Condvar,
    /// Signalled when the queue becomes idle.
    idle: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // A panicking callback must not make the store unusable.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Session_File, backup copy, and settings storage in one data directory.
pub struct DurableFileStore {
    fs: WinFs,
    setup_error: Option<IoErrorKind>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl DurableFileStore {
    /// Open the store in `dir`: create it (or re-apply) with the protected
    /// DACL, delete leftover temp files, and start the writer thread.
    ///
    /// Never fails: a setup failure is reported by
    /// [`setup_error`](Self::setup_error) (for the Activity_Log), and later
    /// operations then fail with their own errors.
    pub fn open(dir: impl Into<PathBuf>, on_complete: CompletionCallback) -> Self {
        let mut fs = WinFs::new(dir);
        let mut setup_error = ensure_data_dir(fs.dir()).err();
        let cleanup = cleanup_all_leftover_temps(&mut fs)
            .and(cleanup_leftover_temp(&mut fs, &SETTINGS_FILE));
        if let Err(e) = cleanup {
            setup_error.get_or_insert(e);
        }
        let shared = Arc::new(Shared::default());
        let worker = {
            let shared = Arc::clone(&shared);
            let fs = fs.clone();
            thread::Builder::new()
                .name("esr-writer".into())
                .spawn(move || writer_loop(&shared, fs, on_complete))
                .ok()
        };
        if worker.is_none() {
            setup_error.get_or_insert(IoErrorKind::Other(E_UNEXPECTED.0 as u32));
        }
        Self { fs, setup_error, shared, worker }
    }

    /// The data directory.
    pub fn dir(&self) -> &Path {
        self.fs.dir()
    }

    /// The first error from creating the directory, applying its DACL,
    /// deleting leftover temp files, or starting the writer thread.
    pub fn setup_error(&self) -> Option<IoErrorKind> {
        self.setup_error
    }

    /// Read the Session_File (leftover temp files were deleted by `open`).
    pub fn read_session(&self) -> SessionFileRead {
        match self.fs.read(SESSION_FILE.target) {
            Ok(Some(bytes)) => SessionFileRead::Content(bytes),
            Ok(None) => SessionFileRead::Absent,
            Err(kind) => SessionFileRead::Unreadable(kind),
        }
    }

    /// Queue an atomic replace of the Session_File. `urgent` writes run
    /// before every queued non-urgent job.
    pub fn write_session(&self, seq: u64, text: &str, urgent: bool) {
        self.enqueue(Job { seq, kind: JobKind::Session, content: text.as_bytes().to_vec() }, urgent);
    }

    /// Queue replacing the backup copy `session.esr.unreadable` with `content`.
    pub fn preserve_backup(&self, seq: u64, content: &[u8]) {
        self.enqueue(Job { seq, kind: JobKind::Backup, content: content.to_vec() }, false);
    }

    /// Block until every queued job has completed (its callback included) or
    /// `timeout_ms` passed. Returns `true` when the queue drained.
    pub fn flush(&self, timeout_ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        let mut queue = self.shared.lock();
        while !queue.idle() {
            let now = Instant::now();
            if now >= deadline || self.worker.is_none() {
                return false;
            }
            queue = self
                .shared
                .idle
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Settings text, or `None` when `settings.ini` is absent. Invalid UTF-8
    /// is replaced (the settings parser falls back to defaults per value).
    pub fn read_settings(&self) -> Result<Option<String>, IoErrorKind> {
        Ok(self
            .fs
            .read(SETTINGS_FILE.target)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Atomically replace `settings.ini` (synchronously, on the caller's thread).
    pub fn write_settings(&self, text: &str) -> Result<(), IoErrorKind> {
        let mut fs = self.fs.clone();
        ensure_dir_present(&fs);
        atomic_replace(&mut fs, &SETTINGS_FILE, text.as_bytes()).map_err(|e| e.kind)
    }

    fn enqueue(&self, job: Job, urgent: bool) {
        let mut queue = self.shared.lock();
        if urgent {
            queue.urgent.push_back(job);
        } else {
            queue.normal.push_back(job);
        }
        self.shared.work.notify_one();
    }
}

impl Drop for DurableFileStore {
    /// Finish the queued jobs, then stop and join the writer thread.
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.work.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Re-create the directory (with its DACL) if it disappeared while running.
fn ensure_dir_present(fs: &WinFs) {
    if !fs.dir().is_dir() {
        let _ = ensure_data_dir(fs.dir());
    }
}

fn writer_loop(shared: &Shared, mut fs: WinFs, on_complete: CompletionCallback) {
    // Highest seq of a committed session write (see the module docs).
    let mut committed_session_seq: Option<u64> = None;
    loop {
        let job = {
            let mut queue = shared.lock();
            loop {
                if let Some(job) = queue.pop() {
                    queue.busy = true;
                    break job;
                }
                if queue.shutdown {
                    return;
                }
                queue = shared.work.wait(queue).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let result = match job.kind {
            JobKind::Session if committed_session_seq.is_some_and(|c| job.seq < c) => Ok(()),
            JobKind::Session => {
                ensure_dir_present(&fs);
                let r = atomic_replace(&mut fs, &SESSION_FILE, &job.content).map_err(|e| e.kind);
                if r.is_ok() {
                    committed_session_seq = Some(job.seq);
                }
                r
            }
            JobKind::Backup => {
                ensure_dir_present(&fs);
                atomic_replace(&mut fs, &BACKUP_FILE, &job.content).map_err(|e| e.kind)
            }
        };
        // A panicking callback must not leave `busy` set forever.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_complete(job.seq, result)));
        let mut queue = shared.lock();
        queue.busy = false;
        if queue.idle() {
            shared.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    };

    /// `%TEMP%\esr_test_filestore_<pid>\<name>`, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("esr_test_filestore_{}", std::process::id()))
                .join(name);
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        /// The store's data directory (not created yet).
        fn data(&self) -> PathBuf {
            self.0.join("data")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            // Succeeds only for the last test to finish.
            if let Some(parent) = self.0.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    type Log = Arc<Mutex<Vec<(u64, Result<(), IoErrorKind>)>>>;

    fn recorder() -> (Log, CompletionCallback) {
        let log: Log = Arc::default();
        let l = Arc::clone(&log);
        (log, Box::new(move |seq, r| l.lock().unwrap().push((seq, r))))
    }

    fn open(dir: &TestDir) -> (DurableFileStore, Log) {
        let (log, cb) = recorder();
        let store = DurableFileStore::open(dir.data(), cb);
        assert_eq!(store.setup_error(), None);
        (store, log)
    }

    /// The DACL of `path` as SDDL (`D:<flags>(ace)(ace)…`).
    fn dacl_sddl(path: &Path) -> String {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: valid path; `sd` is freed with LocalFree below.
        let err = unsafe {
            GetNamedSecurityInfoW(
                &HSTRING::from(path),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            )
        };
        assert_eq!(err, ERROR_SUCCESS);
        let mut s = PWSTR::null();
        // SAFETY: `sd` is valid; `s` is freed with LocalFree.
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                None,
            )
            .unwrap();
            let out = s.to_string().unwrap();
            let _ = LocalFree(Some(HLOCAL(s.0.cast())));
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            out
        }
    }

    /// Splits `D:<flags>(a)(b)` into the flags and the ACE strings.
    fn split_dacl(sddl: &str) -> (String, Vec<String>) {
        let body = sddl.strip_prefix("D:").expect("DACL SDDL");
        let open = body.find('(').unwrap_or(body.len());
        let aces = body[open..]
            .split(')')
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_start_matches('(').to_string())
            .collect();
        (body[..open].to_string(), aces)
    }

    fn assert_protected_three_aces(dir: &Path) {
        let sid = current_user_sid().unwrap();
        let (flags, aces) = split_dacl(&dacl_sddl(dir));
        assert!(flags.contains('P'), "DACL must be protected: {flags}");
        assert_eq!(
            aces,
            vec![
                format!("A;OICI;FA;;;{sid}"),
                "A;OICI;FA;;;SY".to_string(),
                "A;OICI;FA;;;BA".to_string(),
            ]
        );
    }

    fn file(dir: &TestDir, name: &str) -> PathBuf {
        dir.data().join(name)
    }

    #[test]
    fn maps_win32_errors() {
        assert_eq!(map_win32_error(2), IoErrorKind::NotFound);
        assert_eq!(map_win32_error(3), IoErrorKind::NotFound);
        assert_eq!(map_win32_error(5), IoErrorKind::AccessDenied);
        assert_eq!(map_win32_error(32), IoErrorKind::SharingViolation);
        assert_eq!(map_win32_error(112), IoErrorKind::DiskFull);
        assert_eq!(map_win32_error(1175), IoErrorKind::Other(1175));
    }

    #[test]
    fn sddl_has_the_three_aces() {
        assert_eq!(
            data_dir_sddl("S-1-5-21-1"),
            "D:P(A;OICI;FA;;;S-1-5-21-1)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        );
    }

    #[test]
    fn default_data_dir_is_under_local_app_data() {
        let dir = default_data_dir().unwrap();
        assert!(dir.ends_with(DATA_DIR_NAME), "{}", dir.display());
        assert!(dir.parent().unwrap().is_dir());
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = TestDir::new("round_trip");
        let (store, log) = open(&dir);
        assert_eq!(store.read_session(), SessionFileRead::Absent);
        let text = "esr 1\nwindow C:\\Users\\Ünïcødé\n";
        store.write_session(1, text, false);
        assert!(store.flush(5000));
        assert_eq!(store.read_session(), SessionFileRead::Content(text.as_bytes().to_vec()));
        assert_eq!(*log.lock().unwrap(), vec![(1, Ok(()))]);
        assert!(!file(&dir, "session.esr.tmp").exists());
    }

    #[test]
    fn replaces_an_existing_file() {
        let dir = TestDir::new("replace");
        let (store, log) = open(&dir);
        store.write_session(1, "old", false);
        assert!(store.flush(5000));
        store.write_session(2, "new", false);
        assert!(store.flush(5000));
        assert_eq!(std::fs::read(file(&dir, "session.esr")).unwrap(), b"new");
        assert!(!file(&dir, "session.esr.tmp").exists());
        assert_eq!(*log.lock().unwrap(), vec![(1, Ok(())), (2, Ok(()))]);
    }

    #[test]
    fn first_write_without_target_uses_the_move_path() {
        let dir = TestDir::new("move_path");
        ensure_data_dir(&dir.data()).unwrap();
        let mut fs = WinFs::new(dir.data());
        let temp = fs.write_temp("session.esr.tmp", b"content").unwrap();
        fs.flush(temp).unwrap();
        // ReplaceFileW on an absent target: NotFound, nothing changed.
        assert_eq!(
            fs.replace_existing("session.esr.tmp", "session.esr"),
            Err(IoErrorKind::NotFound)
        );
        assert!(file(&dir, "session.esr.tmp").exists());
        assert!(!file(&dir, "session.esr").exists());
        // The full algorithm falls back to MoveFileExW.
        atomic_replace(&mut fs, &SESSION_FILE, b"first").unwrap();
        assert_eq!(std::fs::read(file(&dir, "session.esr")).unwrap(), b"first");
        assert!(!file(&dir, "session.esr.tmp").exists());
        // Replacing now succeeds through ReplaceFileW.
        let temp = fs.write_temp("session.esr.tmp", b"second").unwrap();
        fs.flush(temp).unwrap();
        fs.replace_existing("session.esr.tmp", "session.esr").unwrap();
        assert_eq!(std::fs::read(file(&dir, "session.esr")).unwrap(), b"second");
        assert!(!file(&dir, "session.esr.tmp").exists());
    }

    #[test]
    fn leftover_temps_are_removed_at_open() {
        let dir = TestDir::new("leftover");
        std::fs::create_dir_all(dir.data()).unwrap();
        for name in ["session.esr.tmp", "session.esr.unreadable.tmp", "settings.ini.tmp"] {
            std::fs::write(file(&dir, name), b"partial").unwrap();
        }
        std::fs::write(file(&dir, "session.esr"), b"complete").unwrap();
        let (store, _log) = open(&dir);
        for name in ["session.esr.tmp", "session.esr.unreadable.tmp", "settings.ini.tmp"] {
            assert!(!file(&dir, name).exists(), "{name}");
        }
        assert_eq!(store.read_session(), SessionFileRead::Content(b"complete".to_vec()));
    }

    #[test]
    fn backup_is_written_and_replaced_without_touching_the_session_file() {
        let dir = TestDir::new("backup");
        let (store, log) = open(&dir);
        std::fs::write(file(&dir, "session.esr"), b"\xffgarbage").unwrap();
        store.preserve_backup(3, b"older backup");
        store.preserve_backup(4, b"\xffgarbage");
        assert!(store.flush(5000));
        assert_eq!(std::fs::read(file(&dir, "session.esr.unreadable")).unwrap(), b"\xffgarbage");
        assert_eq!(std::fs::read(file(&dir, "session.esr")).unwrap(), b"\xffgarbage");
        assert!(!file(&dir, "session.esr.unreadable.tmp").exists());
        assert_eq!(*log.lock().unwrap(), vec![(3, Ok(())), (4, Ok(()))]);
    }

    #[test]
    fn read_reports_absent_content_and_unreadable() {
        let dir = TestDir::new("read_kinds");
        let (store, _log) = open(&dir);
        assert_eq!(store.read_session(), SessionFileRead::Absent);
        std::fs::write(file(&dir, "session.esr"), b"x").unwrap();
        assert_eq!(store.read_session(), SessionFileRead::Content(b"x".to_vec()));
        std::fs::remove_file(file(&dir, "session.esr")).unwrap();
        // A directory in place of the file cannot be read.
        std::fs::create_dir(file(&dir, "session.esr")).unwrap();
        assert!(matches!(store.read_session(), SessionFileRead::Unreadable(_)));
    }

    #[test]
    fn settings_round_trip() {
        let dir = TestDir::new("settings");
        let (store, _log) = open(&dir);
        assert_eq!(store.read_settings(), Ok(None));
        store.write_settings("restore=1\n").unwrap();
        assert_eq!(store.read_settings(), Ok(Some("restore=1\n".to_string())));
        store.write_settings("restore=0\nstartup=1\n").unwrap();
        assert_eq!(store.read_settings(), Ok(Some("restore=0\nstartup=1\n".to_string())));
        assert!(!file(&dir, "settings.ini.tmp").exists());
    }

    #[test]
    fn flush_waits_for_every_queued_write() {
        let dir = TestDir::new("flush");
        let (store, log) = open(&dir);
        assert!(store.flush(0), "an empty queue is drained");
        let big = "x".repeat(256 * 1024);
        for seq in 1..=20 {
            store.write_session(seq, &format!("{seq}{big}"), false);
        }
        assert!(store.flush(30_000));
        let log = log.lock().unwrap();
        assert_eq!(log.iter().map(|(s, _)| *s).collect::<Vec<_>>(), (1..=20).collect::<Vec<_>>());
        assert!(log.iter().all(|(_, r)| r.is_ok()));
        let content = std::fs::read(file(&dir, "session.esr")).unwrap();
        assert!(content.starts_with(b"20x"));
    }

    #[test]
    fn flush_times_out_while_a_write_is_blocked() {
        let dir = TestDir::new("flush_timeout");
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let store = DurableFileStore::open(
            dir.data(),
            Box::new(move |_, _| {
                let _ = rx.lock().unwrap().recv();
            }),
        );
        store.write_session(1, "a", false);
        assert!(!store.flush(100));
        tx.send(()).unwrap();
        assert!(store.flush(5000));
    }

    #[test]
    fn callbacks_carry_seq_and_urgent_writes_run_first() {
        let dir = TestDir::new("urgent");
        let (log, record) = recorder();
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let store = DurableFileStore::open(
            dir.data(),
            Box::new(move |seq, r| {
                record(seq, r);
                if seq == 1 {
                    // Hold the writer so the next jobs queue up behind it.
                    let _ = rx.lock().unwrap().recv();
                }
            }),
        );
        store.write_session(1, "one", false);
        // Wait until job 1 has been taken.
        while log.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(5));
        }
        store.write_session(2, "two", false);
        store.preserve_backup(3, b"backup");
        store.write_session(4, "four", true);
        tx.send(()).unwrap();
        assert!(store.flush(5000));
        let seqs: Vec<u64> = log.lock().unwrap().iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 4, 2, 3]);
        assert!(log.lock().unwrap().iter().all(|(_, r)| r.is_ok()));
        // Write 2 was superseded by urgent write 4 and must not regress the file.
        assert_eq!(std::fs::read(file(&dir, "session.esr")).unwrap(), b"four");
        assert_eq!(std::fs::read(file(&dir, "session.esr.unreadable")).unwrap(), b"backup");
    }

    #[test]
    fn drop_finishes_queued_writes() {
        let dir = TestDir::new("drop");
        let (store, log) = open(&dir);
        for seq in 1..=5 {
            store.write_session(seq, &format!("v{seq}"), false);
        }
        drop(store);
        assert_eq!(log.lock().unwrap().len(), 5);
        assert_eq!(std::fs::read(file(&dir, "session.esr")).unwrap(), b"v5");
    }

    #[test]
    fn new_directory_gets_the_protected_dacl() {
        let dir = TestDir::new("dacl_new");
        let (store, _log) = open(&dir);
        assert_protected_three_aces(&dir.data());
        // Files inside inherit exactly these trustees.
        store.write_session(1, "x", false);
        assert!(store.flush(5000));
        let sid = current_user_sid().unwrap();
        let (_, aces) = split_dacl(&dacl_sddl(&file(&dir, "session.esr")));
        assert!(!aces.is_empty());
        for ace in &aces {
            assert!(
                [sid.as_str(), "SY", "BA"].iter().any(|t| ace.ends_with(&format!(";;;{t}"))),
                "{ace}"
            );
        }
    }

    #[test]
    fn existing_directory_gets_the_protected_dacl() {
        let dir = TestDir::new("dacl_existing");
        std::fs::create_dir_all(dir.data()).unwrap();
        let (flags, _) = split_dacl(&dacl_sddl(&dir.data()));
        assert!(!flags.contains('P'), "a plain directory inherits: {flags}");
        let (_store, _log) = open(&dir);
        assert_protected_three_aces(&dir.data());
    }
}
