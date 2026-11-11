//! Own an idle process without treating the test runner or its children as a host.
use crate::Result;
use std::os::unix::process::CommandExt;

// Native file-access reviews sample the shared OS process table. Serialize
// independent fixtures so their launches and exits cannot change each other's
// frozen reviews. Each case still exercises its own concurrent workers.
pub(crate) static FILE_FIXTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(crate) struct IdleTestHost(std::process::Child);
impl IdleTestHost {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self(
            std::process::Command::new("/bin/sleep")
                .arg("300")
                .current_dir("/private/tmp")
                .process_group(0)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?,
        ))
    }
    pub(crate) fn identity(&self) -> Result<crate::process::Identity> {
        Ok(crate::process::identity(self.0.id())?)
    }
}
impl Drop for IdleTestHost {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
