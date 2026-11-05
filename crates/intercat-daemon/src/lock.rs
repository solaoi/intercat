use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};

/// Keep this alive for the entire daemon lifetime, before opening SQLite or hosts.
pub struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    pub fn acquire(directory: &Path) -> io::Result<Option<Self>> {
        fs::create_dir_all(directory)?;
        let metadata = fs::symlink_metadata(directory)?;
        // SAFETY: geteuid has no arguments and returns the current effective uid.
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "daemon directory must be owned by this user and not a symlink",
            ));
        }
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("daemon.lock"))?;
        // SAFETY: the file owns the descriptor and flock does not use pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        Ok(Some(Self { _file: file }))
    }
}
