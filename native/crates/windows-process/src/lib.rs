//! Safe ownership of one Windows managed process tree.
#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    FILETIME, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::*;
use windows::core::{PCWSTR, PWSTR};

#[cfg(test)]
thread_local! {
    static REFUSE_ASSIGNMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CREATION_REACHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REFUSE_RESUME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SUSPENDED_PROCESS: std::cell::RefCell<Option<OwnedHandle>> = const { std::cell::RefCell::new(None) };
    static PAUSE_AFTER_CREATE: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn error(error: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(error.code().0 & 0xffff)
}
fn handle(owned: &OwnedHandle) -> HANDLE {
    HANDLE(owned.as_raw_handle())
}

/// PID and exact Windows creation time in 100-nanosecond FILETIME units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub creation_time: u64,
}

/// Retained process handle. PID reuse cannot retarget this handle.
#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
    identity: Identity,
}

impl Process {
    pub fn identity(&self) -> Identity {
        self.identity
    }

    pub fn wait(&self) -> io::Result<u32> {
        // SAFETY: the retained process handle remains valid until this call returns.
        if unsafe { WaitForSingleObject(handle(&self.handle), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0;
        // SAFETY: code is writable and the retained handle is a process handle.
        unsafe { GetExitCodeProcess(handle(&self.handle), &mut code) }.map_err(error)?;
        Ok(code)
    }
}

/// Private, non-breakaway job. Drop kills any remaining descendants.
#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
}

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: no name or inheritable security attributes; successful handle is owned once.
        let raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(error)?;
        // SAFETY: CreateJobObjectW returned a fresh valid handle.
        let job = Self {
            handle: unsafe { OwnedHandle::from_raw_handle(raw.0) },
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the information buffer has the exact class layout and remains live during the call.
        unsafe {
            SetInformationJobObject(
                handle(&job.handle),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of_val(&limits) as u32,
            )
        }
        .map_err(error)?;
        Ok(job)
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: the writable buffer matches the requested class and the job is retained.
        unsafe {
            QueryInformationJobObject(
                Some(handle(&self.handle)),
                JobObjectBasicAccountingInformation,
                &mut accounting as *mut _ as *mut _,
                size_of_val(&accounting) as u32,
                None,
            )
        }
        .map_err(error)?;
        Ok(accounting.ActiveProcesses == 0)
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: only this launch's private retained job is targeted.
        unsafe { TerminateJobObject(handle(&self.handle), 1) }.map_err(error)
    }

    pub fn wait_empty(&self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_empty()? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // Covers startup failures after resume, including reader/waiter thread creation.
        // Kill-on-close remains the kernel fallback if either operation fails.
        let _ = self.terminate();
        let _ = self.wait_empty(Duration::from_secs(5));
    }
}

/// Parent pipe ends and ownership handles from a successfully resumed launch.
#[derive(Debug)]
pub struct Spawned {
    pub process: Process,
    pub job: Job,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}

fn pipe(parent_reads: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    // SAFETY: both output handles and security attributes live through this call.
    unsafe { CreatePipe(&mut read, &mut write, Some(&security), 0) }.map_err(error)?;
    // SAFETY: successful CreatePipe returned two distinct fresh handles.
    let (read, write) = unsafe {
        (
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        )
    };
    let (parent, child) = if parent_reads {
        (read, write)
    } else {
        (write, read)
    };
    // SAFETY: parent is a valid pipe handle; clearing inheritance changes no ownership.
    unsafe { SetHandleInformation(handle(&parent), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .map_err(error)?;
    Ok((parent, child))
}

struct Attributes {
    storage: Vec<usize>,
    initialized: bool,
}
impl Attributes {
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.storage.as_mut_ptr().cast())
    }
    fn new(handles: &[HANDLE], jobs: &[HANDLE]) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: sizing query intentionally passes no list and a valid size pointer.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 2, None, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut list = Self {
            storage: vec![0; bytes.div_ceil(size_of::<usize>())],
            initialized: false,
        };
        // SAFETY: storage is pointer-aligned and has the size returned by the sizing query.
        unsafe { InitializeProcThreadAttributeList(Some(list.pointer()), 2, None, &mut bytes) }
            .map_err(error)?;
        list.initialized = true;
        // SAFETY: handles remains live through CreateProcessW; each handle is inheritable.
        unsafe {
            UpdateProcThreadAttribute(
                list.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                size_of_val(handles),
                None,
                None,
            )
        }
        .map_err(error)?;
        // SAFETY: the job list and its retained non-inheritable job handles remain
        // live until the attribute list is destroyed after CreateProcessW.
        unsafe {
            UpdateProcThreadAttribute(
                list.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                size_of_val(jobs),
                None,
                None,
            )
        }
        .map_err(error)?;
        Ok(list)
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: storage contains the initialized attribute list and has not moved allocation.
        if self.initialized {
            unsafe { DeleteProcThreadAttributeList(self.pointer()) };
        }
    }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "embedded NUL"));
    }
    value.push(0);
    Ok(value)
}

// The CommandLineToArgvW/CRT quoting rule, including trailing backslashes.
fn quote(value: &OsStr, output: &mut Vec<u16>) -> io::Result<()> {
    let value = wide(value)?;
    output.push(b'"' as u16);
    let mut slashes = 0;
    for &unit in &value[..value.len() - 1] {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        slashes = 0;
        output.push(unit);
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
    Ok(())
}

/// Launches without a console, atomically assigned to its private job at creation.
/// The primary thread remains suspended until process identity has been captured.
/// `environment` is the complete environment, with no implicit inheritance.
pub fn spawn(
    program: &Path,
    args: &[OsString],
    environment: &[(OsString, OsString)],
) -> io::Result<Spawned> {
    let application = wide(program.as_os_str())?;
    let mut command = Vec::new();
    quote(program.as_os_str(), &mut command)?;
    for arg in args {
        command.push(b' ' as u16);
        quote(arg, &mut command)?;
    }
    command.push(0);
    // Reuse std's Windows ordinal, case-insensitive environment key handling.
    // Last assignment wins even when its spelling differs from an inherited key.
    let mut environment_command = std::process::Command::new(program);
    environment_command
        .env_clear()
        .envs(environment.iter().map(|(key, value)| (key, value)));
    let mut block = Vec::new();
    for (key, value) in environment_command.get_envs() {
        let Some(value) = value else {
            continue;
        };
        // Windows includes hidden drive-current-directory entries such as =C:.
        let key_units: Vec<_> = key.encode_wide().collect();
        if key_units.is_empty() || key_units[1..].contains(&(b'=' as u16)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid environment name",
            ));
        }
        let mut entry = key.to_os_string();
        entry.push("=");
        entry.push(value);
        block.extend(wide(&entry)?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    let job = Job::new()?;
    let (stdin, child_stdin) = pipe(false)?;
    let (stdout, child_stdout) = pipe(true)?;
    let (stderr, child_stderr) = pipe(true)?;
    let inherited = [
        handle(&child_stdin),
        handle(&child_stdout),
        handle(&child_stderr),
    ];
    let jobs = [handle(&job.handle)];
    #[cfg(test)]
    let jobs = if REFUSE_ASSIGNMENT.get() {
        [HANDLE::default()]
    } else {
        jobs
    };
    let mut attributes = Attributes::new(&inherited, &jobs)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.pointer();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all UTF-16 buffers, inherited handles and attribute storage remain live;
    // STARTUPINFOEXW embeds STARTUPINFOW first and the extended flag selects its layout.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            Some(block.as_ptr().cast()),
            PCWSTR::null(),
            &startup.StartupInfo,
            &mut info,
        )
    }
    .map_err(error)?;
    #[cfg(test)]
    CREATION_REACHED.set(true);
    #[cfg(test)]
    PAUSE_AFTER_CREATE.with_borrow(|directory| {
        if let Some(directory) = directory {
            std::fs::write(
                directory.join("created.partial"),
                info.dwProcessId.to_string(),
            )
            .expect("creation checkpoint");
            std::fs::rename(
                directory.join("created.partial"),
                directory.join("created.pid"),
            )
            .expect("publish complete creation checkpoint");
            // Parent kills this synthetic owner while the child is still suspended.
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
    // SAFETY: successful creation transfers two fresh non-inheritable handles to us.
    let (process_handle, thread_handle) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess.0),
            OwnedHandle::from_raw_handle(info.hThread.0),
        )
    };
    let prepare = || -> io::Result<Identity> {
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: four independent writable FILETIME buffers and a retained process handle.
        unsafe {
            GetProcessTimes(
                handle(&process_handle),
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        }
        .map_err(error)?;
        let resume_thread = handle(&thread_handle);
        #[cfg(test)]
        let resume_thread = if REFUSE_RESUME.get() {
            SUSPENDED_PROCESS.set(Some(process_handle.try_clone()?));
            HANDLE::default()
        } else {
            resume_thread
        };
        // SAFETY: the primary thread remains suspended and retained.
        // Unit tests pass an invalid handle to exercise failure before resume.
        if unsafe { ResumeThread(resume_thread) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(Identity {
            pid: info.dwProcessId,
            creation_time: (u64::from(created.dwHighDateTime) << 32)
                | u64::from(created.dwLowDateTime),
        })
    };
    let identity = match prepare() {
        Ok(identity) => identity,
        Err(error) => {
            // SAFETY: the retained handle targets exactly the newly created job member.
            unsafe { TerminateProcess(handle(&process_handle), 1) }.map_err(self::error)?;
            // SAFETY: termination was requested on this retained process handle.
            if unsafe { WaitForSingleObject(handle(&process_handle), INFINITE) } != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            return Err(error);
        }
    };
    Ok(Spawned {
        process: Process {
            handle: process_handle,
            identity,
        },
        job,
        stdin: File::from(stdin),
        stdout: File::from(stdout),
        stderr: File::from(stderr),
    })
}

/// Kernel-provided random nonce bytes for managed-server readiness.
pub fn random_nonce() -> io::Result<[u8; 16]> {
    let mut bytes = [0; 16];
    // SAFETY: the buffer is writable; system-preferred RNG takes no algorithm handle.
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(error)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::GetHandleInformation;

    #[test]
    fn owner_death_immediately_after_creation_kills_the_suspended_child() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        struct Observed(OwnedHandle);
        impl Drop for Observed {
            fn drop(&mut self) {
                // SAFETY: retained handle targets only the synthetic suspended child.
                let _ = unsafe { TerminateProcess(handle(&self.0), 1) };
                // SAFETY: the test retains synchronize access until cleanup completes.
                unsafe { WaitForSingleObject(handle(&self.0), INFINITE) };
            }
        }
        let directory =
            std::env::temp_dir().join(format!("oc-atomic-create-{}", std::process::id()));
        std::fs::create_dir(&directory).expect("unique fixture directory");
        let mut owner = ChildGuard(
            Command::new(std::env::current_exe().expect("test exe"))
                .args(["--exact", "tests::startup_owner_fixture", "--ignored"])
                .env("OC_STARTUP_WORK", &directory)
                .creation_flags(CREATE_NO_WINDOW.0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("startup owner"),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.join("created.pid").is_file() {
            assert!(Instant::now() < deadline, "owner creation checkpoint");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = std::fs::read_to_string(directory.join("created.pid"))
            .expect("child PID")
            .parse()
            .expect("numeric PID");
        // SAFETY: the checkpoint identifies this paused owner's fresh synthetic child.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, false, pid) }
            .expect("observe suspended child");
        // SAFETY: OpenProcess returned a fresh valid handle, retained until test cleanup.
        let observed = Observed(unsafe { OwnedHandle::from_raw_handle(raw.0) });
        owner.0.kill().expect("kill exact startup owner handle");
        owner.0.wait().expect("reap startup owner");
        // SAFETY: this retained synchronize handle remains valid throughout the wait.
        let result = unsafe { WaitForSingleObject(handle(&observed.0), 5000) };
        drop(observed);
        std::fs::remove_dir_all(directory).expect("remove synthetic checkpoint");
        assert_eq!(
            result, WAIT_OBJECT_0,
            "owner death must kill the child before resume or post-creation setup"
        );
    }

    #[test]
    #[ignore]
    fn startup_owner_fixture() {
        let Some(directory) = std::env::var_os("OC_STARTUP_WORK") else {
            return;
        };
        PAUSE_AFTER_CREATE.set(Some(directory.into()));
        let _ = spawn(&std::env::current_exe().expect("test exe"), &[], &[]);
        panic!("startup checkpoint was not reached");
    }

    #[test]
    fn invalid_atomic_job_assignment_creates_no_child() {
        CREATION_REACHED.set(false);
        REFUSE_ASSIGNMENT.set(true);
        let result = spawn(&std::env::current_exe().expect("test executable"), &[], &[]);
        REFUSE_ASSIGNMENT.set(false);
        assert!(result.is_err(), "invalid job assignment must fail");
        assert!(
            !CREATION_REACHED.get(),
            "creation cannot succeed without job ownership"
        );
    }

    #[test]
    fn resume_failure_terminates_and_reaps_the_suspended_child() {
        REFUSE_RESUME.set(true);
        let result = spawn(&std::env::current_exe().expect("test executable"), &[], &[]);
        REFUSE_RESUME.set(false);
        assert!(
            result.is_err(),
            "invalid primary-thread handle must fail resume"
        );
        let observed = SUSPENDED_PROCESS.take().expect("suspended process handle");
        // SAFETY: the test retains a duplicate synchronize-capable process handle.
        assert_eq!(
            unsafe { WaitForSingleObject(handle(&observed), 0) },
            WAIT_OBJECT_0,
            "the suspended process was terminated and reaped before returning failure"
        );
    }

    #[test]
    fn retained_job_and_parent_pipe_handles_are_noninheritable() {
        let job = Job::new().expect("job");
        let (parent, _child) = pipe(true).expect("pipe");
        for retained in [&job.handle, &parent] {
            let mut flags = 0;
            // SAFETY: valid retained kernel handle and writable flag output.
            unsafe { GetHandleInformation(handle(retained), &mut flags) }.expect("handle flags");
            assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0);
        }
    }

    #[test]
    fn retained_process_handle_is_noninheritable() {
        let started = spawn(
            &std::env::current_exe().expect("test executable"),
            &[
                "--exact".into(),
                "tests::child_fixture".into(),
                "--ignored".into(),
            ],
            &[],
        )
        .expect("fixture");
        let mut flags = 0;
        // SAFETY: the process handle is retained while its flags are queried.
        unsafe { GetHandleInformation(handle(&started.process.handle), &mut flags) }
            .expect("process flags");
        assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0);
        drop(started.stdin);
        assert_eq!(started.process.wait().expect("fixture exit"), 0);
        assert!(
            started
                .job
                .wait_empty(Duration::from_secs(5))
                .expect("job accounting")
        );
    }

    #[test]
    #[ignore]
    fn child_fixture() {
        use std::io::Read;
        std::io::stdin()
            .read_to_end(&mut Vec::new())
            .expect("fixture stdin");
    }
}
