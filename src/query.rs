use std::{ffi::c_void, mem, time::Duration};
use windows::Win32::{
    Foundation::ERROR_MORE_DATA,
    System::JobObjects::{
        JobObjectBasicAndIoAccountingInformation, JobObjectBasicProcessIdList,
        QueryInformationJobObject, JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST,
    },
};

use crate::{Job, JobError};

const INITIAL_PROCESS_ID_CAPACITY: usize = 1024;

const PROCESS_ID_LIST_OFFSET: usize =
    mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / mem::size_of::<usize>();

#[derive(Debug, Clone)]
pub struct BasicAndIoAccountingInfo {
    pub total_user_time: Duration,
    pub total_kernel_time: Duration,
    pub this_period_total_user_time: Duration,
    pub this_period_total_kernel_time: Duration,
    pub total_page_fault_count: u32,
    pub total_processes: u32,
    pub active_processes: u32,
    pub total_terminated_processes: u32,
    pub read_operation_count: u64,
    pub write_operation_count: u64,
    pub other_operation_count: u64,
    pub read_transfer_count: u64,
    pub write_transfer_count: u64,
    pub other_transfer_count: u64,
}

impl Job {
    /// Return all the process identifiers for a job object.
    /// If the job is nested, the process identifier list consists of all processes
    /// associated with the job and its child jobs.
    pub fn query_process_id_list(&self) -> Result<Vec<usize>, JobError> {
        let mut capacity = INITIAL_PROCESS_ID_CAPACITY;

        loop {
            let mut buffer = vec![0usize; PROCESS_ID_LIST_OFFSET + capacity];

            let result = unsafe {
                QueryInformationJobObject(
                    Some(self.handle),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr() as *mut c_void,
                    mem::size_of_val(buffer.as_slice()) as u32,
                    None,
                )
            };

            let header = unsafe {
                buffer
                    .as_ptr()
                    .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                    .read()
            };

            match result {
                Ok(()) => {
                    let process_ids = &buffer[PROCESS_ID_LIST_OFFSET..];

                    return Ok(process_ids[..header.NumberOfProcessIdsInList as usize].to_vec());
                }
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {
                    // WOW64 leaves NumberOfAssignedProcesses unset here, so grow at least twofold.
                    capacity = (header.NumberOfAssignedProcesses as usize).max(2 * capacity);
                }
                Err(e) => return Err(JobError::GetInfoFailed(e.into())),
            }
        }
    }

    /// Return basic accounting and I/O accounting information for a job object.
    /// The totals include processes that have ended and, if the job is nested, the processes in its
    /// child jobs; `total_terminated_processes` counts only those a limit violation terminated.
    /// See also [Microsoft Docs](https://docs.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_and_io_accounting_information).
    pub fn query_basic_and_io_accounting_info(&self) -> Result<BasicAndIoAccountingInfo, JobError> {
        let mut info = JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION::default();

        unsafe {
            QueryInformationJobObject(
                Some(self.handle),
                JobObjectBasicAndIoAccountingInformation,
                &mut info as *mut _ as *mut c_void,
                mem::size_of_val(&info) as u32,
                None,
            )
        }
        .map_err(|e| JobError::GetInfoFailed(e.into()))?;

        Ok(accounting_info(&info))
    }
}

fn accounting_info(
    info: &JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
) -> BasicAndIoAccountingInfo {
    BasicAndIoAccountingInfo {
        total_user_time: duration_from_ticks(info.BasicInfo.TotalUserTime),
        total_kernel_time: duration_from_ticks(info.BasicInfo.TotalKernelTime),
        this_period_total_user_time: duration_from_ticks(info.BasicInfo.ThisPeriodTotalUserTime),
        this_period_total_kernel_time: duration_from_ticks(
            info.BasicInfo.ThisPeriodTotalKernelTime,
        ),
        total_page_fault_count: info.BasicInfo.TotalPageFaultCount,
        total_processes: info.BasicInfo.TotalProcesses,
        active_processes: info.BasicInfo.ActiveProcesses,
        total_terminated_processes: info.BasicInfo.TotalTerminatedProcesses,
        read_operation_count: info.IoInfo.ReadOperationCount,
        write_operation_count: info.IoInfo.WriteOperationCount,
        other_operation_count: info.IoInfo.OtherOperationCount,
        read_transfer_count: info.IoInfo.ReadTransferCount,
        write_transfer_count: info.IoInfo.WriteTransferCount,
        other_transfer_count: info.IoInfo.OtherTransferCount,
    }
}

/// Convert a time the system reports in 100-nanosecond ticks.
fn duration_from_ticks(ticks: i64) -> Duration {
    let ticks = u64::try_from(ticks).unwrap_or(0);

    Duration::new(ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100)
}

#[cfg(test)]
mod tests {
    use std::{
        os::windows::{io::AsRawHandle, process::CommandExt},
        process::{Child, Command},
        time::Duration,
    };

    use rusty_fork::rusty_fork_test;
    use windows::Win32::{
        Foundation::HANDLE,
        System::{
            JobObjects::{
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
            },
            Threading::{CREATE_SUSPENDED, IO_COUNTERS},
        },
    };

    use super::{accounting_info, duration_from_ticks, INITIAL_PROCESS_ID_CAPACITY};
    use crate::{utils::get_current_process, ExtendedLimitInfo, Job, JobError};

    #[test]
    fn query_proc_id() {
        let job = Job::create().unwrap();

        let pids = job.query_process_id_list().unwrap();
        assert_eq!(pids, []);

        job.assign_current_process().unwrap();

        let pids = job.query_process_id_list().unwrap();

        let current_process_id = std::process::id() as usize;

        // It's not equal to 1 because sometime we "catch" `rusty_fork_test` sub procs.
        assert!(pids.len() >= 1);

        assert!(pids.contains(&current_process_id));
    }

    #[test]
    fn query_proc_id_fails_on_a_process_handle() {
        let process = Job {
            handle: HANDLE(get_current_process() as _),
        };

        assert!(matches!(
            process.query_process_id_list(),
            Err(JobError::GetInfoFailed(_))
        ));
    }

    rusty_fork_test! {
        #[test]
        fn query_proc_id_past_initial_capacity() {
            let job =
                Job::create_with_limit_info(ExtendedLimitInfo::new().limit_kill_on_job_close())
                    .unwrap();
            let program = std::env::current_exe().unwrap();

            let children: Vec<Child> = (0..2 * INITIAL_PROCESS_ID_CAPACITY)
                .map(|_| {
                    let mut child = Command::new(&program)
                        .creation_flags(CREATE_SUSPENDED.0)
                        .spawn()
                        .unwrap();
                    job.assign_process(child.as_raw_handle() as isize)
                        .inspect_err(|_| child.kill().unwrap())
                        .unwrap();
                    child
                })
                .collect();

            let mut pids = job.query_process_id_list().unwrap();
            assert_eq!(pids.capacity(), pids.len());
            pids.sort_unstable();

            let mut child_ids: Vec<usize> =
                children.iter().map(|child| child.id() as usize).collect();
            child_ids.sort_unstable();

            assert_eq!(pids, child_ids);
        }

        #[test]
        fn query_basic_and_io_accounting() {
            let job = Job::create().unwrap();

            let info = job.query_basic_and_io_accounting_info().unwrap();
            assert_eq!(info.total_processes, 0);
            assert_eq!(info.total_user_time, Duration::ZERO);
            assert_eq!(info.read_transfer_count, 0);

            job.assign_current_process().unwrap();

            let executable = std::fs::read(std::env::current_exe().unwrap()).unwrap();

            let info = job.query_basic_and_io_accounting_info().unwrap();
            assert_eq!(info.total_processes, 1);
            assert_eq!(info.active_processes, 1);
            assert_eq!(info.total_terminated_processes, 0);
            assert!(info.read_transfer_count >= executable.len() as u64);
        }
    }

    #[test]
    fn accounting_info_maps_every_member() {
        let info = accounting_info(&JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION {
            BasicInfo: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION {
                TotalUserTime: 10_000_000,
                TotalKernelTime: 20_000_000,
                ThisPeriodTotalUserTime: 30_000_000,
                ThisPeriodTotalKernelTime: 40_000_000,
                TotalPageFaultCount: 5,
                TotalProcesses: 6,
                ActiveProcesses: 7,
                TotalTerminatedProcesses: 8,
            },
            IoInfo: IO_COUNTERS {
                ReadOperationCount: 9,
                WriteOperationCount: 10,
                OtherOperationCount: 11,
                ReadTransferCount: 12,
                WriteTransferCount: 13,
                OtherTransferCount: 14,
            },
        });

        assert_eq!(info.total_user_time, Duration::from_secs(1));
        assert_eq!(info.total_kernel_time, Duration::from_secs(2));
        assert_eq!(info.this_period_total_user_time, Duration::from_secs(3));
        assert_eq!(info.this_period_total_kernel_time, Duration::from_secs(4));
        assert_eq!(info.total_page_fault_count, 5);
        assert_eq!(info.total_processes, 6);
        assert_eq!(info.active_processes, 7);
        assert_eq!(info.total_terminated_processes, 8);
        assert_eq!(info.read_operation_count, 9);
        assert_eq!(info.write_operation_count, 10);
        assert_eq!(info.other_operation_count, 11);
        assert_eq!(info.read_transfer_count, 12);
        assert_eq!(info.write_transfer_count, 13);
        assert_eq!(info.other_transfer_count, 14);
    }

    #[test]
    fn ticks_to_duration() {
        assert_eq!(duration_from_ticks(0), Duration::ZERO);
        assert_eq!(duration_from_ticks(1), Duration::from_nanos(100));
        assert_eq!(
            duration_from_ticks(9_999_999),
            Duration::from_nanos(999_999_900)
        );
        assert_eq!(duration_from_ticks(10_000_000), Duration::from_secs(1));
        assert_eq!(
            duration_from_ticks(i64::MAX),
            Duration::new(922_337_203_685, 477_580_700)
        );
        assert_eq!(duration_from_ticks(-1), Duration::ZERO);
    }
}
