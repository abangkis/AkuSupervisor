#![allow(unsafe_code)]

use std::fmt;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
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

const MAX_OWNED_PROCESSES: usize = 4_096;
const FORCE_EXIT_CODE: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const FORCED_SHUTDOWN_CONFIRMATION: Duration = Duration::from_secs(5);

/// A process tree bounded by a Windows Job Object created in this session.
#[derive(Debug)]
pub struct OwnedProcessTree {
    job: OwnedHandle,
    child: Child,
    root_pid: u32,
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
        let job = create_kill_on_close_job()?;
        command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
        let mut child = command.spawn().map_err(ProcessTreeError::Spawn)?;
        let root_pid = child.id();

        let process_handle = child.as_raw_handle().cast();
        if unsafe { AssignProcessToJobObject(job.raw(), process_handle) } == 0 {
            let error = io::Error::last_os_error();
            terminate_suspended_child(&mut child);
            return Err(ProcessTreeError::AssignJob(error));
        }

        if let Err(error) = resume_process_thread(root_pid) {
            terminate_suspended_child(&mut child);
            return Err(error);
        }

        Ok(Self {
            job,
            child,
            root_pid,
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
    pub fn stop(&mut self, grace: Duration) -> Result<StopReport, ProcessTreeError> {
        let owned_pids_before = self.owned_pids()?;
        if owned_pids_before.is_empty() {
            return Ok(StopReport {
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
        Ok(StopReport {
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
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        if self.owned_pids().is_ok_and(|pids| !pids.is_empty()) {
            unsafe {
                TerminateJobObject(self.job.raw(), FORCE_EXIT_CODE);
            }
            let _ = self.child.wait();
        }
    }
}

/// Observable outcome of an owned-tree stop operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    pub owned_pids_before: Vec<u32>,
    pub owned_pids_after: Vec<u32>,
    pub graceful_signal_sent: bool,
    pub graceful_signal_error: Option<String>,
    pub forced: bool,
}

/// Stage-specific Windows process ownership error.
#[derive(Debug)]
pub enum ProcessTreeError {
    CreateJob(io::Error),
    ConfigureJob(io::Error),
    Spawn(io::Error),
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
