//! Windows Job Object with KILL_ON_JOB_CLOSE: child processes die with the hub, even on a crash
//! (the OS closes our job handle when the process ends, which kills everything in the job).

#[cfg(windows)]
pub fn assign(child: &tokio::process::Child) {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;

    // Handle kept for the lifetime of the process (stored as usize: HANDLE is not Send/Sync).
    static JOB: OnceLock<Option<usize>> = OnceLock::new();

    let job = JOB.get_or_init(|| unsafe {
        let h = match CreateJobObjectW(None, None) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("CreateJobObject failed: {e}");
                return None;
            }
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let r = SetInformationJobObject(
            h,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if let Err(e) = r {
            tracing::error!("SetInformationJobObject failed: {e}");
            return None;
        }
        Some(h.0 as usize)
    });
    let (Some(job), Some(proc)) = (job, child.raw_handle()) else {
        tracing::warn!("child not attached to job object; it may outlive the hub if the hub crashes");
        return;
    };
    if let Err(e) = unsafe { AssignProcessToJobObject(HANDLE(*job as *mut _), HANDLE(proc as *mut _)) } {
        tracing::warn!("AssignProcessToJobObject failed: {e}");
    }
}

#[cfg(not(windows))]
pub fn assign(_child: &tokio::process::Child) {}
