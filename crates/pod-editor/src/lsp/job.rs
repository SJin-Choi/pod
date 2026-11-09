//! Process-tree ownership for language servers.
//!
//! On Windows each server is assigned to its own Job Object with
//! `KILL_ON_JOB_CLOSE`: when the job handle closes (normal shutdown, crash
//! of Pod, or `terminate`) every process the server started — rustup proxy,
//! the real rust-analyzer, `cargo check`, proc-macro servers, node — dies
//! with it. Elsewhere this falls back to killing the direct child.

use std::process::Child;

#[cfg(windows)]
pub struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: a job handle is a kernel object handle usable from any thread.
#[cfg(windows)]
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
unsafe impl Sync for ProcessTree {}

#[cfg(windows)]
impl ProcessTree {
    /// Creates a kill-on-close job and puts `child` in it. Processes the
    /// child starts afterwards inherit the job.
    pub fn adopt(child: &Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let tree = ProcessTree { job };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(tree)
        }
    }

    /// Kills every process in the tree now.
    pub fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
    }

    /// Number of live processes in the job (diagnostics/tests).
    pub fn active_processes(&self) -> u32 {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let ok = QueryInformationJobObject(
                self.job,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            );
            if ok == 0 { 0 } else { info.ActiveProcesses }
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(not(windows))]
pub struct ProcessTree {
    pid: u32,
}

#[cfg(not(windows))]
impl ProcessTree {
    pub fn adopt(child: &Child) -> std::io::Result<Self> {
        Ok(ProcessTree { pid: child.id() })
    }
    pub fn terminate(&self) {
        let _ = std::process::Command::new("kill").arg("-9").arg(self.pid.to_string()).status();
    }
    pub fn active_processes(&self) -> u32 {
        0
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn closing_the_job_kills_grandchildren() {
        // cmd -> ping (grandchild). Dropping the job must kill both.
        let child = Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = child;
        let tree = ProcessTree::adopt(&child).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(tree.active_processes() >= 2, "cmd + ping in the job");
        drop(tree);
        let t = std::time::Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(t.elapsed().as_secs() < 5, "process tree not killed");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
