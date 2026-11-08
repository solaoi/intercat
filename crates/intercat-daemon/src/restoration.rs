//! Restore only named saved files. No Git filters, hooks, index or HEAD writes.
use crate::{Result, deletion::Snapshot};
use intercat_git::Repository;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
};

fn sync(file: &File) -> Result<()> {
    // SAFETY: file owns this valid descriptor for the duration of the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    sync(&File::open(path)?)
}
fn parents(root: &Path, path: &str) -> Result<()> {
    crate::deletion::validate_parent(root, path)?;
    let mut current = root.to_path_buf();
    for part in Path::new(path)
        .parent()
        .into_iter()
        .flat_map(Path::components)
    {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(info) if info.is_dir() && !info.file_type().is_symlink() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                sync(&File::open(current.parent().unwrap())?)?;
            }
            _ => return Err("A restore parent is not a real directory.".into()),
        }
    }
    Ok(())
}
pub fn apply(root: &Path, snapshot: &Snapshot) -> Result<()> {
    crate::deletion::validate_parent(root, "")?;
    let repo = Repository::open(root)?;
    let files = repo.saved_files(snapshot.sha.parse()?)?;
    let mut writes = Vec::new();
    // Validate all paths and load every source before changing the first file.
    for path in snapshot.paths.iter().chain(&snapshot.absent) {
        crate::deletion::validate_parent(root, path)?;
        if fs::symlink_metadata(root.join(path)).is_ok_and(|m| m.is_dir()) {
            return Err(format!("{path} is now a directory; no files were restored.").into());
        }
    }
    for path in &snapshot.paths {
        let file = files
            .get(path)
            .ok_or("Saved file is missing from the snapshot.")?;
        if ![0o100644, 0o100755, 0o120000].contains(&file.mode) {
            return Err("Unsupported saved file type.".into());
        }
        writes.push((path.clone(), file.mode, repo.saved_bytes(file)?));
    }
    for directory in &snapshot.directories {
        crate::deletion::validate_parent(root, directory)?;
        parents(root, &format!("{directory}/placeholder"))?;
    }
    for (name, mode, bytes) in writes {
        write_file(root, &name, mode, &bytes)?;
    }
    for name in &snapshot.absent {
        crate::deletion::validate_parent(root, name)?;
        let target = root.join(name);
        match fs::remove_file(&target) {
            Ok(()) => sync(&File::open(target.parent().ok_or("Invalid restore path")?)?)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(crate) fn write_file(root: &Path, name: &str, mode: u16, bytes: &[u8]) -> Result<()> {
    parents(root, name)?;
    let target = root.join(name);
    let parent = target.parent().ok_or("Invalid restore path")?;
    let temporary = parent.join(format!(".intercat-restore-{}", uuid::Uuid::now_v7()));
    let result = (|| -> Result<()> {
        if mode == 0o120000 {
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(bytes), &temporary)?;
        } else {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.set_permissions(fs::Permissions::from_mode(if mode == 0o100755 {
                0o755
            } else {
                0o644
            }))?;
            sync(&file)?;
        }
        fs::rename(&temporary, &target)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
