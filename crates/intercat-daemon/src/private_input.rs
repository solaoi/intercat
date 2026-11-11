//! Bounded local inputs are read below an owner-only directory. Resolve every
//! component by descriptor and never follow links, devices or shared hard links.
use crate::Result;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};
fn owned(file: &File, directory: bool) -> Result<()> {
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
    {
        return Err("Unsafe private input.".into());
    }
    Ok(())
}
pub(crate) fn read(root: &Path, parts: &[&str], limit: u64) -> Result<Vec<u8>> {
    if parts.is_empty()
        || parts.iter().any(|part| {
            part.is_empty()
                || matches!(*part, "." | "..")
                || part.contains('/')
                || part.contains('\\')
        })
    {
        return Err("Invalid private input identifier.".into());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    owned(&file, true)?;
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        let name = CString::new(*part)?;
        let fd = unsafe {
            libc::openat(
                file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | if last {
                        libc::O_NONBLOCK
                    } else {
                        libc::O_DIRECTORY
                    },
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file = unsafe { File::from_raw_fd(fd) };
        owned(&file, !last)?;
    }
    if file.metadata()?.len() > limit {
        return Err("Private input exceeds its size limit.".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("Private input grew beyond its size limit.".into());
    }
    Ok(bytes)
}
