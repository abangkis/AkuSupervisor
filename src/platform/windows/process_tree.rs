#![allow(unsafe_code)]

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{size_of, size_of_val};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
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
    child: Child,
    root_pid: u32,
    log_threads: Vec<LogThread>,
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
            child,
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
    LogSetup { path: PathBuf, source: io::Error },
    LogPump { path: PathBuf, source: io::Error },
    LogThreadPanicked { path: PathBuf },
    AssignJob(io::Error),
    ThreadSnapshot(io::Error),
    PrimaryThreadNotFound { pid: u32 },
    OpenPrimaryThread(io::Error),
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

    loop {
        if entry.th32OwnerProcessID == pid {
            let thread = OwnedHandle::from_nullable(
                unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) },
                ProcessTreeError::OpenPrimaryThread,
            )?;
            if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
                return Err(ProcessTreeError::ResumePrimaryThread(
                    io::Error::last_os_error(),
                ));
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.raw(), &raw mut entry) } == 0 {
            break;
        }
    }
    Err(ProcessTreeError::PrimaryThreadNotFound { pid })
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
    use std::fs;
    use std::io::Write;

    use super::{MAX_LOG_BYTES, RotatingLogWriter, generation_path};

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
