use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::ptr::null;

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::Result;

pub(super) fn prepare(command: &mut Command) -> Result<OwnedHandle> {
    command.creation_flags(CREATE_NO_WINDOW);
    // Null parameters create an unnamed, non-inherited handle owned by Exodys.
    let handle = unsafe { CreateJobObjectW(null(), null()) };
    if handle.is_null() {
        return Err("无法创建 sing-box 进程组。".into());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // The job handle and the initialized structure stay valid throughout the call.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err("无法设置 sing-box 进程组。".into());
    }
    Ok(job)
}

pub(super) fn assign(job: &OwnedHandle, child: &mut Child) -> Result<()> {
    // Both process and job handles are held open by the caller.
    let ok = unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) };
    // A short-lived `sing-box check` can exit before it joins the job.
    if ok == 0 && !matches!(child.try_wait(), Ok(Some(_))) {
        return Err("无法将 sing-box 加入进程组。".into());
    }
    Ok(())
}
