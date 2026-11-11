//! Fixed Git executable discovery. Job coordinators own execution policies.
use crate::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Clone)]
pub struct GitTool {
    pub(crate) binary: PathBuf,
    pub(crate) exec_dir: PathBuf,
}

impl GitTool {
    pub fn binary(&self) -> &Path {
        &self.binary
    }
    pub fn exec_dir(&self) -> &Path {
        &self.exec_dir
    }
    pub async fn system() -> Result<Self> {
        let output = Command::new("/usr/bin/xcrun")
            .args(["--find", "git"])
            .output()
            .await?;
        if !output.status.success() {
            return Err(std::io::Error::other("Xcode Git is unavailable").into());
        }
        Self::new(PathBuf::from(String::from_utf8(output.stdout)?.trim())).await
    }
    pub async fn new(binary: PathBuf) -> Result<Self> {
        let binary = binary.canonicalize()?;
        let output = Command::new(&binary)
            .arg("--exec-path")
            .env_remove("GIT_EXEC_PATH")
            .output()
            .await?;
        if !output.status.success() {
            return Err(std::io::Error::other("cannot find Git helper directory").into());
        }
        let exec_dir = PathBuf::from(String::from_utf8(output.stdout)?.trim()).canonicalize()?;
        Ok(Self { binary, exec_dir })
    }
}
