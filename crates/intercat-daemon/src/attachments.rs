//! App-owned copy directories. Allocation never reuses pre-existing user paths.
use crate::Result;
use intercat_git::{AttachmentPaths, Repository};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registry {
    device: u64,
    inode: u64,
    base: String,
    roots: Vec<String>,
}
fn sync(file: &File) -> Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn directory(parent: &File, name: &std::ffi::CStr) -> Result<File> {
    // A held parent descriptor prevents a swapped path from redirecting writes.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if file.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err("An attachment directory has a different owner.".into());
    }
    Ok(file)
}
fn mkdir(parent: &File, name: &std::ffi::CStr, exclusive: bool) -> Result<()> {
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        return sync(parent);
    }
    let error = std::io::Error::last_os_error();
    if !exclusive && error.kind() == std::io::ErrorKind::AlreadyExists {
        return Ok(());
    }
    Err(error.into())
}
fn initialize(workspace: &Path, root: &str, create: bool) -> Result<()> {
    let workspace = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(workspace)?;
    if workspace.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err("The attachment workspace has a different owner.".into());
    }
    let parent = if root == ".intercat/attachments" {
        mkdir(&workspace, c".intercat", false)?;
        directory(&workspace, c".intercat")?
    } else {
        workspace
    };
    let name = std::ffi::CString::new(
        root.rsplit('/')
            .next()
            .ok_or("Invalid attachment directory.")?,
    )?;
    if create {
        mkdir(&parent, &name, true)?;
    }
    let folder = directory(&parent, &name)?;
    let fd = unsafe {
        libc::openat(
            folder.as_raw_fd(),
            c".gitignore".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Ok(());
        }
        return Err(error.into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(b"*\n")?;
    sync(&file)?;
    sync(&folder)
}
pub fn reserve(workspace: &Path, base: &str) -> Result<Vec<String>> {
    let workspace = workspace.canonicalize()?;
    let repo = Repository::open(&workspace)?;
    let files = repo.saved_files(base.parse()?)?;
    let paths = AttachmentPaths::from_paths(files.keys().map(String::as_str));
    for _ in 0..10000 {
        let root = paths.directory(&workspace)?;
        match initialize(&workspace, &root, true) {
            Ok(()) => return Ok(vec![root]),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err("Attachment directory allocation did not settle.".into())
}
pub fn ensure(workspace: &Path, base: &str, roots: &[String]) -> Result<()> {
    let repo = Repository::open(workspace)?;
    let files = repo.saved_files(base.parse()?)?;
    AttachmentPaths::from_paths(files.keys().map(String::as_str)).with_roots(roots)?;
    for root in roots {
        let exists = fs::symlink_metadata(workspace.join(root));
        initialize(
            workspace,
            root,
            matches!(exists,Err(ref e) if e.kind()==std::io::ErrorKind::NotFound),
        )?;
    }
    Ok(())
}
pub fn reserve_checkout(control: &Path, workspace: &Path, base: &str) -> Result<Vec<String>> {
    let roots = reserve(workspace, base)?;
    let metadata = fs::symlink_metadata(workspace)?;
    let registry = Registry {
        device: metadata.dev(),
        inode: metadata.ino(),
        base: base.into(),
        roots: roots.clone(),
    };
    let path = control.join("attachment-roots.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(&serde_json::to_vec(&registry)?)?;
    sync(&file)?;
    sync(&File::open(control)?)?;
    // This is an isolated clone's metadata, never the user's common Git dir.
    let mut exclude = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(workspace.join(".git/info/exclude"))?;
    for root in &roots {
        writeln!(exclude, "/{root}/")?;
    }
    sync(&exclude)?;
    Ok(roots)
}
pub fn resume_checkout(control: &Path, workspace: &Path) -> Result<Vec<String>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(control.join("attachment-roots.json"))?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > 16384
    {
        return Err("Invalid attachment directory ownership record.".into());
    }
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    let registry: Registry = serde_json::from_slice(&bytes)?;
    if registry.roots.is_empty() {
        return Err("The attachment directory ownership record is empty.".into());
    }
    let metadata = fs::symlink_metadata(workspace)?;
    if !metadata.is_dir() || metadata.dev() != registry.device || metadata.ino() != registry.inode {
        return Err("The attachment workspace changed.".into());
    }
    ensure(workspace, &registry.base, &registry.roots)?;
    Ok(registry.roots)
}
