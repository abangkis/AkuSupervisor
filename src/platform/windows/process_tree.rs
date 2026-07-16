#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::os::windows::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    STILL_ACTIVE, SetHandleInformation, WAIT_FAILED,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    GetExitCodeProcess, GetThreadTimes, INFINITE, OpenThread, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOW, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
    TerminateProcess, WaitForSingleObject,
};

use crate::application::{ManagedProcessTree, TreeStopReport};

const MAX_OWNED_PROCESSES: usize = 4_096;
const FORCE_EXIT_CODE: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const FORCED_SHUTDOWN_CONFIRMATION: Duration = Duration::from_secs(5);
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_GENERATIONS: usize = 5;

/// A process tree bounded by a Windows Job Object created in this session.
#[derive(Debug)]
pub struct OwnedProcessTree {
    job: OwnedHandle,
    child: RootProcess,
    root_pid: u32,
    log_threads: Vec<LogThread>,
}

#[derive(Debug)]
enum RootProcess {
    Standard(Child),
    Native(NativeChild),
}

impl RootProcess {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Standard(child) => child.try_wait(),
            Self::Native(child) => child.try_wait(),
        }
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        match self {
            Self::Standard(child) => child.wait(),
            Self::Native(child) => child.wait(),
        }
    }
}

#[derive(Debug)]
struct NativeChild {
    process: OwnedHandle,
}

impl NativeChild {
    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(self.process.raw(), &raw mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if exit_code == STILL_ACTIVE as u32 {
            Ok(None)
        } else {
            Ok(Some(ExitStatus::from_raw(exit_code)))
        }
    }

    fn wait(&self) -> io::Result<ExitStatus> {
        if unsafe { WaitForSingleObject(self.process.raw(), INFINITE) } == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.try_wait()?.ok_or_else(|| {
            io::Error::other("process remained active after its wait handle was signaled")
        })
    }
}

impl OwnedProcessTree {
    /// Starts a command suspended, assigns it to a new Job Object, and resumes
    /// it only after the ownership boundary is active.
    ///
    /// The command inherits the host context of the `AkuSupervisor` process.
    /// `CREATE_NEW_PROCESS_GROUP` permits a targeted Ctrl+Break request before
    /// the Job Object is used as the forced-shutdown boundary.
    ///
    /// # Errors
    ///
    /// Returns a stage-specific error if the Job Object cannot be configured,
    /// the process cannot be spawned or assigned, or its primary thread cannot
    /// be resumed. A process that fails before resume is terminated.
    pub fn spawn(command: &mut Command) -> Result<Self, ProcessTreeError> {
        Self::spawn_with_logs(command, None, None)
    }

    pub(crate) fn spawn_with_logs(
        command: &mut Command,
        stdout_path: Option<&Path>,
        stderr_path: Option<&Path>,
    ) -> Result<Self, ProcessTreeError> {
        if is_batch_program(command.get_program()) {
            Self::spawn_batch_with_logs(command, stdout_path, stderr_path)
        } else {
            Self::spawn_native_with_logs(command, stdout_path, stderr_path)
        }
    }

    fn spawn_batch_with_logs(
        command: &mut Command,
        stdout_path: Option<&Path>,
        stderr_path: Option<&Path>,
    ) -> Result<Self, ProcessTreeError> {
        let job = create_kill_on_close_job()?;
        let stdout_writer = stdout_path
            .map(RotatingLogWriter::open)
            .transpose()
            .map_err(|source| ProcessTreeError::LogSetup {
                path: stdout_path.expect("mapped stdout path").to_owned(),
                source,
            })?;
        let stderr_writer = stderr_path
            .map(RotatingLogWriter::open)
            .transpose()
            .map_err(|source| ProcessTreeError::LogSetup {
                path: stderr_path.expect("mapped stderr path").to_owned(),
                source,
            })?;
        if stdout_writer.is_some() {
            command.stdout(Stdio::piped());
        }
        if stderr_writer.is_some() {
            command.stderr(Stdio::piped());
        }
        command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
        let mut child = command.spawn().map_err(ProcessTreeError::Spawn)?;
        let root_pid = child.id();

        let process_handle = child.as_raw_handle().cast();
        if unsafe { AssignProcessToJobObject(job.raw(), process_handle) } == 0 {
            let error = io::Error::last_os_error();
            terminate_suspended_child(&mut child);
            return Err(ProcessTreeError::AssignJob(error));
        }

        let log_threads = (|| {
            let mut threads = Vec::new();
            if let Some(writer) = stdout_writer {
                let reader = child
                    .stdout
                    .take()
                    .ok_or_else(|| ProcessTreeError::LogSetup {
                        path: writer.path.clone(),
                        source: io::Error::other("stdout pipe was not created"),
                    })?;
                threads.push(spawn_log_thread(reader, writer)?);
            }
            if let Some(writer) = stderr_writer {
                let reader = child
                    .stderr
                    .take()
                    .ok_or_else(|| ProcessTreeError::LogSetup {
                        path: writer.path.clone(),
                        source: io::Error::other("stderr pipe was not created"),
                    })?;
                threads.push(spawn_log_thread(reader, writer)?);
            }
            Ok(threads)
        })();
        let log_threads = match log_threads {
            Ok(threads) => threads,
            Err(error) => {
                terminate_suspended_child(&mut child);
                return Err(error);
            }
        };

        if let Err(error) = resume_process_thread(root_pid) {
            terminate_suspended_child(&mut child);
            for thread in log_threads {
                thread.handle.join().ok();
            }
            return Err(error);
        }

        Ok(Self {
            job,
            child: RootProcess::Standard(child),
            root_pid,
            log_threads,
        })
    }

    fn spawn_native_with_logs(
        command: &Command,
        stdout_path: Option<&Path>,
        stderr_path: Option<&Path>,
    ) -> Result<Self, ProcessTreeError> {
        let job = create_kill_on_close_job()?;
        let stdout_writer = stdout_path
            .map(RotatingLogWriter::open)
            .transpose()
            .map_err(|source| ProcessTreeError::LogSetup {
                path: stdout_path.expect("mapped stdout path").to_owned(),
                source,
            })?;
        let stderr_writer = stderr_path
            .map(RotatingLogWriter::open)
            .transpose()
            .map_err(|source| ProcessTreeError::LogSetup {
                path: stderr_path.expect("mapped stderr path").to_owned(),
                source,
            })?;

        let mut spawned =
            spawn_native_suspended(command, stdout_writer.is_some(), stderr_writer.is_some())?;
        let root_pid = spawned.pid;

        if unsafe { AssignProcessToJobObject(job.raw(), spawned.child.process.raw()) } == 0 {
            let error = io::Error::last_os_error();
            terminate_native_child(&mut spawned.child);
            return Err(ProcessTreeError::AssignJob(error));
        }

        let log_threads = (|| {
            let mut threads = Vec::new();
            if let Some(writer) = stdout_writer {
                let reader = spawned
                    .stdout
                    .take()
                    .ok_or_else(|| ProcessTreeError::LogSetup {
                        path: writer.path.clone(),
                        source: io::Error::other("stdout pipe was not created"),
                    })?;
                threads.push(spawn_log_thread(reader, writer)?);
            }
            if let Some(writer) = stderr_writer {
                let reader = spawned
                    .stderr
                    .take()
                    .ok_or_else(|| ProcessTreeError::LogSetup {
                        path: writer.path.clone(),
                        source: io::Error::other("stderr pipe was not created"),
                    })?;
                threads.push(spawn_log_thread(reader, writer)?);
            }
            Ok(threads)
        })();
        let log_threads = match log_threads {
            Ok(threads) => threads,
            Err(error) => {
                terminate_native_child(&mut spawned.child);
                return Err(error);
            }
        };

        // Resume exactly the suspension added by CREATE_SUSPENDED through the
        // primary thread handle returned by CreateProcessW. Never enumerate or
        // resume injected threads, and never drain a suspension owned by EDR.
        let previous_suspend_count = unsafe { ResumeThread(spawned.primary_thread.raw()) };
        if previous_suspend_count == u32::MAX || previous_suspend_count == 0 {
            let error = if previous_suspend_count == u32::MAX {
                io::Error::last_os_error()
            } else {
                io::Error::other("primary thread was not suspended")
            };
            terminate_native_child(&mut spawned.child);
            for thread in log_threads {
                thread.handle.join().ok();
            }
            return Err(ProcessTreeError::ResumePrimaryThread(error));
        }

        Ok(Self {
            job,
            child: RootProcess::Native(spawned.child),
            root_pid,
            log_threads,
        })
    }

    #[must_use]
    pub const fn root_pid(&self) -> u32 {
        self.root_pid
    }

    /// Returns the PIDs currently associated with this Job Object.
    ///
    /// Job membership, rather than PID name or port use, is the ownership
    /// evidence used for every destructive operation.
    ///
    /// # Errors
    ///
    /// Returns an operating-system query error or a bounded-capacity error.
    pub fn owned_pids(&self) -> Result<Vec<u32>, ProcessTreeError> {
        query_job_pids(self.job.raw())
    }

    /// Checks current Job Object membership for a PID.
    ///
    /// # Errors
    ///
    /// Returns the same query errors as [`Self::owned_pids`].
    pub fn owns_pid(&self, pid: u32) -> Result<bool, ProcessTreeError> {
        Ok(self.owned_pids()?.contains(&pid))
    }

    /// Returns the launcher exit status when it has exited.
    ///
    /// # Errors
    ///
    /// Returns an operating-system process wait error.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessTreeError> {
        self.child.try_wait().map_err(ProcessTreeError::Wait)
    }

    /// Requests Ctrl+Break, waits for graceful shutdown, and forcibly
    /// terminates only the owned Job Object if processes remain.
    ///
    /// Failure to deliver Ctrl+Break is recorded in the report and causes the
    /// operation to continue to the ownership-bounded forced path.
    ///
    /// # Errors
    ///
    /// Returns a Job Object query or termination error, or
    /// [`ProcessTreeError::ShutdownTimeout`] if owned PIDs remain after forced
    /// termination.
    pub fn stop(&mut self, grace: Duration) -> Result<TreeStopReport, ProcessTreeError> {
        let owned_pids_before = self.owned_pids()?;
        if owned_pids_before.is_empty() {
            return Ok(TreeStopReport {
                owned_pids_before,
                owned_pids_after: Vec::new(),
                graceful_signal_sent: false,
                graceful_signal_error: None,
                forced: false,
            });
        }

        let signal_result = send_ctrl_break(self.root_pid);
        let graceful_signal_error = signal_result.as_ref().err().map(ToString::to_string);
        let graceful_signal_sent = signal_result.is_ok();
        let mut forced = false;

        if !self.wait_until_empty(grace)? {
            forced = true;
            if unsafe { TerminateJobObject(self.job.raw(), FORCE_EXIT_CODE) } == 0 {
                return Err(ProcessTreeError::TerminateJob(io::Error::last_os_error()));
            }
            if !self.wait_until_empty(FORCED_SHUTDOWN_CONFIRMATION)? {
                return Err(ProcessTreeError::ShutdownTimeout {
                    remaining_pids: self.owned_pids()?,
                });
            }
        }

        self.child.try_wait().map_err(ProcessTreeError::Wait)?;
        self.finish_log_threads()?;
        Ok(TreeStopReport {
            owned_pids_before,
            owned_pids_after: self.owned_pids()?,
            graceful_signal_sent,
            graceful_signal_error,
            forced,
        })
    }

    fn wait_until_empty(&self, timeout: Duration) -> Result<bool, ProcessTreeError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.owned_pids()?.is_empty() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn finish_log_threads(&mut self) -> Result<(), ProcessTreeError> {
        for thread in self.log_threads.drain(..) {
            match thread.handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(source)) => {
                    return Err(ProcessTreeError::LogPump {
                        path: thread.path,
                        source,
                    });
                }
                Err(_) => return Err(ProcessTreeError::LogThreadPanicked { path: thread.path }),
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct NativeSpawn {
    child: NativeChild,
    primary_thread: OwnedHandle,
    pid: u32,
    stdout: Option<File>,
    stderr: Option<File>,
}

#[derive(Debug)]
struct PreparedPipe {
    read: OwnedHandle,
    write: OwnedHandle,
}

fn spawn_native_suspended(
    command: &Command,
    capture_stdout: bool,
    capture_stderr: bool,
) -> Result<NativeSpawn, ProcessTreeError> {
    let stdout_pipe = capture_stdout.then(create_inheritable_pipe).transpose()?;
    let stderr_pipe = capture_stderr.then(create_inheritable_pipe).transpose()?;

    let application = wide_null(command.get_program()).map_err(ProcessTreeError::Spawn)?;
    let mut command_line = build_command_line(command).map_err(ProcessTreeError::Spawn)?;
    let current_directory = command
        .get_current_dir()
        .map(|directory| wide_null(directory.as_os_str()))
        .transpose()
        .map_err(ProcessTreeError::Spawn)?;
    let environment = build_environment_block(command).map_err(ProcessTreeError::Spawn)?;

    let mut startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).expect("STARTUPINFOW size fits u32"),
        ..STARTUPINFOW::default()
    };
    let inherit_handles = stdout_pipe.is_some() || stderr_pipe.is_some();
    let inherited_stdin = inherit_handles.then(open_inheritable_null).transpose()?;
    if inherit_handles {
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = inherited_stdin
            .as_ref()
            .expect("inheritable stdin was prepared")
            .raw();
        startup.hStdOutput = stdout_pipe.as_ref().map_or_else(
            || unsafe { GetStdHandle(STD_OUTPUT_HANDLE) },
            |pipe| pipe.write.raw(),
        );
        startup.hStdError = stderr_pipe.as_ref().map_or_else(
            || unsafe { GetStdHandle(STD_ERROR_HANDLE) },
            |pipe| pipe.write.raw(),
        );
    }

    let mut process_information = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(inherit_handles),
            CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
            environment
                .as_ref()
                .map_or(ptr::null(), |block| block.as_ptr().cast()),
            current_directory.as_ref().map_or(ptr::null(), Vec::as_ptr),
            &raw const startup,
            &raw mut process_information,
        )
    };
    if created == 0 {
        return Err(ProcessTreeError::Spawn(io::Error::last_os_error()));
    }

    let process =
        OwnedHandle::from_nullable(process_information.hProcess, ProcessTreeError::Spawn)?;
    let primary_thread =
        OwnedHandle::from_nullable(process_information.hThread, ProcessTreeError::Spawn)?;

    // The child owns the write ends after CreateProcessW. Closing the parent
    // copies ensures each log reader reaches EOF when the child tree exits.
    let stdout = stdout_pipe.map(|pipe| {
        drop(pipe.write);
        unsafe { File::from_raw_handle(pipe.read.into_raw_handle()) }
    });
    let stderr = stderr_pipe.map(|pipe| {
        drop(pipe.write);
        unsafe { File::from_raw_handle(pipe.read.into_raw_handle()) }
    });

    Ok(NativeSpawn {
        child: NativeChild { process },
        primary_thread,
        pid: process_information.dwProcessId,
        stdout,
        stderr,
    })
}

fn create_inheritable_pipe() -> Result<PreparedPipe, ProcessTreeError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES size fits u32"),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    if unsafe { CreatePipe(&raw mut read, &raw mut write, &raw const attributes, 0) } == 0 {
        return Err(ProcessTreeError::StdioPipe(io::Error::last_os_error()));
    }
    let read = OwnedHandle::from_nullable(read, ProcessTreeError::StdioPipe)?;
    let write = OwnedHandle::from_nullable(write, ProcessTreeError::StdioPipe)?;
    if unsafe { SetHandleInformation(read.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(ProcessTreeError::StdioPipe(io::Error::last_os_error()));
    }
    Ok(PreparedPipe { read, write })
}

fn open_inheritable_null() -> Result<OwnedHandle, ProcessTreeError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES size fits u32"),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let null_device = wide_null(OsStr::new("NUL")).expect("NUL contains no embedded NUL");
    let handle = unsafe {
        CreateFileW(
            null_device.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &raw const attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(ProcessTreeError::StdioPipe(io::Error::last_os_error()))
    } else {
        OwnedHandle::from_nullable(handle, ProcessTreeError::StdioPipe)
    }
}

fn build_command_line(command: &Command) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    append_windows_argument(&mut line, command.get_program())?;
    for argument in command.get_args() {
        line.push(u16::from(b' '));
        append_windows_argument(&mut line, argument)?;
    }
    line.push(0);
    Ok(line)
}

fn append_windows_argument(line: &mut Vec<u16>, argument: &OsStr) -> io::Result<()> {
    let wide = argument.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process argument contains NUL",
        ));
    }
    let quote = wide.is_empty()
        || wide.iter().any(|value| {
            *value == u16::from(b' ') || *value == u16::from(b'\t') || *value == u16::from(b'"')
        });
    if !quote {
        line.extend(wide);
        return Ok(());
    }

    line.push(u16::from(b'"'));
    let mut backslashes = 0;
    for value in wide {
        if value == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if value == u16::from(b'"') {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        backslashes = 0;
        line.push(value);
    }
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    line.push(u16::from(b'"'));
    Ok(())
}

fn build_environment_block(command: &Command) -> io::Result<Option<Vec<u16>>> {
    let overrides = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
        .collect::<Vec<_>>();
    if overrides.is_empty() {
        return Ok(None);
    }

    let mut environment = BTreeMap::<String, (OsString, OsString)>::new();
    for (key, value) in std::env::vars_os() {
        environment.insert(environment_key(&key), (key, value));
    }
    for (key, value) in overrides {
        let normalized = environment_key(&key);
        if let Some(value) = value {
            environment.insert(normalized, (key, value));
        } else {
            environment.remove(&normalized);
        }
    }

    let mut block = Vec::new();
    for (_, (key, value)) in environment {
        let key = wide_without_nul(&key, "environment key")?;
        if key.is_empty() || key.contains(&u16::from(b'=')) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "environment key is empty or contains '='",
            ));
        }
        block.extend(key);
        block.push(u16::from(b'='));
        block.extend(wide_without_nul(&value, "environment value")?);
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(Some(block))
}

fn environment_key(key: &OsStr) -> String {
    key.to_string_lossy().to_uppercase()
}

fn wide_null(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide = wide_without_nul(value, "Windows string")?;
    wide.push(0);
    Ok(wide)
}

fn wide_without_nul(value: &OsStr, label: &str) -> io::Result<Vec<u16>> {
    let wide = value.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains NUL"),
        ))
    } else {
        Ok(wide)
    }
}

fn is_batch_program(program: &OsStr) -> bool {
    Path::new(program)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        })
}

fn terminate_native_child(child: &mut NativeChild) {
    unsafe {
        TerminateProcess(child.process.raw(), FORCE_EXIT_CODE);
        WaitForSingleObject(child.process.raw(), INFINITE);
    }
}

impl ManagedProcessTree for OwnedProcessTree {
    type Error = ProcessTreeError;

    fn root_pid(&self) -> u32 {
        Self::root_pid(self)
    }

    fn owned_pids(&self) -> Result<Vec<u32>, Self::Error> {
        Self::owned_pids(self)
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, Self::Error> {
        Self::try_wait(self)
    }

    fn stop(&mut self, grace: Duration) -> Result<TreeStopReport, Self::Error> {
        Self::stop(self, grace)
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        if self.owned_pids().is_ok_and(|pids| !pids.is_empty()) {
            unsafe {
                TerminateJobObject(self.job.raw(), FORCE_EXIT_CODE);
            }
            let _ = self.child.wait();
        }
        for thread in self.log_threads.drain(..) {
            thread.handle.join().ok();
        }
    }
}

#[derive(Debug)]
struct LogThread {
    path: PathBuf,
    handle: thread::JoinHandle<io::Result<()>>,
}

fn spawn_log_thread(
    mut reader: impl Read + Send + 'static,
    mut writer: RotatingLogWriter,
) -> Result<LogThread, ProcessTreeError> {
    let path = writer.path.clone();
    let thread_path = path.clone();
    let handle = thread::Builder::new()
        .name("aku-supervisor-log-pump".to_owned())
        .spawn(move || {
            io::copy(&mut reader, &mut writer)?;
            writer.flush()
        })
        .map_err(|source| ProcessTreeError::LogSetup {
            path: thread_path,
            source,
        })?;
    Ok(LogThread { path, handle })
}

#[derive(Debug)]
struct RotatingLogWriter {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
}

impl RotatingLogWriter {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut writer = Self {
            path: path.to_owned(),
            file: None,
            bytes: fs::metadata(path).map_or(0, |metadata| metadata.len()),
        };
        if writer.bytes >= MAX_LOG_BYTES {
            writer.rotate()?;
        } else {
            writer.open_active()?;
        }
        Ok(writer)
    }

    fn open_active(&mut self) -> io::Result<()> {
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        self.bytes = fs::metadata(&self.path).map_or(0, |metadata| metadata.len());
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.take();
        let oldest = generation_path(&self.path, LOG_GENERATIONS);
        if oldest.exists() {
            fs::remove_file(oldest)?;
        }
        for generation in (1..LOG_GENERATIONS).rev() {
            let source = generation_path(&self.path, generation);
            if source.exists() {
                fs::rename(source, generation_path(&self.path, generation + 1))?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, generation_path(&self.path, 1))?;
        }
        self.bytes = 0;
        self.open_active()
    }
}

impl Write for RotatingLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut offset = 0;
        while offset < buffer.len() {
            if self.bytes >= MAX_LOG_BYTES {
                self.rotate()?;
            }
            let available = usize::try_from(MAX_LOG_BYTES - self.bytes)
                .unwrap_or(usize::MAX)
                .min(buffer.len() - offset);
            self.file
                .as_mut()
                .expect("active log file is open")
                .write_all(&buffer[offset..offset + available])?;
            self.bytes += u64::try_from(available).expect("write length fits u64");
            offset += available;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("active log file is open").flush()
    }
}

fn generation_path(path: &Path, generation: usize) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".{generation}"));
    PathBuf::from(value)
}

/// Stage-specific Windows process ownership error.
#[derive(Debug)]
pub enum ProcessTreeError {
    CreateJob(io::Error),
    ConfigureJob(io::Error),
    Spawn(io::Error),
    StdioPipe(io::Error),
    LogSetup { path: PathBuf, source: io::Error },
    LogPump { path: PathBuf, source: io::Error },
    LogThreadPanicked { path: PathBuf },
    AssignJob(io::Error),
    ThreadSnapshot(io::Error),
    PrimaryThreadNotFound { pid: u32 },
    OpenPrimaryThread(io::Error),
    QueryPrimaryThreadTime(io::Error),
    ResumePrimaryThread(io::Error),
    QueryJob(io::Error),
    TooManyOwnedProcesses { assigned: usize, capacity: usize },
    CtrlBreak(io::Error),
    TerminateJob(io::Error),
    Wait(io::Error),
    ShutdownTimeout { remaining_pids: Vec<u32> },
}

impl fmt::Display for ProcessTreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateJob(error) => write!(formatter, "create Job Object failed: {error}"),
            Self::ConfigureJob(error) => write!(formatter, "configure Job Object failed: {error}"),
            Self::Spawn(error) => write!(formatter, "spawn suspended process failed: {error}"),
            Self::StdioPipe(error) => {
                write!(
                    formatter,
                    "create inherited service log pipe failed: {error}"
                )
            }
            Self::LogSetup { path, source } => {
                write!(
                    formatter,
                    "service log setup failed for {}: {source}",
                    path.display()
                )
            }
            Self::LogPump { path, source } => {
                write!(
                    formatter,
                    "service log capture failed for {}: {source}",
                    path.display()
                )
            }
            Self::LogThreadPanicked { path } => {
                write!(
                    formatter,
                    "service log capture panicked for {}",
                    path.display()
                )
            }
            Self::AssignJob(error) => {
                write!(formatter, "assign process to Job Object failed: {error}")
            }
            Self::ThreadSnapshot(error) => write!(formatter, "thread snapshot failed: {error}"),
            Self::PrimaryThreadNotFound { pid } => {
                write!(formatter, "no suspended primary thread found for PID {pid}")
            }
            Self::OpenPrimaryThread(error) => {
                write!(formatter, "open primary thread failed: {error}")
            }
            Self::QueryPrimaryThreadTime(error) => {
                write!(
                    formatter,
                    "query primary thread creation time failed: {error}"
                )
            }
            Self::ResumePrimaryThread(error) => {
                write!(formatter, "resume primary thread failed: {error}")
            }
            Self::QueryJob(error) => write!(formatter, "query Job Object failed: {error}"),
            Self::TooManyOwnedProcesses { assigned, capacity } => write!(
                formatter,
                "Job Object contains {assigned} processes; observation capacity is {capacity}"
            ),
            Self::CtrlBreak(error) => write!(formatter, "send Ctrl+Break failed: {error}"),
            Self::TerminateJob(error) => write!(formatter, "terminate Job Object failed: {error}"),
            Self::Wait(error) => write!(formatter, "wait for launcher process failed: {error}"),
            Self::ShutdownTimeout { remaining_pids } => {
                write!(
                    formatter,
                    "owned PIDs remained after termination: {remaining_pids:?}"
                )
            }
        }
    }
}

impl std::error::Error for ProcessTreeError {}

#[derive(Debug)]
struct OwnedHandle(isize);

impl OwnedHandle {
    fn from_nullable(
        raw: HANDLE,
        stage: fn(io::Error) -> ProcessTreeError,
    ) -> Result<Self, ProcessTreeError> {
        if raw.is_null() {
            Err(stage(io::Error::last_os_error()))
        } else {
            Ok(Self(raw as isize))
        }
    }

    fn from_snapshot(raw: HANDLE) -> Result<Self, ProcessTreeError> {
        if raw == INVALID_HANDLE_VALUE {
            Err(ProcessTreeError::ThreadSnapshot(io::Error::last_os_error()))
        } else {
            Ok(Self(raw as isize))
        }
    }

    const fn raw(&self) -> HANDLE {
        self.0 as HANDLE
    }

    fn into_raw_handle(self) -> RawHandle {
        let handle = self.raw().cast();
        std::mem::forget(self);
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.raw());
        }
    }
}

fn create_kill_on_close_job() -> Result<OwnedHandle, ProcessTreeError> {
    let job = OwnedHandle::from_nullable(
        unsafe { CreateJobObjectW(ptr::null(), ptr::null()) },
        ProcessTreeError::CreateJob,
    )?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&limits).cast(),
            u32::try_from(size_of_val(&limits)).expect("Job Object limits fit in u32"),
        )
    };
    if configured == 0 {
        return Err(ProcessTreeError::ConfigureJob(io::Error::last_os_error()));
    }
    Ok(job)
}

#[derive(Debug)]
struct ThreadCandidate {
    thread_id: u32,
    creation_time: u64,
    handle: OwnedHandle,
}

fn resume_process_thread(pid: u32) -> Result<(), ProcessTreeError> {
    let snapshot =
        OwnedHandle::from_snapshot(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).expect("THREADENTRY32 size fits u32"),
        ..THREADENTRY32::default()
    };
    if unsafe { Thread32First(snapshot.raw(), &raw mut entry) } == 0 {
        return Err(ProcessTreeError::ThreadSnapshot(io::Error::last_os_error()));
    }

    let mut candidates = Vec::new();
    let mut inspection_error = None;
    loop {
        if entry.th32OwnerProcessID == pid {
            let access = THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION;
            match OwnedHandle::from_nullable(
                unsafe { OpenThread(access, 0, entry.th32ThreadID) },
                ProcessTreeError::OpenPrimaryThread,
            ) {
                Ok(handle) => match thread_creation_time(handle.raw()) {
                    Ok(creation_time) => candidates.push(ThreadCandidate {
                        thread_id: entry.th32ThreadID,
                        creation_time,
                        handle,
                    }),
                    Err(error) => {
                        inspection_error = Some(ProcessTreeError::QueryPrimaryThreadTime(error));
                    }
                },
                Err(error) => inspection_error = Some(error),
            }
        }
        if unsafe { Thread32Next(snapshot.raw(), &raw mut entry) } == 0 {
            break;
        }
    }

    if candidates.is_empty() {
        return Err(inspection_error.unwrap_or(ProcessTreeError::PrimaryThreadNotFound { pid }));
    }

    // CREATE_SUSPENDED applies to the primary thread. Security software can
    // inject an additional thread before this snapshot and Toolhelp does not
    // guarantee enumeration order, so inspect oldest threads first and never
    // treat a zero previous suspend count as a successful resume.
    candidates.sort_by_key(|candidate| (candidate.creation_time, candidate.thread_id));
    let resumed = resume_first_suspended_candidate(&mut candidates, |candidate| {
        let previous = unsafe { ResumeThread(candidate.handle.raw()) };
        if previous == u32::MAX {
            Err(ProcessTreeError::ResumePrimaryThread(
                io::Error::last_os_error(),
            ))
        } else {
            Ok(previous)
        }
    })?;
    if resumed {
        Ok(())
    } else {
        Err(ProcessTreeError::PrimaryThreadNotFound { pid })
    }
}

fn thread_creation_time(thread: HANDLE) -> Result<u64, io::Error> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe {
        GetThreadTimes(
            thread,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn resume_first_suspended_candidate<T, E>(
    candidates: &mut [T],
    mut resume: impl FnMut(&mut T) -> Result<u32, E>,
) -> Result<bool, E> {
    for candidate in candidates {
        if resume(candidate)? > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn query_job_pids(job: HANDLE) -> Result<Vec<u32>, ProcessTreeError> {
    let header_words = size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>().div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; header_words + MAX_OWNED_PROCESSES];
    let byte_length = u32::try_from(buffer.len() * size_of::<usize>())
        .expect("bounded Job Object PID buffer fits u32");
    let success = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buffer.as_mut_ptr().cast(),
            byte_length,
            ptr::null_mut(),
        )
    };
    if success == 0 {
        return Err(ProcessTreeError::QueryJob(io::Error::last_os_error()));
    }

    let info = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
    let assigned = unsafe { (*info).NumberOfAssignedProcesses as usize };
    let listed = unsafe { (*info).NumberOfProcessIdsInList as usize };
    if assigned > MAX_OWNED_PROCESSES || listed > MAX_OWNED_PROCESSES || listed < assigned {
        return Err(ProcessTreeError::TooManyOwnedProcesses {
            assigned,
            capacity: MAX_OWNED_PROCESSES,
        });
    }
    let process_ids = unsafe { ptr::addr_of!((*info).ProcessIdList).cast::<usize>() };
    let slice = unsafe { std::slice::from_raw_parts(process_ids, listed) };
    slice
        .iter()
        .map(|pid| {
            u32::try_from(*pid).map_err(|_| ProcessTreeError::TooManyOwnedProcesses {
                assigned,
                capacity: MAX_OWNED_PROCESSES,
            })
        })
        .collect()
}

fn send_ctrl_break(process_group_id: u32) -> Result<(), ProcessTreeError> {
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id) } == 0 {
        Err(ProcessTreeError::CtrlBreak(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn terminate_suspended_child(child: &mut Child) {
    child.kill().ok();
    child.wait().ok();
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Write;
    use std::process::Command;

    use super::{
        MAX_LOG_BYTES, RotatingLogWriter, build_command_line, generation_path, is_batch_program,
        resume_first_suspended_candidate,
    };

    fn resume_count(count: &mut u32) -> Result<u32, &'static str> {
        if *count == u32::MAX {
            return Err("invalid fake suspend count");
        }
        let previous = *count;
        *count = count.saturating_sub(1);
        Ok(previous)
    }

    #[test]
    fn resume_skips_injected_thread_that_was_not_suspended() {
        let mut suspend_counts = [0, 1];

        assert!(
            resume_first_suspended_candidate(&mut suspend_counts, resume_count)
                .expect("infallible fake resume")
        );
        assert_eq!(suspend_counts, [0, 0]);
    }

    #[test]
    fn resume_leaves_injected_suspensions_owned_by_other_software() {
        let mut suspend_counts = [1, 1, 0];

        assert!(
            resume_first_suspended_candidate(&mut suspend_counts, resume_count)
                .expect("infallible fake resume")
        );
        assert_eq!(suspend_counts, [0, 1, 0]);
    }

    #[test]
    fn resume_removes_only_the_supervisor_owned_suspend_count() {
        let mut suspend_counts = [3];

        assert!(
            resume_first_suspended_candidate(&mut suspend_counts, resume_count)
                .expect("infallible fake resume")
        );
        assert_eq!(suspend_counts, [2]);
    }

    #[test]
    fn resume_rejects_candidates_that_are_all_running() {
        let mut suspend_counts = [0, 0];

        assert!(
            !resume_first_suspended_candidate(&mut suspend_counts, resume_count)
                .expect("infallible fake resume")
        );
    }

    #[test]
    fn native_command_line_preserves_windows_argument_boundaries() {
        let mut command = Command::new(r"C:\Program Files\fixture.exe");
        command.args([
            OsStr::new("plain"),
            OsStr::new("two words"),
            OsStr::new("embedded\"quote"),
            OsStr::new(r"trailing slash\"),
            OsStr::new(""),
        ]);

        let encoded = build_command_line(&command).expect("command line should encode");
        let value = String::from_utf16(&encoded[..encoded.len() - 1])
            .expect("test command line is Unicode");

        assert_eq!(
            value,
            r#""C:\Program Files\fixture.exe" plain "two words" "embedded\"quote" "trailing slash\\" """#
        );
    }

    #[test]
    fn batch_program_detection_is_case_insensitive_and_extension_bound() {
        assert!(is_batch_program(OsStr::new(r"C:\tools\npm.CMD")));
        assert!(is_batch_program(OsStr::new(r"C:\tools\build.bat")));
        assert!(!is_batch_program(OsStr::new(r"C:\tools\service.exe")));
        assert!(!is_batch_program(OsStr::new(r"C:\tools\not-a-cmd.txt")));
    }

    #[test]
    fn service_log_rotates_while_output_is_streaming() {
        let directory = std::env::temp_dir().join(format!(
            "aku-supervisor-log-rotation-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create rotation test directory");
        let path = directory.join("service.stdout.log");
        let mut writer = RotatingLogWriter::open(&path).expect("open rotating log");
        writer
            .write_all(&vec![
                b'x';
                usize::try_from(MAX_LOG_BYTES)
                    .expect("test log size fits usize")
                    + 3
            ])
            .expect("write across rotation boundary");
        writer.flush().expect("flush active log");
        drop(writer);

        assert_eq!(fs::metadata(&path).expect("active log").len(), 3);
        assert_eq!(
            fs::metadata(generation_path(&path, 1))
                .expect("first generation")
                .len(),
            MAX_LOG_BYTES
        );
        fs::remove_dir_all(directory).ok();
    }
}
